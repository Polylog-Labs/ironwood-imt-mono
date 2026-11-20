//! Public bundle composition and shielded action counts, checked against indexed block hashes.
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::watch;

use crate::{
    model::Bytes32,
    providers::Providers,
    rpc::{JsonRpc, Rpc},
    store::Store,
};

pub struct BlockUsage {
    pub height: u32,
    pub hash: Bytes32,
    pub transactions: u64,
    pub ironwood_only: u64,
    pub ironwood_actions: u64,
    pub shielded_actions: u64,
}

#[derive(Deserialize)]
struct Bundle {
    actions: Vec<serde::de::IgnoredAny>,
}
#[derive(Deserialize)]
struct Tx {
    vin: Vec<serde::de::IgnoredAny>,
    vout: Vec<serde::de::IgnoredAny>,
    #[serde(rename = "vShieldedSpend", default)]
    spends: Vec<serde::de::IgnoredAny>,
    #[serde(rename = "vShieldedOutput", default)]
    outputs: Vec<serde::de::IgnoredAny>,
    orchard: Option<Bundle>,
    ironwood: Option<Bundle>,
}
#[derive(Deserialize)]
struct Block {
    height: u32,
    hash: Bytes32,
    tx: Vec<Tx>,
}

async fn json_block(rpc: &JsonRpc, height: u32) -> Result<BlockUsage> {
    let block: Block = rpc
        .call("getblock", serde_json::json!([height.to_string(), 2]))
        .await?;
    ensure!(block.height == height, "unexpected block height");
    let mut usage = BlockUsage::new(height, block.hash);
    for (index, tx) in block.tx.iter().enumerate() {
        usage.count(
            index == 0,
            tx.ironwood.as_ref().map_or(0, |b| b.actions.len()),
            tx.orchard.as_ref().map_or(0, |b| b.actions.len()) + tx.spends.len() + tx.outputs.len(),
            !tx.vin.is_empty() || !tx.vout.is_empty(),
        );
    }
    Ok(usage)
}
impl BlockUsage {
    pub fn new(height: u32, hash: Bytes32) -> Self {
        Self {
            height,
            hash,
            transactions: 0,
            ironwood_only: 0,
            ironwood_actions: 0,
            shielded_actions: 0,
        }
    }
    pub fn count(
        &mut self,
        coinbase: bool,
        ironwood: usize,
        other_shielded: usize,
        transparent: bool,
    ) {
        self.ironwood_actions += ironwood as u64;
        self.shielded_actions += (ironwood + other_shielded) as u64;
        if ironwood > 0 && !coinbase {
            self.transactions += 1;
            self.ironwood_only += u64::from(other_shielded == 0 && !transparent);
        }
    }
}
pub async fn backfill_batch(
    providers: &Providers,
    store: &Store,
    concurrency: usize,
) -> Result<bool> {
    ensure!(
        (1..=32).contains(&concurrency),
        "invalid activity concurrency"
    );
    let Some((min, max)) = store.usage_gap().await? else {
        return Ok(false);
    };
    providers
        .execute("pool activity", |_, rpc| async move {
            let rows = match rpc {
                Rpc::Lightwalletd(client) => {
                    let chunk = u32::from(client.batch_size());
                    let start = min.max(max.saturating_sub(chunk * concurrency as u32 - 1));
                    let ranges = (start..=max).step_by(chunk as usize);
                    let batches = stream::iter(ranges)
                        .map(|height| {
                            client.block_usage(height, max.min(height.saturating_add(chunk - 1)))
                        })
                        .buffer_unordered(concurrency)
                        .try_collect::<Vec<_>>()
                        .await?;
                    Ok::<_, anyhow::Error>(batches.into_iter().flatten().collect::<Vec<_>>())
                }
                Rpc::Json(client) => {
                    stream::iter(min.max(max.saturating_sub(7))..=max)
                        .map(|height| json_block(&client, height))
                        .buffer_unordered(concurrency)
                        .try_collect()
                        .await
                }
            }?;
            ensure!(!rows.is_empty(), "missing block usage");
            store
                .save_usage(rows)
                .await
                .context("cannot store block usage")
        })
        .await?;
    Ok(true)
}

pub async fn run(
    providers: Providers,
    store: Store,
    concurrency: usize,
    poll: Duration,
    mut stop: watch::Receiver<bool>,
) {
    let work = async {
        loop {
            let delay = match backfill_batch(&providers, &store, concurrency).await {
                Ok(true) => Duration::from_millis(100),
                Ok(false) => poll,
                Err(e) => {
                    tracing::warn!(error=%format!("{e:#}"),"pool activity backfill interrupted");
                    poll.max(Duration::from_secs(10))
                }
            };
            tokio::time::sleep(delay).await;
        }
    };
    tokio::select! {_=stop.changed()=>{},_=work=>{}}
}
