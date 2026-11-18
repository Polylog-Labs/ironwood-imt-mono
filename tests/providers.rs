mod support;

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use clap::Parser;
use ironwood_indexer::{
    api::{self, AppState},
    config::Config,
    mempool::{MempoolSnapshot, ProviderMempool},
    providers::Providers,
    sync::{self, SyncStatus},
};
use std::sync::Arc;
use support::*;
use tokio::sync::RwLock;
use tower::ServiceExt;

fn pool(nodes: &[&MockNode]) -> Providers {
    Providers::from_endpoints(
        "regtest",
        nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (format!("provider-{i}"), n.rpc.clone()))
            .collect(),
    )
    .unwrap()
}

#[tokio::test]
async fn failover_resumes_after_a_partially_committed_pass_without_duplicate_rows() {
    let expected = chain(&[vec![1, 2], vec![3], vec![4, 5]]);
    let mut invalid = expected.clone();
    invalid[1].expected_root = bytes(123);
    let first = MockNode::new(invalid).await;
    let second = MockNode::new(expected.clone()).await;
    let providers = pool(&[&first, &second]);
    let (_dir, store) = database().await;
    let status = Arc::new(RwLock::new(SyncStatus {
        failed: true,
        ..Default::default()
    }));
    sync::sync_with_providers(&providers, &store, 1, &status)
        .await
        .unwrap();
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.checkpoint.root, expected[2].expected_root);
    assert_eq!(snapshot.checkpoint.leaf_count, 5);
    assert_eq!(snapshot.generation, 0);
    assert_eq!(
        second.state.lock().unwrap().calls["getblock"],
        2,
        "fallback should download only the uncommitted suffix"
    );
    assert_eq!(providers.status()[0].consecutive_failures, 1);
    assert!(providers.status()[1].selected);
    assert!(!status.read().await.failed);
    let calls = first.state.lock().unwrap().calls.clone();
    sync::sync_with_providers(&providers, &store, 1, &status)
        .await
        .unwrap();
    assert_eq!(
        first.state.lock().unwrap().calls,
        calls,
        "keep using the healthy source"
    );
}

#[tokio::test]
async fn a_lagging_fallback_cannot_roll_back_the_database() {
    let expected = chain(&[vec![1], vec![2], vec![3]]);
    let lagging = MockNode::new(expected[..1].to_vec()).await;
    let current = MockNode::new(expected.clone()).await;
    let providers = pool(&[&lagging, &current]);
    let (_dir, store) = database().await;
    store.apply_batch(expected.clone()).await.unwrap();
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    sync::sync_with_providers(&providers, &store, 1, &status)
        .await
        .unwrap();
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.generation, 0);
    assert_eq!(snapshot.checkpoint.root, expected[2].expected_root);
    assert_eq!(snapshot.checkpoint.leaf_count, 3);
    assert_eq!(providers.status()[0].consecutive_failures, 1);
    assert!(providers.status()[1].selected);
}

#[tokio::test]
async fn providers_with_a_wrong_network_or_genesis_are_not_used() {
    let unavailable = MockNode::new(chain(&[])).await;
    unavailable.state.lock().unwrap().fail_method = Some("getrawmempool".into());
    let wrong_network = MockNode::new(chain(&[])).await;
    wrong_network.state.lock().unwrap().network = "main".into();
    let wrong_genesis = MockNode::new(chain(&[])).await;
    wrong_genesis.state.lock().unwrap().genesis = bytes(99);
    let healthy = MockNode::new(chain(&[])).await;
    let providers = pool(&[&unavailable, &wrong_network, &wrong_genesis, &healthy]);
    let shared = Arc::new(RwLock::new(MempoolSnapshot::default()));
    ProviderMempool::new(&providers)
        .refresh(&providers, 1, &shared)
        .await
        .unwrap();
    assert!(providers.status()[3].selected);
    assert!(
        !wrong_network
            .state
            .lock()
            .unwrap()
            .calls
            .contains_key("getrawmempool")
    );
    assert!(
        !wrong_genesis
            .state
            .lock()
            .unwrap()
            .calls
            .contains_key("getrawmempool")
    );
    assert!(shared.read().await.observed_at.is_some());
}

#[tokio::test]
async fn mempool_failover_keeps_caches_separate_and_preserves_the_last_complete_sample() {
    let first = MockNode::new(chain(&[vec![1, 2]])).await;
    let second = MockNode::new(chain(&[vec![3]])).await;
    for node in [&first, &second] {
        let tx = node.state.lock().unwrap().chain[0].block.tx[0].clone();
        node.state
            .lock()
            .unwrap()
            .pool
            .insert(tx.txid, Some(transaction_json(&tx)));
    }
    let chain_sources = pool(&[&first, &second]);
    let providers = chain_sources.independent();
    let cache = ProviderMempool::new(&providers);
    let shared = Arc::new(RwLock::new(MempoolSnapshot::default()));
    cache.refresh(&providers, 1, &shared).await.unwrap();
    cache.refresh(&providers, 1, &shared).await.unwrap();
    assert_eq!(first.state.lock().unwrap().calls["getrawtransaction"], 1);
    assert_eq!(shared.read().await.actions.len(), 2);
    first.state.lock().unwrap().fail_method = Some("getrawmempool".into());
    cache.refresh(&providers, 1, &shared).await.unwrap();
    assert_eq!(shared.read().await.actions.len(), 1);
    assert_eq!(shared.read().await.actions[0].action.cmx, bytes(3));
    assert!(
        chain_sources
            .status()
            .iter()
            .all(|s| s.consecutive_failures == 0 && !s.selected)
    );
    second.state.lock().unwrap().fail_method = Some("getrawmempool".into());
    let sequence = shared.read().await.sequence;
    assert!(cache.refresh(&providers, 1, &shared).await.is_err());
    assert_eq!(shared.read().await.sequence, sequence);
    let before = second.state.lock().unwrap().calls.clone();
    assert!(cache.refresh(&providers, 1, &shared).await.is_err());
    assert_eq!(
        second.state.lock().unwrap().calls,
        before,
        "cooldown prevents repeated requests to unavailable sources"
    );
}

#[tokio::test]
async fn public_preset_custom_urls_and_redacted_provider_status() {
    let config = Config::try_parse_from(["indexer", "--public-rpc"]).unwrap();
    let public = Providers::new(&config).unwrap();
    assert_eq!(public.len(), 4);
    assert!(
        public
            .status()
            .iter()
            .all(|p| p.transport == "lightwalletd")
    );
    let config = Config::try_parse_from([
        "indexer",
        "--lightwalletd-url",
        "https://zec.rocks:443,https://nodo.zcashesp.com:443",
        "--lightwalletd-url",
        "https://zec.rocks",
    ])
    .unwrap();
    assert_eq!(Providers::new(&config).unwrap().len(), 2);
    assert!(
        Config::try_parse_from([
            "indexer",
            "--public-rpc",
            "--rpc-url",
            "https://example.com"
        ])
        .is_err()
    );
    assert!(
        Config::try_parse_from([
            "indexer",
            "--lightwalletd-url",
            "https://zec.rocks",
            "--rpc-password",
            "x"
        ])
        .is_err()
    );
    let config = Config::try_parse_from(["indexer", "--public-rpc", "--network", "test"]).unwrap();
    assert!(Providers::new(&config).is_err());
    let config = Config::try_parse_from([
        "indexer",
        "--rpc-url",
        "https://example.com/private-api-key?token=private-token",
    ])
    .unwrap();
    let providers = Providers::new(&config).unwrap();
    let (_dir, store) = database().await;
    let app = api::router(AppState {
        store,
        mempool: Arc::new(RwLock::new(MempoolSnapshot::default())),
        sync: Arc::new(RwLock::new(SyncStatus::default())),
        providers: Some((providers.clone(), providers.independent())),
        stale_after_seconds: 60,
    });
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = String::from_utf8(
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("https://example.com"));
    assert!(!body.contains("private-api-key") && !body.contains("private-token"));
}
