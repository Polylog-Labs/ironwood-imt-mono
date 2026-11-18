use anyhow::Result;
use clap::Parser;
use ironwood_indexer::{
    api::{self, AppState},
    config::Config,
    mempool::{self, MempoolSnapshot, ProviderMempool},
    providers::Providers,
    store::Store,
    sync::{self, SyncStatus},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{RwLock, watch};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ironwood_indexer=info")),
        )
        .init();
    let config = Config::parse();
    let providers = Providers::new(&config)?;
    let mempool_providers = providers.independent();
    let store = Store::open(&config.database).await?;
    providers
        .execute("initialize", |_, rpc| {
            let store = &store;
            let config = &config;
            async move { sync::initialize(&rpc, store, config).await }
        })
        .await?;
    let checkpoint = store.snapshot().await?.checkpoint;
    tracing::info!(
        height = checkpoint.height,
        leaves = checkpoint.leaf_count,
        "loaded durable checkpoint"
    );
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    let pending = Arc::new(RwLock::new(MempoolSnapshot::default()));
    let concurrency = usize::from(config.concurrency);
    if config.once {
        sync::sync_with_providers(&providers, &store, concurrency, &status).await?;
        ProviderMempool::new(&mempool_providers)
            .refresh(&mempool_providers, concurrency, &pending)
            .await?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"chain":store.snapshot().await?, "mempool":*pending.read().await})
            )?
        );
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let app = api::router(AppState {
        store: store.clone(),
        mempool: pending.clone(),
        sync: status.clone(),
        providers: Some((providers.clone(), mempool_providers.clone())),
        stale_after_seconds: config.poll_seconds.saturating_mul(3).max(60),
    });
    let (shutdown, stop) = watch::channel(false);
    let poll = Duration::from_secs(config.poll_seconds);
    let chain_task = tokio::spawn(sync::run(
        providers,
        store,
        concurrency,
        poll,
        status,
        stop.clone(),
    ));
    let mempool_task = tokio::spawn(mempool::run(
        mempool_providers,
        concurrency,
        poll,
        pending,
        stop,
    ));
    tracing::info!(bind = %config.bind, "read API listening");
    let signal_shutdown = shutdown.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = signal_shutdown.send(true);
        })
        .await;
    let _ = shutdown.send(true);
    chain_task.await?;
    mempool_task.await?;
    server?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
