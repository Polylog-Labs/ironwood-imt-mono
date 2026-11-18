//! Sticky provider selection with independent chain/mempool health and bounded failover.
use std::{
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::{
    config::Config,
    lightwalletd::Lightwalletd,
    model::{Bytes32, unix_time},
    rpc::Rpc,
};

pub const PUBLIC_MAINNET_PROVIDERS: [&str; 4] = [
    "https://eu.zec.stardust.rest:443",
    "https://zec-node.cakewallet.com:443",
    "https://zec.rocks:443",
    "https://nodo.zcashesp.com:443",
];

struct Provider {
    label: String,
    rpc: Rpc,
    validated: OnceCell<()>,
}

#[derive(Default)]
struct Health {
    failures: u32,
    retry_at: Option<Instant>,
    last_success: Option<u64>,
}
struct State {
    selected: Option<usize>,
    health: Vec<Health>,
}

#[derive(Clone)]
pub struct Providers {
    entries: Arc<Vec<Provider>>,
    network: Arc<str>,
    genesis: Arc<OnceCell<Bytes32>>,
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Serialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub transport: &'static str,
    pub selected: bool,
    pub consecutive_failures: u32,
    pub retry_after_seconds: u64,
    /// Last complete successful operation (a chain catch-up pass or mempool sample).
    pub last_success: Option<u64>,
}

impl Providers {
    pub fn new(config: &Config) -> Result<Self> {
        ensure!(
            !config.public_rpc || config.network == "main",
            "--public-rpc contains mainnet providers only"
        );
        let urls: Vec<_> = if config.public_rpc {
            PUBLIC_MAINNET_PROVIDERS
                .iter()
                .map(|url| (*url).to_owned())
                .collect()
        } else {
            config.lightwalletd_url.clone()
        };
        let mut endpoints = Vec::new();
        if urls.is_empty() {
            // Provider URLs may contain API keys in paths/queries. Only expose the origin.
            let label = reqwest::Url::parse(&config.rpc_url)?
                .origin()
                .ascii_serialization();
            endpoints.push((label, Rpc::new(config)?));
        } else {
            let mut seen = std::collections::HashSet::new();
            for url in urls {
                let normalized = reqwest::Url::parse(&url)?;
                if seen.insert(normalized.to_string()) {
                    endpoints.push((
                        normalized.origin().ascii_serialization(),
                        Rpc::Lightwalletd(Lightwalletd::new(&url, config)?),
                    ));
                }
            }
        }
        Self::from_endpoints(&config.network, endpoints)
    }

    /// Construct an explicit pool. Labels are public metadata; never include credentials.
    pub fn from_endpoints(network: &str, endpoints: Vec<(String, Rpc)>) -> Result<Self> {
        ensure!(
            !endpoints.is_empty() && endpoints.len() <= 16,
            "configure between 1 and 16 providers"
        );
        let entries: Vec<_> = endpoints
            .into_iter()
            .map(|(label, rpc)| Provider {
                label,
                rpc,
                validated: OnceCell::new(),
            })
            .collect();
        let state = State {
            selected: None,
            health: (0..entries.len()).map(|_| Health::default()).collect(),
        };
        Ok(Self {
            entries: Arc::new(entries),
            network: Arc::from(network),
            genesis: Arc::new(OnceCell::new()),
            state: Arc::new(Mutex::new(state)),
        })
    }

    /// Share connections and identity checks, but keep worker health/selection independent.
    pub fn independent(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            network: self.network.clone(),
            genesis: self.genesis.clone(),
            state: Arc::new(Mutex::new(State {
                selected: None,
                health: (0..self.len()).map(|_| Health::default()).collect(),
            })),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn status(&self) -> Vec<ProviderStatus> {
        let state = self.state();
        let now = Instant::now();
        self.entries
            .iter()
            .enumerate()
            .map(|(i, provider)| {
                let health = &state.health[i];
                ProviderStatus {
                    provider: provider.label.clone(),
                    transport: match provider.rpc {
                        Rpc::Json(_) => "json_rpc",
                        Rpc::Lightwalletd(_) => "lightwalletd",
                    },
                    selected: state.selected == Some(i),
                    consecutive_failures: health.failures,
                    retry_after_seconds: health.retry_at.map_or(0, |deadline| {
                        deadline
                            .saturating_duration_since(now)
                            .as_secs()
                            .saturating_add(u64::from(deadline > now))
                    }),
                    last_success: health.last_success,
                }
            })
            .collect()
    }

    async fn validate(&self, provider: &Provider) -> Result<()> {
        provider
            .validated
            .get_or_try_init(|| async {
                let info = provider.rpc.chain_info().await?;
                ensure!(
                    info.chain == self.network.as_ref(),
                    "provider network mismatch"
                );
                let genesis = provider.rpc.block_hash(0).await?;
                let expected = self.genesis.get_or_init(|| async { genesis }).await;
                ensure!(
                    genesis == *expected,
                    "provider genesis differs from the other sources"
                );
                Ok::<_, anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Pin each attempt to one source. The operation must be read-only or checkpointed.
    /// A failed operation is retried on each eligible provider at most once per call.
    pub async fn execute<T, F, Fut>(&self, operation: &str, mut action: F) -> Result<T>
    where
        F: FnMut(usize, Rpc) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let start = self.state().selected.unwrap_or(0);
        let mut last_error = None;
        for offset in 0..self.len() {
            let index = (start + offset) % self.len();
            {
                let mut state = self.state();
                if state.health[index]
                    .retry_at
                    .is_some_and(|deadline| deadline > Instant::now())
                {
                    continue;
                }
                state.selected = Some(index);
            }
            let provider = &self.entries[index];
            let result = match self.validate(provider).await {
                Ok(()) => action(index, provider.rpc.clone()).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(value) => {
                    let mut state = self.state();
                    state.health[index] = Health {
                        last_success: Some(unix_time()),
                        ..Health::default()
                    };
                    return Ok(value);
                }
                Err(error) => {
                    let delay = {
                        let mut state = self.state();
                        let health = &mut state.health[index];
                        health.failures = health.failures.saturating_add(1);
                        let delay = Duration::from_secs(
                            (10u64 << health.failures.saturating_sub(1).min(5)).min(300),
                        );
                        health.retry_at = Some(Instant::now() + delay);
                        state.selected = None;
                        delay
                    };
                    tracing::warn!(operation, provider = %provider.label, retry_seconds = delay.as_secs(), error = %format!("{error:#}"), "provider failed; trying next eligible source");
                    last_error =
                        Some(error.context(format!("{operation} failed via {}", provider.label)));
                }
            }
        }
        match last_error {
            Some(error) => Err(error).context("no configured provider completed the operation"),
            None => bail!("all providers are cooling down; retry later"),
        }
    }
}
