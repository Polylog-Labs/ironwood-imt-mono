//! Enrich recent, already verified transactions independently of tree ingestion.
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};
use tokio::sync::watch;

use crate::{
    config::Config,
    model::Bytes32,
    providers::Providers,
    rpc::{JsonRpc, Rpc},
    store::Store,
};

// A fixed transaction window keeps initial enrichment bounded on free public endpoints.
pub const FLOW_WINDOW: usize = 1000;
#[derive(Clone, Copy)]
pub struct FlowKey {
    pub position: u64,
    pub height: u32,
    pub txid: Bytes32,
}
pub struct CandidateBlock {
    pub height: u32,
    pub hash: Bytes32,
    pub other_pool_txids: HashSet<Bytes32>,
}

#[derive(Clone, Copy)]
pub struct Flow {
    pub net_zat: i64,
    pub ironwood_only: bool,
}

fn decode_flow(bytes: &[u8], expected: Bytes32) -> Result<Flow> {
    let mut data = bytes;
    // v6 reads its branch ID from the transaction header.
    let tx = zcash_primitives::transaction::Transaction::read(
        &mut data,
        zcash_protocol::consensus::BranchId::Nu6_3,
    )
    .map_err(|err| anyhow::anyhow!("invalid full transaction: {err}"))?;
    let mut id = *tx.txid().as_ref();
    id.reverse();
    ensure!(
        data.is_empty() && Bytes32(id) == expected,
        "transaction ID or length mismatch"
    );
    let bundle = tx.ironwood_bundle().context("missing Ironwood bundle")?;
    Ok(Flow {
        net_zat: i64::from(*bundle.value_balance()),
        ironwood_only: tx.transparent_bundle().is_none()
            && tx.sapling_bundle().is_none()
            && tx.orchard_bundle().is_none(),
    })
}

async fn block_flows(
    node: &JsonRpc,
    height: u32,
    keys: Vec<FlowKey>,
) -> Result<Vec<(FlowKey, Flow)>> {
    #[derive(Deserialize)]
    struct RawTx {
        txid: Bytes32,
        hex: String,
    }
    #[derive(Deserialize)]
    struct VerboseBlock {
        height: u32,
        tx: Vec<RawTx>,
    }
    let block: VerboseBlock = node
        .call("getblock", serde_json::json!([height.to_string(), 2]))
        .await?;
    ensure!(block.height == height, "flow block height mismatch");
    let transactions: HashMap<_, _> = block.tx.into_iter().map(|tx| (tx.txid, tx.hex)).collect();
    keys.into_iter()
        .map(|key| {
            let hex = transactions
                .get(&key.txid)
                .context("transaction missing from block")?;
            Ok((key, decode_flow(&hex::decode(hex)?, key.txid)?))
        })
        .collect()
}

pub async fn fill_flows(providers: &Providers, store: &Store, concurrency: usize) -> Result<()> {
    let missing = store.missing_flows().await?;
    fetch_flows(providers, store, concurrency, &missing).await
}

async fn fetch_flows(
    providers: &Providers,
    store: &Store,
    concurrency: usize,
    missing: &[FlowKey],
) -> Result<()> {
    for batch in missing.chunks(128) {
        let rows = providers
            .execute("transaction flows", |_, rpc| async move {
                match rpc {
                    Rpc::Lightwalletd(client) => {
                        stream::iter(batch.iter().copied())
                            .map(|key| {
                                let client = &client;
                                async move {
                                    let (raw, height) = client.raw_transaction(key.txid).await?;
                                    ensure!(
                                        height == u64::from(key.height),
                                        "transaction height mismatch"
                                    );
                                    Ok((key, decode_flow(&raw, key.txid)?))
                                }
                            })
                            .buffer_unordered(concurrency)
                            .try_collect()
                            .await
                    }
                    Rpc::Json(client) => {
                        let mut blocks = BTreeMap::<_, Vec<_>>::new();
                        for &key in batch {
                            blocks.entry(key.height).or_default().push(key);
                        }
                        let rows = stream::iter(blocks)
                            .map(|(height, keys)| block_flows(&client, height, keys))
                            .buffer_unordered(concurrency)
                            .try_collect::<Vec<_>>()
                            .await?;
                        Ok(rows.into_iter().flatten().collect())
                    }
                }
            })
            .await?;
        store.save_flows(rows).await?;
    }
    Ok(())
}

pub fn balance_source(config: &Config) -> Result<Option<JsonRpc>> {
    if config.lightwalletd_url.is_empty() && !config.public_rpc {
        return Ok(Some(JsonRpc::new(config)?));
    }
    let url = config.pool_balance_url.as_deref().or(config
        .public_rpc
        .then_some("https://zcash-mainnet-zebrad.gateway.tatum.io"));
    let Some(url) = url else { return Ok(None) };
    let config = Config {
        rpc_url: url.to_owned(),
        rpc_cookie: None,
        rpc_user: None,
        rpc_password: None,
        ..config.clone()
    };
    Ok(Some(JsonRpc::new(&config)?))
}

pub async fn refresh_balance(node: &JsonRpc, store: &Store) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Block {
        hash: Bytes32,
        value_pools: Vec<Pool>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pool {
        id: String,
        monitored: bool,
        chain_value_zat: Option<u64>,
    }
    let checkpoint = store.snapshot().await?.checkpoint;
    let block: Block = node
        .call("getblock", serde_json::json!([checkpoint.hash, 1]))
        .await?;
    ensure!(block.hash == checkpoint.hash, "pool balance block mismatch");
    let amount = block
        .value_pools
        .iter()
        .find(|p| p.id == "ironwood" && p.monitored)
        .and_then(|p| p.chain_value_zat)
        .context("provider omitted monitored Ironwood total")?;
    store.save_balance(checkpoint.hash, amount).await
}

async fn classify_daily_candidates(
    providers: &Providers,
    store: &Store,
    day: u64,
    concurrency: usize,
) -> Result<()> {
    let Some((first, last)) = store.flow_candidate_span(day).await? else {
        return Ok(());
    };
    providers
        .execute("daily flow candidates", |_, rpc| async move {
            let Rpc::Lightwalletd(client) = rpc else {
                return Ok(());
            };
            let size = u32::from(client.batch_size());
            let ranges = (first..=last).step_by(size as usize);
            let mut blocks = stream::iter(ranges)
                .map(|start| {
                    client.flow_candidates(start, last.min(start.saturating_add(size - 1)))
                })
                .buffer_unordered(concurrency.min(4));
            while let Some(batch) = blocks.next().await {
                store.save_flow_candidates(batch?).await?;
            }
            Ok(())
        })
        .await
}

async fn backfill_day(providers: &Providers, store: &Store, concurrency: usize) -> Result<()> {
    let day = match store.next_flow_day() {
        Some(day) => Some(day),
        None => store.next_incomplete_flow_day().await?,
    };
    let Some(day) = day else { return Ok(()) };
    let mut missing = store.missing_daily_flows(day, 512).await?;
    if !missing.is_empty() {
        if let Err(error) = classify_daily_candidates(providers, store, day, concurrency).await {
            tracing::warn!(day,error=%format!("{error:#}"),"compact flow candidates unavailable");
        }
        missing = store.missing_daily_flows(day, 512).await?;
    }
    let result = fetch_flows(providers, store, concurrency, &missing).await;
    store.save_daily_flows(day).await?;
    result
}

pub async fn run(
    providers: Providers,
    store: Store,
    balance: Option<JsonRpc>,
    concurrency: usize,
    poll: Duration,
    mut stop: watch::Receiver<bool>,
) {
    let mut flows = tokio::time::interval(poll);
    let mut balances = tokio::time::interval(Duration::from_secs(300));
    let mut daily = tokio::time::interval(Duration::from_secs(2));
    daily.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    flows.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    balances.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let work = async {
        loop {
            tokio::select! {
                _ = flows.tick() => if let Err(e) = fill_flows(&providers,&store,concurrency).await {
                    tracing::warn!(error = %format!("{e:#}"), "flow refresh failed");
                },
                _ = daily.tick() => if let Err(error) = backfill_day(&providers,&store,concurrency).await {
                    tracing::warn!(error=%format!("{error:#}"), "daily flow refresh failed");
                },
                _ = balances.tick(), if balance.is_some() => if let Some(node) = &balance
                    && let Err(e) = refresh_balance(node, &store).await {
                        tracing::warn!(error = %format!("{e:#}"), "pool balance refresh failed");
                    },
            }
        }
    };
    tokio::select! { _ = stop.changed() => {}, _ = work => {} }
}

#[derive(Serialize, Default)]
pub struct Day {
    pub start: u64,
    pub hours: [u64; 24],
    pub internal: [u64; 24],
    pub classified: bool,
}
