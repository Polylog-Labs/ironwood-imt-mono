mod support;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use incrementalmerkletree::{Hashable, Level, frontier::Frontier};
use ironwood_indexer::{
    api::{self, AppState},
    mempool::{Mempool, MempoolSnapshot},
    model::*,
    rpc::Rpc,
    store::{SnapshotGone, Store},
    sync::{self, SyncStatus},
    tree,
};
use orchard::tree::MerkleHashOrchard;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;
use tokio::sync::RwLock;
use tower::ServiceExt;

#[tokio::test]
async fn rpc_sync_resume_and_pool_isolation() {
    let expected = chain(&[vec![1, 2], vec![], vec![3, 4, 5]]);
    let node = MockNode::new(expected.clone()).await;
    let (dir, store) = database().await;
    sync::initialize(&node.rpc, &store, &node.config)
        .await
        .unwrap();
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    sync::sync_once(&node.rpc, &store, 3, &status)
        .await
        .unwrap();
    let checkpoint = store.snapshot().await.unwrap().checkpoint;
    assert_eq!(checkpoint.leaf_count, 5);
    assert_eq!(checkpoint.root, expected[2].expected_root);
    assert_eq!(
        store
            .leaves(0, 100, None)
            .await
            .unwrap()
            .items
            .iter()
            .map(|l| l.cmx)
            .collect::<Vec<_>>(),
        (1..=5).map(bytes).collect::<Vec<_>>()
    );
    let calls = node.state.lock().unwrap().calls["getblock"];
    drop(store);
    let reopened = Store::open(&dir.path().join("index.sqlite")).await.unwrap();
    sync::sync_once(&node.rpc, &reopened, 2, &status)
        .await
        .unwrap();
    assert_eq!(
        node.state.lock().unwrap().calls["getblock"],
        calls,
        "restart must not refetch indexed blocks"
    );
    assert_eq!(reopened.snapshot().await.unwrap().checkpoint, checkpoint);
}

#[tokio::test]
async fn cli_once_bootstraps_an_empty_database_and_emits_json() {
    let node = MockNode::new(chain(&[vec![1, 2]])).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cli.sqlite");
    let url = node.config.rpc_url.clone();
    let database = path.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ironwood-indexer"))
            .args([
                "--rpc-url",
                &url,
                "--network",
                "regtest",
                "--once",
                "--database",
            ])
            .arg(database)
            .env_remove("ZCASH_RPC_USER")
            .env_remove("ZCASH_RPC_PASSWORD")
            .env_remove("ZCASH_RPC_COOKIE")
            .env_remove("IRONWOOD_START_HEIGHT")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["chain"]["checkpoint"]["leaf_count"], 2);
    assert!(result["mempool"]["observed_at"].is_number());
    let store = Store::open(&path).await.unwrap();
    assert_eq!(store.snapshot().await.unwrap().checkpoint.height, 10);
}

#[tokio::test]
async fn root_count_duplicates_and_parent_fail_atomically() {
    let (_dir, store) = database().await;
    let blocks = chain(&[vec![1, 2, 3]]);
    let mut wrong_root = blocks[0].clone();
    wrong_root.expected_root = bytes(1);
    assert!(store.apply(wrong_root).await.is_err());
    assert_eq!(store.snapshot().await.unwrap().checkpoint.leaf_count, 0);
    let mut wrong_count = blocks[0].clone();
    wrong_count.block.trees.ironwood.as_mut().unwrap().size = 4;
    assert!(store.apply(wrong_count).await.is_err());
    let mut duplicate = blocks[0].clone();
    duplicate.block.tx[0].ironwood.as_mut().unwrap().actions[1].nullifier =
        duplicate.block.tx[0].ironwood.as_ref().unwrap().actions[0].nullifier;
    assert!(store.apply(duplicate).await.is_err());
    let mut wrong_parent = blocks[0].clone();
    wrong_parent.block.previousblockhash = bytes(123);
    assert!(store.apply(wrong_parent).await.is_err());
    assert!(store.leaves(0, 10, None).await.unwrap().items.is_empty());
    // The same completed subtree keys must be free after all failed transactions.
    store.apply(blocks[0].clone()).await.unwrap();
    assert_eq!(store.snapshot().await.unwrap().checkpoint.leaf_count, 3);
    assert!(store.apply(blocks[0].clone()).await.is_err());
}

#[tokio::test]
async fn reorg_replaces_orphan_nodes_and_invalidates_pinned_snapshots() {
    let old = chain(&[vec![1, 2], vec![3, 4], vec![5]]);
    let replacement = chain(&[vec![1, 2], vec![8], vec![9, 10, 11]]);
    let node = MockNode::new(old.clone()).await;
    let (_dir, store) = database().await;
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    sync::sync_once(&node.rpc, &store, 2, &status)
        .await
        .unwrap();
    node.state.lock().unwrap().chain = replacement.clone();
    sync::sync_once(&node.rpc, &store, 2, &status)
        .await
        .unwrap();
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.checkpoint.root, replacement[2].expected_root);
    assert!(
        store
            .leaves(0, 10, Some(old[2].block.hash))
            .await
            .unwrap_err()
            .is::<SnapshotGone>()
    );
    assert_eq!(
        store
            .leaves(0, 10, Some(old[0].block.hash))
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    assert!(!store.nullifier(bytes(100003)).await.unwrap().1.is_some());
    assert_eq!(
        store.node(32, 0, None).await.unwrap().hash,
        replacement[2].expected_root
    );
    // A shorter node tip also rolls back safely, without reusing orphan positions.
    node.state.lock().unwrap().chain.truncate(1);
    sync::sync_once(&node.rpc, &store, 2, &status)
        .await
        .unwrap();
    assert_eq!(store.snapshot().await.unwrap().checkpoint.leaf_count, 2);
}

#[tokio::test]
async fn historical_roots_match_upstream_frontier_across_power_of_two_boundaries() {
    let (_dir, store) = database().await;
    let groups: Vec<_> = (1..=17).map(|n| vec![n]).collect();
    let blocks = chain(&groups);
    for block in &blocks {
        store.apply(block.clone()).await.unwrap();
    }
    for block in &blocks {
        let node = store.node(32, 0, Some(block.block.hash)).await.unwrap();
        assert_eq!(node.hash, block.expected_root);
        assert_eq!(
            node.occupied,
            block.block.trees.ironwood.as_ref().unwrap().size
        );
    }
    let last = store.node(0, tree::CAPACITY - 1, None).await.unwrap();
    assert_eq!(
        last.hash,
        Bytes32(MerkleHashOrchard::empty_leaf().to_bytes())
    );
    assert_eq!(last.occupied, 0);
    assert!(store.node(0, tree::CAPACITY, None).await.is_err());
    assert!(store.node(33, 0, None).await.is_err());
}

#[tokio::test]
async fn transaction_and_action_order_determine_positions() {
    let (_dir, store) = database().await;
    let mut block = chain(&[vec![1, 2, 3, 4]]).remove(0);
    let second = block.block.tx[0]
        .ironwood
        .as_mut()
        .unwrap()
        .actions
        .split_off(2);
    block.block.tx.push(Transaction {
        index: None,
        txid: bytes(444),
        version: 6,
        ironwood: Some(Bundle { actions: second }),
    });
    store.apply(block).await.unwrap();
    let leaves = store.leaves(0, 10, None).await.unwrap().items;
    assert_eq!(
        leaves
            .iter()
            .map(|l| (l.position, l.tx_index, l.action_index))
            .collect::<Vec<_>>(),
        vec![(0, 0, 0), (1, 0, 1), (2, 1, 0), (3, 1, 1)]
    );
}

#[tokio::test]
async fn mempool_caches_handles_eviction_and_disappearance_without_touching_chain() {
    let blocks = chain(&[vec![1, 2]]);
    let node = MockNode::new(blocks.clone()).await;
    let pending = Arc::new(RwLock::new(MempoolSnapshot::default()));
    let mut cache = Mempool::default();
    let tx = &blocks[0].block.tx[0];
    node.state
        .lock()
        .unwrap()
        .pool
        .insert(tx.txid, Some(transaction_json(tx)));
    cache.refresh(&node.rpc, 2, &pending).await.unwrap();
    assert_eq!(pending.read().await.actions.len(), 2);
    cache.refresh(&node.rpc, 2, &pending).await.unwrap();
    assert_eq!(node.state.lock().unwrap().calls["getrawtransaction"], 1);
    node.state.lock().unwrap().pool.insert(bytes(777), None);
    cache.refresh(&node.rpc, 2, &pending).await.unwrap();
    assert_eq!(pending.read().await.disappeared_during_sample, 1);
    node.state.lock().unwrap().pool.clear();
    cache.refresh(&node.rpc, 2, &pending).await.unwrap();
    assert!(pending.read().await.actions.is_empty());
    let sequence = pending.read().await.sequence;
    node.state.lock().unwrap().fail_method = Some("getrawmempool".into());
    assert!(cache.refresh(&node.rpc, 2, &pending).await.is_err());
    assert_eq!(pending.read().await.sequence, sequence);
}

#[tokio::test]
async fn missing_ironwood_capability_and_wrong_network_fail_closed() {
    let mut node = MockNode::new(chain(&[vec![1]])).await;
    let (_dir, store) = database().await;
    node.state.lock().unwrap().omit_ironwood_size = true;
    assert!(node.rpc.block(10).await.is_err());
    node.config.network = "main".into();
    assert!(
        sync::initialize(&node.rpc, &store, &node.config)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rpc_rejects_mismatched_ids_and_rereads_cookie() {
    let mut node = MockNode::new(chain(&[vec![1]])).await;
    let dir = tempfile::tempdir().unwrap();
    let cookie = dir.path().join("cookie");
    std::fs::write(&cookie, "u:p").unwrap();
    node.config.rpc_cookie = Some(cookie.clone());
    let rpc = Rpc::new(&node.config).unwrap();
    rpc.chain_info().await.unwrap();
    std::fs::write(&cookie, "u:q").unwrap();
    rpc.chain_info().await.unwrap();
    assert_eq!(
        node.state.lock().unwrap().auth,
        vec!["Basic dTpw", "Basic dTpx"]
    );
    node.state.lock().unwrap().bad_response_id = true;
    assert!(rpc.chain_info().await.is_err());
}

#[test]
fn malformed_bundles_and_future_transaction_versions_are_not_silently_empty() {
    assert!(
        serde_json::from_value::<Transaction>(json!({"txid":bytes(1),"version":6,"ironwood":{}}))
            .is_err()
    );
    assert!(serde_json::from_value::<Transaction>(json!({"txid":bytes(1),"version":6,"ironwood":{"actions":[{"nullifier":bytes(2),"cmx":"bad"}]}})).is_err());
    let tx: Transaction = serde_json::from_value(json!({"txid":bytes(1),"version":7})).unwrap();
    assert!(tx.actions().is_err());
    assert!(tree::decode(Bytes32([255; 32])).is_err());
}

async fn request(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn embedded_ui_serves_compiled_modules_without_a_node_runtime() {
    let (_dir, store) = database().await;
    let app = api::router(AppState {
        store,
        mempool: Arc::new(RwLock::new(MempoolSnapshot::default())),
        sync: Arc::new(RwLock::new(SyncStatus::default())),
        providers: None,
        stale_after_seconds: 60,
    });
    for (uri, expected) in [
        ("/activity.js", "from \"./dom.js\""),
        ("/navigation.js", "import(\"./activity.js\")"),
        ("/dashboard.js", "from \"./tree.js\""),
        ("/tree.js", "from \"./dom.js\""),
        ("/tree-model.js", "export const DEPTH = 32"),
        ("/tree-model.mjs", "export const DEPTH = 32"),
        ("/dom.js", "document.getElementById"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            response.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let script = std::str::from_utf8(&body).unwrap();
        assert!(script.contains(expected), "{uri}");
        assert!(!script.contains(".ts\""), "uncompiled import in {uri}");
    }
    for uri in ["/", "/ironwood", "/ironwood/"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            !std::str::from_utf8(&body)
                .unwrap()
                .contains("/_vercel/insights/"),
            "local pages should not include Vercel analytics"
        );
    }
}

#[tokio::test]
async fn api_pagination_history_bounds_readiness_and_reorg_conflicts() {
    let (_dir, store) = database().await;
    let blocks = chain(&[vec![1, 2], vec![3]]);
    for b in &blocks {
        store.apply(b.clone()).await.unwrap();
    }
    let pending = Arc::new(RwLock::new(MempoolSnapshot::default()));
    let sync = Arc::new(RwLock::new(SyncStatus::default()));
    let app = api::router(AppState {
        store: store.clone(),
        mempool: pending.clone(),
        sync: sync.clone(),
        providers: None,
        stale_after_seconds: 60,
    });
    assert_eq!(
        request(app.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(request(app.clone(), "/healthz").await.0, StatusCode::OK);
    let (code, page) = request(app.clone(), "/v1/leaves?limit=1").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(page["next"], 1);
    let (_, page) = request(
        app.clone(),
        &format!("/v1/leaves?from=1&limit=10&at={}", blocks[0].block.hash),
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    for uri in [
        "/v1/leaves?limit=0",
        "/v1/leaves?limit=1001",
        "/v1/tree/nodes/33/0",
        "/v1/tree/nodes/0/4294967296",
        "/v1/nullifiers/bad",
    ] {
        assert_eq!(
            request(app.clone(), uri).await.0,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    *sync.write().await = SyncStatus {
        node_height: Some(11),
        node_hash: Some(blocks[1].block.hash),
        last_success: Some(unix_time()),
        failed: false,
    };
    pending.write().await.observed_at = Some(unix_time());
    assert_eq!(request(app.clone(), "/readyz").await.0, StatusCode::OK);
    assert_eq!(
        request(app.clone(), "/v1/mempool?sequence=9").await.0,
        StatusCode::CONFLICT
    );
    store.rollback(10).await.unwrap();
    assert_eq!(
        request(
            app.clone(),
            &format!("/v1/leaves?at={}", blocks[1].block.hash)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(app.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn database_rejects_identity_changes() {
    let (_dir, store) = database().await;
    assert!(
        store
            .initialize("main".into(), bytes(0), 10, baseline())
            .await
            .is_err()
    );
    assert!(
        store
            .initialize("regtest".into(), bytes(123), 10, baseline())
            .await
            .is_err()
    );
    assert!(
        store
            .initialize("regtest".into(), bytes(0), 11, baseline())
            .await
            .is_err()
    );
    assert!(store.rollback(0).await.is_err());
}

#[tokio::test]
async fn published_orchard_vectors_match_stored_subtrees() {
    let vectors: Value =
        serde_json::from_str(include_str!("fixtures/merkle_vectors.json")).unwrap();
    let leaves: Vec<Bytes32> = serde_json::from_value(vectors["leaves"].clone()).unwrap();
    let roots: Vec<Bytes32> = serde_json::from_value(vectors["roots"].clone()).unwrap();
    let (_dir, store) = database().await;
    let mut frontier = Frontier::<MerkleHashOrchard, 32>::empty();
    let mut previous = baseline().hash;
    for (i, cmx) in leaves.iter().enumerate() {
        assert!(frontier.append(tree::decode(*cmx).unwrap()));
        let hash = bytes(1000 + i as u64);
        let input = VerifiedBlockInput {
            block: Block {
                hash,
                height: 10 + i as u32,
                previousblockhash: previous,
                time: 0,
                tx: vec![Transaction {
                    index: None,
                    txid: hash,
                    version: 6,
                    ironwood: Some(Bundle {
                        actions: vec![Action {
                            cmx: *cmx,
                            nullifier: bytes(i as u64 + 1),
                        }],
                    }),
                }],
                trees: Trees {
                    ironwood: Some(TreeSize { size: i as u64 + 1 }),
                },
            },
            expected_root: Bytes32(frontier.root().to_bytes()),
        };
        store.apply(input).await.unwrap();
        assert_eq!(store.node(4, 0, None).await.unwrap().hash, roots[i]);
        previous = hash;
    }
    assert_eq!(
        store.node(5, 1, None).await.unwrap().hash,
        Bytes32(MerkleHashOrchard::empty_root(Level::from(5)).to_bytes())
    );
}

#[tokio::test]
async fn a_later_batch_failure_rolls_back_earlier_blocks_and_subtrees() {
    let (_dir, store) = database().await;
    let blocks = chain(&[vec![1, 2], vec![3, 4, 5]]);
    let mut corrupt = blocks.clone();
    corrupt[1].expected_root = bytes(9);
    assert!(store.apply_batch(corrupt).await.is_err());
    assert_eq!(store.snapshot().await.unwrap().checkpoint, baseline());
    assert!(store.leaves(0, 100, None).await.unwrap().items.is_empty());
    assert!(store.checkpoint(10).await.unwrap().is_none());
    let checkpoint = store.apply_batch(blocks.clone()).await.unwrap();
    assert_eq!(checkpoint.root, blocks[1].expected_root);
    assert_eq!(checkpoint.leaf_count, 5);
    assert_eq!(
        store.checkpoint(10).await.unwrap().unwrap().root,
        blocks[0].expected_root
    );
}

#[tokio::test]
async fn dashboard_uses_a_rolling_cutoff_and_keeps_root_and_latest_leaves_consistent() {
    let (_dir, store) = database().await;
    let now = 100_000;
    let cutoff = now - 86_400;
    let mut blocks = chain(&[vec![1, 2], vec![3, 4], vec![5, 6]]);
    blocks[0].block.time = cutoff - 10;
    blocks[1].block.time = cutoff;
    blocks[2].block.time = now - 5;
    store.apply_batch(blocks.clone()).await.unwrap();
    let dashboard = store.dashboard(now, 2).await.unwrap();
    assert_eq!(dashboard.growth_24h.window_start, cutoff);
    assert_eq!(dashboard.growth_24h.baseline.unwrap().height, 11);
    assert_eq!(dashboard.growth_24h.added, Some(2));
    assert_eq!(dashboard.growth_24h.percent, Some(50.0));
    assert_eq!(dashboard.snapshot.checkpoint.root, blocks[2].expected_root);
    assert_eq!(dashboard.latest_leaves[0].time, blocks[2].block.time);
    assert_eq!(dashboard.used_percent, 6.0 / tree::CAPACITY as f64 * 100.0);
    assert_eq!(
        dashboard
            .latest_leaves
            .iter()
            .map(|l| l.position)
            .collect::<Vec<_>>(),
        vec![5, 4]
    );
    // Moving the wall-clock cutoff by one second changes the baseline; no calendar-day bucket.
    let previous_second = store.dashboard(now - 1, 2).await.unwrap();
    assert_eq!(previous_second.growth_24h.added, Some(4));
    assert_eq!(previous_second.growth_24h.percent, Some(200.0));
    store.rollback(11).await.unwrap();
    let after_rollback = store.dashboard(now - 1, 2).await.unwrap();
    assert_eq!(after_rollback.snapshot.generation, 1);
    assert_eq!(
        after_rollback.snapshot.checkpoint.root,
        blocks[1].expected_root
    );
    assert_eq!(after_rollback.latest_leaves[0].position, 3);
    assert_eq!(after_rollback.growth_24h.added, Some(2));
}

#[tokio::test]
async fn dashboard_handles_zero_missing_and_stale_growth_baselines() {
    let (_dir, store) = database().await;
    let mut blocks = chain(&[vec![1, 2]]);
    blocks[0].block.time = 100_000;
    store.apply_batch(blocks).await.unwrap();
    let zero = store.dashboard(100_000, 12).await.unwrap();
    assert_eq!(zero.growth_24h.added, Some(2));
    assert_eq!(zero.growth_24h.percent, None);
    let missing = store.dashboard(86_450, 12).await.unwrap();
    assert!(missing.growth_24h.baseline.is_none());
    assert!(missing.growth_24h.added.is_none());
    assert!(missing.growth_24h.percent.is_none());
    let stale = store.dashboard(200_000, 12).await.unwrap();
    assert!(
        stale.growth_24h.added.is_none(),
        "an old checkpoint must not look like zero growth today"
    );
    assert!(store.dashboard(100_000, 0).await.is_err());
    assert!(store.dashboard(100_000, 101).await.is_err());
}

#[tokio::test]
async fn dashboard_api_distinguishes_an_unsampled_mempool_from_an_empty_one() {
    let (_dir, store) = database().await;
    let block = chain(&[vec![1, 2]]).remove(0);
    store.apply(block.clone()).await.unwrap();
    let pending = Arc::new(RwLock::new(MempoolSnapshot::default()));
    let sync = Arc::new(RwLock::new(SyncStatus {
        node_height: Some(10),
        node_hash: Some(block.block.hash),
        last_success: Some(unix_time()),
        failed: false,
    }));
    let app = api::router(AppState {
        store,
        mempool: pending.clone(),
        sync,
        providers: None,
        stale_after_seconds: 60,
    });
    let (code, value) = request(app.clone(), "/v1/dashboard").await;
    assert_eq!(code, StatusCode::OK);
    assert!(value["mempool"]["actions"].is_null());
    assert_eq!(value["ready"], false);
    assert_eq!(
        value["data"]["snapshot"]["checkpoint"]["root"],
        json!(block.expected_root)
    );
    pending.write().await.observed_at = Some(unix_time());
    let (_, value) = request(app, "/v1/dashboard").await;
    assert_eq!(value["mempool"]["actions"], 0);
    assert_eq!(value["ready"], true);
    assert_eq!(value["data"]["latest_leaves"][0]["position"], 1);
    assert_eq!(value["data"]["latest_leaves"][0]["time"], block.block.time);
}

#[tokio::test]
async fn leaf_dates_follow_pinned_blocks_and_reorgs() {
    let (_dir, store) = database().await;
    let blocks = chain(&[vec![1, 2], vec![3]]);
    store.apply_batch(blocks.clone()).await.unwrap();
    let old_hash = blocks[1].block.hash;
    let historical = store
        .leaves(0, 10, Some(blocks[0].block.hash))
        .await
        .unwrap();
    assert_eq!(historical.items.len(), 2);
    assert!(
        historical
            .items
            .iter()
            .all(|l| l.time == blocks[0].block.time)
    );
    let latest = store.leaves(2, 1, Some(old_hash)).await.unwrap();
    assert_eq!(latest.items[0].time, blocks[1].block.time);
    let (_, leaf) = store.nullifier(latest.items[0].nullifier).await.unwrap();
    assert_eq!(leaf.unwrap().time, blocks[1].block.time);

    store.rollback(10).await.unwrap();
    assert!(
        store
            .leaves(2, 1, Some(old_hash))
            .await
            .unwrap_err()
            .is::<SnapshotGone>()
    );
    let mut replacement = chain(&[vec![1, 2], vec![4]]).remove(1);
    replacement.block.hash = bytes(123456);
    replacement.block.time += 60;
    store.apply(replacement.clone()).await.unwrap();
    let replaced = store.leaves(2, 1, None).await.unwrap();
    assert_eq!(replaced.items[0].time, replacement.block.time);
    assert_eq!(replaced.items[0].cmx, bytes(4));
}

#[tokio::test]
async fn activity_counts_transactions_once_and_reorgs_remove_enrichment() {
    use ironwood_indexer::{activity::Flow, usage::BlockUsage};
    let (dir, store) = database().await;
    let mut blocks = chain(&[vec![1, 2, 3], vec![4], vec![5], vec![6]]);
    for (i, b) in blocks.iter_mut().enumerate() {
        b.block.time = 86_400 * (10 + i as u64) + 3600;
        b.block.tx[0].index = Some(if i == 0 { 0 } else { 1 });
    }
    let pending_id = blocks[1].block.tx[0].txid;
    store
        .observe_pending(unix_time() - 20, vec![pending_id])
        .await
        .unwrap();
    store.apply_batch(blocks.clone()).await.unwrap();
    let missing = store.missing_flows().await.unwrap();
    assert_eq!(missing.len(), 3);
    let day = 86_400;
    assert_eq!(
        store
            .missing_daily_flows(13 * day, 128)
            .await
            .unwrap()
            .len(),
        1
    );
    let incomplete = store.daily_flows(13 * day).await.unwrap().unwrap();
    assert_eq!((incomplete.checked, incomplete.transactions), (0, 1));
    // The middle transaction only pays a fee. Its balance still needs to be decoded.
    let rows = missing
        .iter()
        .zip([(-100, false), (800, true), (30, false)])
        .filter(|(_, (_, only))| !only)
        .map(|(&key, (net_zat, ironwood_only))| {
            (
                key,
                Flow {
                    net_zat,
                    ironwood_only,
                },
            )
        })
        .collect();
    store.save_flows(rows).await.unwrap();
    let b = &blocks[2].block;
    let mut usage = BlockUsage::new(b.height, b.hash);
    usage.count(false, 1, 0, false);
    store.save_usage(vec![usage]).await.unwrap();
    let fee_only = store.daily_flows(12 * day).await.unwrap().unwrap();
    assert_eq!((fee_only.checked, fee_only.transactions), (0, 1));
    assert!(fee_only.median_out_zat.is_none());
    assert_eq!(
        store
            .missing_daily_flows(12 * day, 128)
            .await
            .unwrap()
            .len(),
        1
    );
    store
        .save_flows(vec![(
            missing[1],
            Flow {
                net_zat: 800,
                ironwood_only: true,
            },
        )])
        .await
        .unwrap();
    let fee_only = store.daily_flows(12 * day).await.unwrap().unwrap();
    assert_eq!((fee_only.checked, fee_only.transactions), (1, 1));
    assert_eq!(fee_only.ironwood_only, 1);
    assert_eq!(fee_only.total_out_zat, 800);
    assert_eq!(fee_only.median_out_zat, Some(800.0));
    assert!(
        store
            .missing_daily_flows(12 * day, 128)
            .await
            .unwrap()
            .is_empty()
    );
    let deposit = store.daily_flows(13 * day).await.unwrap().unwrap();
    assert_eq!(deposit.median_in_zat, Some(100.0));
    assert!(deposit.median_out_zat.is_none());
    let withdrawal = store.daily_flows(11 * day).await.unwrap().unwrap();
    assert_eq!(withdrawal.median_out_zat, Some(30.0));
    assert!(withdrawal.median_in_zat.is_none());
    assert!(store.daily_flows(13 * day + 1).await.unwrap().is_none());
    assert_eq!(
        store.next_incomplete_flow_day().await.unwrap(),
        Some(13 * day)
    );
    store.save_daily_flows(13 * day).await.unwrap();
    assert_eq!(
        store.next_incomplete_flow_day().await.unwrap(),
        Some(12 * day)
    );
    let saved = rusqlite::Connection::open(dir.path().join("index.sqlite")).unwrap();
    assert_eq!(
        saved
            .query_row(
                "SELECT median_in_zat FROM daily_pool_flows WHERE day=?1",
                [13 * day],
                |r| r.get::<_, f64>(0)
            )
            .unwrap(),
        100.0
    );
    let mixed = &blocks[1].block;
    let mut usage = BlockUsage::new(mixed.height, mixed.hash);
    usage.count(false, 1, 0, true);
    store.save_usage(vec![usage]).await.unwrap();
    let tip = &blocks[3].block;
    store.save_balance(tip.hash, 9000).await.unwrap();
    let stats = store.activity().await.unwrap();
    let days = stats["days"].as_array().unwrap();
    assert_eq!(days.len(), 4);
    assert_eq!(
        stats["commitments"],
        json!({
            "from_calendar":3,"from_coinbase":3,"from_earlier":0,"leaves":6,"transactions":3,
            "most_common":[1]
        })
    );
    assert_eq!(days[0]["hours"][1], 0);
    assert_eq!(days[1]["hours"][1], 1);
    assert_eq!(days[1]["classified"], true);
    assert_eq!(days[2]["classified"], true);
    assert_eq!(days[2]["internal"][1], 1);
    assert_eq!(stats["flows"]["median_in_zat"], 100.0);
    assert_eq!(stats["flows"]["median_out_zat"], 415.0);
    assert_eq!(stats["first_confirmation"]["count"], 1);
    assert_eq!(stats["balance"]["amount_zat"], 9000);

    store.rollback(blocks[0].block.height).await.unwrap();
    assert!(store.daily_flows(13 * day).await.unwrap().is_none());
    assert_eq!(
        saved
            .query_row(
                "SELECT dirty FROM daily_pool_flows WHERE day=?1",
                [13 * day],
                |r| r.get::<_, u8>(0)
            )
            .unwrap(),
        1
    );
    // Simulate the original request completing after the reorg.
    store
        .save_flows(vec![(
            missing[0],
            Flow {
                net_zat: -100,
                ironwood_only: false,
            },
        )])
        .await
        .unwrap();
    let stats = store.activity().await.unwrap();
    assert_eq!(stats["flows"]["transactions"], 0);
    assert_eq!(stats["flows"]["decoded"], 0);
    assert!(stats["flows"]["median_in_zat"].is_null());
    assert!(stats["balance"].is_null());
    assert_eq!(stats["first_confirmation"]["count"], 0);
    assert_eq!(stats["load"]["blocks"], 0);
    assert_eq!(stats["commitments"]["from_coinbase"], 3);
    assert_eq!(stats["commitments"]["from_calendar"], 0);
    assert_eq!(stats["commitments"]["leaves"], 3);
}

#[tokio::test]
async fn historical_flow_progress_excludes_the_current_utc_day() {
    use ironwood_indexer::activity::Flow;

    let (_dir, store) = database().await;
    let today = unix_time() / 86400 * 86400;
    let mut blocks = chain(&[vec![1], vec![2], vec![3]]);
    for (block, time) in blocks.iter_mut().zip([today - 3600, today - 1800, today]) {
        block.block.time = time;
        block.block.tx[0].index = Some(1);
    }
    store.apply_batch(blocks.clone()).await.unwrap();
    let missing = store.missing_flows().await.unwrap();
    let flow = Flow {
        net_zat: -100,
        ironwood_only: false,
    };
    store
        .save_flows(vec![(missing[0], flow), (missing[2], flow)])
        .await
        .unwrap();
    let app = api::router(AppState {
        store,
        mempool: Arc::new(RwLock::new(MempoolSnapshot::default())),
        sync: Arc::new(RwLock::new(SyncStatus::default())),
        providers: None,
        stale_after_seconds: 60,
    });
    let (status, stats) = request(app, "/v1/activity").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stats["flow_history"],
        json!({"transactions": 2, "decoded": 1})
    );
    assert_eq!(stats["flow_worker"], "idle");
}

#[tokio::test]
async fn all_time_flows_use_individual_transfers_and_follow_reorgs() {
    use ironwood_indexer::activity::Flow;

    let (dir, store) = database().await;
    let mut blocks = chain(&[
        vec![1],
        vec![2],
        vec![3],
        vec![4],
        vec![5],
        vec![6],
        vec![7],
        vec![8],
        vec![9],
    ]);
    for (block, day) in blocks
        .iter_mut()
        .zip([10, 10, 500, 500, 500, 501, 502, 503, 503])
    {
        block.block.time = day * 86400;
        block.block.tx[0].index = Some(1);
    }
    store.apply_batch(blocks.clone()).await.unwrap();
    let rows = store
        .missing_flows()
        .await
        .unwrap()
        .into_iter()
        .rev()
        .zip([
            (-3, false),
            (-5, false),
            (-1000, false),
            (-1001, false),
            (-1002, false),
            (30, false),
            (40, false),
            (800, true),
            (0, false),
        ])
        .map(|(key, (net_zat, ironwood_only))| {
            (
                key,
                Flow {
                    net_zat,
                    ironwood_only,
                },
            )
        })
        .collect();
    store.save_flows(rows).await.unwrap();

    let app = api::router(AppState {
        store: store.clone(),
        mempool: Arc::new(RwLock::new(MempoolSnapshot::default())),
        sync: Arc::new(RwLock::new(SyncStatus::default())),
        providers: None,
        stale_after_seconds: 60,
    });
    let (status, flows) = request(app, "/v1/activity/flows/all-time").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        flows,
        json!({
            "transactions":9,"checked":9,"ironwood_only":1,"cross_pool_changes":7,"in_count":5,"out_count":3,
            "total_in_zat":3011,"total_out_zat":870,
            "median_in_zat":1000.0,"median_out_zat":40.0
        })
    );

    store.rollback(blocks[1].block.height).await.unwrap();
    let flows = store.all_time_flows().await.unwrap();
    assert_eq!(flows.transactions, 2);
    assert_eq!((flows.total_in_zat, flows.total_out_zat), (8, 0));
    assert_eq!(flows.median_in_zat, Some(4.0));
    assert_eq!(flows.median_out_zat, None);

    let day = 10 * 86400;
    store.save_daily_flows(day).await.unwrap();
    let flows = store.daily_flows(day).await.unwrap().unwrap();
    assert_eq!((flows.total_in_zat, flows.total_out_zat), (8, 0));
    assert_eq!(flows.cross_pool_changes, 2);
    drop(store);

    let path = dir.path().join("index.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE daily_pool_flows DROP COLUMN total_in_zat;
             ALTER TABLE daily_pool_flows DROP COLUMN total_out_zat;
             ALTER TABLE daily_pool_flows DROP COLUMN cross_pool_changes;
             UPDATE daily_pool_flows SET median_in_zat = NULL, checked=transactions;
             CREATE TRIGGER dirty_daily_flows_classified AFTER INSERT ON block_usage
             BEGIN SELECT 1; END;
             CREATE TRIGGER dirty_daily_flows_reclassified AFTER UPDATE ON block_usage
             BEGIN SELECT 1; END;
             PRAGMA user_version=5;",
        )
        .unwrap();
    }
    let store = Store::open(&path).await.unwrap();
    let flows = store.daily_flows(day).await.unwrap().unwrap();
    assert_eq!((flows.total_in_zat, flows.total_out_zat), (8, 0));
    assert_eq!(flows.cross_pool_changes, 2);
    assert_eq!(flows.median_in_zat, Some(4.0));
}

#[tokio::test]
async fn activity_migration_derives_historical_counts_from_original_transaction_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("../src/schema.sql"))
            .unwrap();
        conn.execute_batch("INSERT INTO metadata VALUES (1,'main',zeroblob(32),11,0);
            INSERT INTO blocks VALUES(10,randomblob(32),900,0,0,randomblob(32));
            INSERT INTO blocks VALUES(11,randomblob(32),1000,0,4,randomblob(32));
            INSERT INTO leaves VALUES(0,11,randomblob(32),0,0,randomblob(32),randomblob(32));
            INSERT INTO leaves VALUES(1,11,randomblob(32),4,0,randomblob(32),randomblob(32));
            INSERT INTO leaves SELECT 2,height,txid,4,1,randomblob(32),randomblob(32) FROM leaves WHERE position=1;
            INSERT INTO leaves VALUES(3,11,randomblob(32),7,0,randomblob(32),randomblob(32));").unwrap();
    }
    for _ in 0..2 {
        let store = Store::open(&path).await.unwrap();
        let stats = store.activity().await.unwrap();
        assert_eq!(stats["days"][0]["hours"][0], 2);
        assert_eq!(stats["flows"]["transactions"], 2);
        assert_eq!(stats["flows"]["decoded"], 0);
        assert_eq!(store.usage_gap().await.unwrap(), Some((11, 11)));
    }
}

#[tokio::test]
async fn commitment_breakdown_accounts_for_transactions_before_the_calendar() {
    let (_dir, store) = database().await;
    let mut blocks = chain(&[vec![1, 2], vec![3], vec![4, 5, 6], vec![7, 8]]);
    for (block, day, tx_index) in [(0, 10, 1), (1, 11, 0), (2, 499, 1), (3, 500, 1)] {
        blocks[block].block.time = day * 86_400;
        blocks[block].block.tx[0].index = Some(tx_index);
    }
    store.apply_batch(blocks).await.unwrap();
    let stats = store.activity().await.unwrap();
    assert_eq!(stats["days"].as_array().unwrap().len(), 365);
    assert_eq!(
        stats["commitments"],
        json!({"from_calendar":5,"from_coinbase":1,"from_earlier":2,"leaves":8,
            "transactions":2,"most_common":[2,3]})
    );
}

#[tokio::test]
async fn activity_archive_bounds_years_and_keeps_historical_pool_flows_available() {
    let (_dir, store) = database().await;
    let mut blocks = chain(&[vec![1], vec![2], vec![3], vec![4]]);
    let timestamps = [
        "2028-02-29T23:00:00Z",
        "2028-12-31T23:00:00Z",
        "2029-01-01T00:00:00Z",
        "2030-01-01T00:00:00Z",
    ];
    for (block, timestamp) in blocks.iter_mut().zip(timestamps) {
        block.block.time = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp() as u64;
        block.block.tx[0].index = Some(1);
    }
    let leap_day = blocks[0].block.time / 86400 * 86400;
    store.apply_batch(blocks).await.unwrap();
    let archive = store.activity_year(2028).await.unwrap().unwrap();
    assert_eq!(archive.first().unwrap().start, leap_day);
    assert_eq!(archive.len(), 307);
    assert_eq!(archive.first().unwrap().hours[23], 1);
    assert_eq!(archive.last().unwrap().hours[23], 1);
    assert_eq!(store.activity_year(2029).await.unwrap().unwrap().len(), 365);
    assert_eq!(store.activity_year(2030).await.unwrap().unwrap().len(), 1);
    assert!(store.activity_year(2027).await.unwrap().is_none());
    assert!(store.activity_year(2031).await.unwrap().is_none());
    assert_eq!(
        store
            .daily_flows(leap_day)
            .await
            .unwrap()
            .unwrap()
            .transactions,
        1
    );
}

#[tokio::test]
async fn zoned_weekdays_split_utc_hours_at_half_hour_midnight() {
    let (_dir, store) = database().await;
    let mut blocks = chain(&[vec![1], vec![2], vec![3], vec![4], vec![5], vec![6]]);
    for (input, timestamp) in blocks.iter_mut().zip([
        "2026-01-02T00:00:00Z",
        "2026-01-05T18:15:00Z",
        "2026-01-05T18:45:00Z",
        "2026-01-12T18:15:00Z",
        "2026-01-12T18:45:00Z",
        "2026-01-14T06:00:00Z",
    ]) {
        input.block.time = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp() as u64;
        input.block.tx[0].index = Some(1);
    }
    store.apply_batch(blocks).await.unwrap();
    let utc = store.activity().await.unwrap();
    assert_eq!(
        utc["weekdays"][0],
        json!({"days":2,"median":2.0,"peak_hour":18})
    );
    assert_eq!(
        utc["weekdays"][1],
        json!({"days":2,"median":0.0,"peak_hour":null})
    );

    let local = store
        .zoned_weekdays(chrono_tz::Asia::Kolkata)
        .await
        .unwrap();
    assert_eq!((local[0].median, local[0].peak_hour), (Some(1.0), Some(23)));
    assert_eq!((local[1].median, local[1].peak_hour), (Some(1.0), Some(0)));
}

#[tokio::test]
async fn zoned_weekdays_include_both_occurrences_of_a_repeated_hour() {
    let (_dir, store) = database().await;
    let mut blocks = chain(&[vec![1], vec![2], vec![3], vec![4]]);
    for (input, timestamp) in blocks.iter_mut().zip([
        "2026-10-31T12:00:00Z",
        "2026-11-01T05:15:00Z",
        "2026-11-01T06:15:00Z",
        "2026-11-02T06:00:00Z",
    ]) {
        input.block.time = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp() as u64;
        input.block.tx[0].index = Some(1);
    }
    store.apply_batch(blocks).await.unwrap();
    let sunday = store
        .zoned_weekdays(chrono_tz::America::New_York)
        .await
        .unwrap()[6];
    assert_eq!(
        (sunday.days, sunday.median, sunday.peak_hour),
        (1, Some(2.0), Some(1))
    );
}
