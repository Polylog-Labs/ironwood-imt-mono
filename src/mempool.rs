use crate::{
    model::*,
    providers::Providers,
    rpc::{Rpc, RpcError},
};
use anyhow::{Result, ensure};
use futures::{StreamExt, stream};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock, watch};

pub(crate) const MAX_MEMPOOL_TRANSACTIONS: usize = 50_000;
pub(crate) const MAX_MEMPOOL_ACTIONS: usize = 250_000;

#[derive(Clone, Debug, Serialize)]
pub struct PendingAction {
    pub txid: Bytes32,
    pub action_index: u32,
    #[serde(flatten)]
    pub action: Action,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct MempoolSnapshot {
    pub sequence: u64,
    pub observed_at: Option<u64>,
    pub sampled_transactions: usize,
    pub disappeared_during_sample: usize,
    pub failed: bool,
    pub actions: Vec<PendingAction>,
}
pub type SharedMempool = Arc<RwLock<MempoolSnapshot>>;

/// Cache even non-Ironwood transactions: a stable mempool costs one RPC per poll.
#[derive(Default)]
pub struct Mempool {
    transactions: BTreeMap<Bytes32, Vec<Action>>,
}

impl Mempool {
    pub async fn refresh(
        &mut self,
        rpc: &Rpc,
        concurrency: usize,
        shared: &SharedMempool,
    ) -> Result<()> {
        ensure!(
            (1..=32).contains(&concurrency),
            "invalid mempool concurrency"
        );
        let (sampled_transactions, disappeared) = if let Rpc::Lightwalletd(client) = rpc {
            self.transactions = client.mempool().await?;
            (self.transactions.len(), 0)
        } else {
            let ids: BTreeSet<_> = rpc.mempool_ids().await?.into_iter().collect();
            ensure!(
                ids.len() <= MAX_MEMPOOL_TRANSACTIONS,
                "mempool exceeds bound of {MAX_MEMPOOL_TRANSACTIONS} transactions"
            );
            self.transactions.retain(|id, _| ids.contains(id));
            let unknown: Vec<_> = ids
                .iter()
                .filter(|id| !self.transactions.contains_key(id))
                .copied()
                .collect();
            let mut fetched = stream::iter(unknown)
                .map(|id| async move { (id, rpc.transaction(id).await) })
                .buffer_unordered(concurrency);
            let mut disappeared = 0;
            let mut action_count: usize = self.transactions.values().map(Vec::len).sum();
            while let Some((id, result)) = fetched.next().await {
                match result {
                    Ok(tx) => {
                        let actions = tx.actions()?.to_vec();
                        for action in &actions {
                            crate::tree::decode(action.cmx)?;
                            crate::tree::decode(action.nullifier)?;
                        }
                        action_count += actions.len();
                        ensure!(
                            action_count <= MAX_MEMPOOL_ACTIONS,
                            "mempool exceeds {MAX_MEMPOOL_ACTIONS} Ironwood actions"
                        );
                        self.transactions.insert(id, actions);
                    }
                    Err(error)
                        if error
                            .downcast_ref::<RpcError>()
                            .is_some_and(|e| e.code == -5) =>
                    {
                        disappeared += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
            (ids.len(), disappeared)
        };
        let actions = self
            .transactions
            .iter()
            .flat_map(|(id, actions)| {
                actions
                    .iter()
                    .enumerate()
                    .map(|(index, action)| PendingAction {
                        txid: *id,
                        action_index: index as u32,
                        action: action.clone(),
                    })
            })
            .collect();
        let mut snapshot = shared.write().await;
        *snapshot = MempoolSnapshot {
            sequence: snapshot.sequence + 1,
            observed_at: Some(unix_time()),
            sampled_transactions,
            disappeared_during_sample: disappeared,
            failed: false,
            actions,
        };
        Ok(())
    }
}

pub async fn run(
    providers: Providers,
    store: crate::store::Store,
    concurrency: usize,
    poll: Duration,
    shared: SharedMempool,
    mut stop: watch::Receiver<bool>,
) {
    let cache = ProviderMempool::new(&providers);
    let mut interval = tokio::time::interval(poll);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retry = poll;
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            _ = interval.tick() => {
                let result = tokio::select! { biased; _ = stop.changed() => break, result = cache.refresh(&providers, concurrency, &shared) => result };
                if let Err(error) = result {
                    shared.write().await.failed = true;
                    tracing::error!(error = %format!("{error:#}"), retry_seconds = retry.as_secs(), "mempool refresh failed; retaining previous sample");
                    interval.reset_after(retry);
                    retry = retry.saturating_mul(2).min(poll.max(Duration::from_secs(300)));
                } else {
                    let sample = shared.read().await;
                    let ids = sample.actions.iter().filter(|a|a.action_index == 0).map(|a|a.txid).collect();
                    let seen = sample.observed_at.unwrap_or_else(unix_time);
                    drop(sample);
                    if let Err(error) = store.observe_pending(seen, ids).await {
                        tracing::warn!(%error,"failed to record mempool observation");
                    }
                    retry = poll;
                    interval.reset_after(poll);
                }
            }
        }
    }
}

/// Caches are provider-local: switching sources never unions their mempool views.
pub struct ProviderMempool {
    caches: Vec<Mutex<Mempool>>,
}

impl ProviderMempool {
    pub fn new(providers: &Providers) -> Self {
        Self {
            caches: (0..providers.len())
                .map(|_| Mutex::new(Mempool::default()))
                .collect(),
        }
    }

    pub async fn refresh(
        &self,
        providers: &Providers,
        concurrency: usize,
        shared: &SharedMempool,
    ) -> Result<()> {
        ensure!(
            self.caches.len() == providers.len(),
            "mempool cache/provider count differs"
        );
        providers
            .execute("mempool", |index, rpc| async move {
                self.caches[index]
                    .lock()
                    .await
                    .refresh(&rpc, concurrency, shared)
                    .await
            })
            .await
    }
}
