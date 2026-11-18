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
