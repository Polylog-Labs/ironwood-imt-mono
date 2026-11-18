use crate::{config::Config, model::*, providers::Providers, rpc::Rpc, store::Store};
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, stream};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::{RwLock, watch};

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncStatus {
    pub node_height: Option<u32>,
    pub node_hash: Option<Bytes32>,
    pub last_success: Option<u64>,
    pub failed: bool,
}
pub type SharedSyncStatus = Arc<RwLock<SyncStatus>>;

pub async fn initialize(rpc: &Rpc, store: &Store, config: &Config) -> Result<()> {
    let info = rpc.chain_info().await?;
    ensure!(
        info.chain == config.network,
        "RPC network does not match configured network"
    );
    let start = config.start_height.or_else(|| info.ironwood_activation()).context("RPC does not advertise NU6.3; use an Ironwood-capable node or provide its exact activation height with --start-height")?;
    ensure!(
        start > 0 && start <= info.blocks,
        "Ironwood start height must be positive and already available on the node"
    );
    if let Some(activation) = info.ironwood_activation() {
        ensure!(
            start == activation,
            "start height must equal the node's Ironwood activation height ({activation}) to index the complete tree"
        );
    }
    let (genesis, baseline) = tokio::try_join!(rpc.block_hash(0), rpc.baseline(start - 1))?;
    store.initialize(info.chain, genesis, start, baseline).await
}

/// Find the common ancestor with logarithmically many RPC reads.
/// Mutate only after checking that the sampled node tip remains canonical.
async fn reconcile(rpc: &Rpc, store: &Store, info: &ChainInfo) -> Result<()> {
    let local = store.snapshot().await?;
    let high = local.checkpoint.height.min(info.blocks);
    let baseline = local.start_height - 1;
    ensure!(
        high >= baseline,
        "node tip precedes stored activation baseline"
    );
    let matches = |height| async move {
        let checkpoint = store
            .checkpoint(height)
            .await?
            .context("missing local checkpoint")?;
        Ok::<_, anyhow::Error>(rpc.block_hash(height).await? == checkpoint.hash)
    };
    let common = if matches(high).await? {
        high
    } else {
        ensure!(
            matches(baseline).await?,
            "reorg crosses activation baseline or RPC points at a different chain; use a new database"
        );
        let (mut low, mut upper) = (baseline, high);
        while upper - low > 1 {
            let mid = low + (upper - low) / 2;
            if matches(mid).await? {
                low = mid;
            } else {
                upper = mid;
            }
        }
        low
    };
    if common < local.checkpoint.height {
        ensure!(
            rpc.block_hash(info.blocks).await? == info.bestblockhash && matches(common).await?,
            "chain moved during reorg discovery; retrying"
        );
        tracing::warn!(
            from = local.checkpoint.height,
            to = common,
            "rolling back orphaned blocks"
        );
        store.rollback(common).await?;
    }
    Ok(())
}

pub async fn sync_once(
    rpc: &Rpc,
    store: &Store,
    concurrency: usize,
    status: &SharedSyncStatus,
) -> Result<()> {
    sync_once_inner(rpc, store, concurrency, status, false).await
}

pub async fn sync_with_providers(
    providers: &Providers,
    store: &Store,
    concurrency: usize,
    status: &SharedSyncStatus,
) -> Result<()> {
    providers
        .execute("chain", |_, rpc| async move {
            sync_once_inner(&rpc, store, concurrency, status, providers.len() > 1).await
        })
        .await
}

async fn sync_once_inner(
    rpc: &Rpc,
    store: &Store,
    concurrency: usize,
    status: &SharedSyncStatus,
    reject_lagging: bool,
) -> Result<()> {
    ensure!((1..=32).contains(&concurrency), "invalid RPC concurrency");
    let info = rpc.chain_info().await?;
    let current = store.snapshot().await?;
    ensure!(info.chain == current.network, "RPC network changed");
    ensure!(
        !reject_lagging || info.blocks >= current.checkpoint.height,
        "provider tip {} is behind stored height {}; refusing a rollback caused by provider lag",
        info.blocks,
        current.checkpoint.height
    );
    {
        let mut state = status.write().await;
        state.node_height = Some(info.blocks);
        state.node_hash = Some(info.bestblockhash);
    }
    reconcile(rpc, store, &info).await?;
    status.write().await.failed = false;
    let local = store.snapshot().await?;
    let mut last_report = std::time::Instant::now();
    if let Rpc::Lightwalletd(client) = rpc {
        let mut checkpoint = local.checkpoint;
        while checkpoint.height < info.blocks {
            let end = checkpoint
                .height
                .saturating_add(u32::from(client.batch_size()))
                .min(info.blocks);
            // The whole range is checked against its ending tree state before any writes.
            let blocks = client.blocks(&checkpoint, end).await?;
            checkpoint = store.apply_batch(blocks).await?;
            report_progress(&checkpoint, info.blocks, &mut last_report);
        }
    } else {
        let start = u64::from(local.checkpoint.height) + 1;
        let mut blocks = stream::iter(start..=u64::from(info.blocks))
            .map(|height| rpc.block(height as u32))
            .buffered(concurrency);
        while let Some(block) = blocks.next().await {
            let checkpoint = store.apply(block?).await?;
            report_progress(&checkpoint, info.blocks, &mut last_report);
        }
    }
    ensure!(
        rpc.block_hash(info.blocks).await? == info.bestblockhash,
        "chain moved during catch-up; retrying"
    );
    ensure!(
        store.snapshot().await?.checkpoint.hash == info.bestblockhash,
        "local tip differs from sampled node tip"
    );
    let mut state = status.write().await;
    state.last_success = Some(unix_time());
    state.failed = false;
    Ok(())
}

pub async fn run(
    providers: Providers,
    store: Store,
    concurrency: usize,
    poll: Duration,
    status: SharedSyncStatus,
    mut stop: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(poll);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retry = poll;
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            _ = interval.tick() => {
                let result = tokio::select! { biased; _ = stop.changed() => break, result = sync_with_providers(&providers, &store, concurrency, &status) => result };
                if let Err(error) = result {
                    status.write().await.failed = true;
                    tracing::error!(error = %format!("{error:#}"), retry_seconds = retry.as_secs(), "chain sync failed; checkpoint retained");
                    interval.reset_after(retry);
                    retry = retry.saturating_mul(2).min(poll.max(Duration::from_secs(300)));
                } else {
                    retry = poll;
                    interval.reset_after(poll);
                }
            }
        }
    }
}

fn report_progress(checkpoint: &Checkpoint, target: u32, last_report: &mut std::time::Instant) {
    if last_report.elapsed() >= Duration::from_secs(5) || checkpoint.height == target {
        tracing::info!(
            height = checkpoint.height,
            target,
            leaves = checkpoint.leaf_count,
            "indexed and verified"
        );
        *last_report = std::time::Instant::now();
    }
}
