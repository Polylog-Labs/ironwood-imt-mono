#![allow(dead_code)]
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use clap::Parser;
use incrementalmerkletree::frontier::Frontier;
use ironwood_indexer::{config::Config, model::*, rpc::Rpc, store::Store, tree};
use orchard::tree::MerkleHashOrchard;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

pub fn bytes(n: u64) -> Bytes32 {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    Bytes32(bytes)
}
pub fn baseline() -> Checkpoint {
    Checkpoint {
        height: 9,
        hash: bytes(9),
        time: 100,
        leaf_count: 0,
        root: tree::empty_root(),
    }
}
pub async fn database() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("index.sqlite")).await.unwrap();
    store
        .initialize("regtest".into(), bytes(0), 10, baseline())
        .await
        .unwrap();
    (dir, store)
}
pub fn chain(groups: &[Vec<u64>]) -> Vec<VerifiedBlockInput> {
    let mut frontier = Frontier::<MerkleHashOrchard, 32>::empty();
    let mut previous = bytes(9);
    groups
        .iter()
        .enumerate()
        .map(|(i, values)| {
            let height = 10 + i as u32;
            let actions: Vec<_> = values
                .iter()
                .map(|n| {
                    let cmx = bytes(*n);
                    assert!(frontier.append(tree::decode(cmx).unwrap()));
                    Action {
                        cmx,
                        nullifier: bytes(*n + 100_000),
                    }
                })
                .collect();
            // Distinct branches get distinct hashes even at the same height.
            let hash = bytes(u64::from(height) * 1000 + values.first().copied().unwrap_or(0));
            let block = Block {
                hash,
                height,
                previousblockhash: previous,
                time: 1000 + u64::from(height),
                tx: vec![Transaction {
                    index: None,
                    txid: bytes(u64::from(height) * 10),
                    version: 6,
                    ironwood: Some(Bundle { actions }),
                }],
                trees: Trees {
                    ironwood: Some(TreeSize {
                        size: frontier.tree_size(),
                    }),
                },
            };
            previous = hash;
            VerifiedBlockInput {
                block,
                expected_root: Bytes32(frontier.root().to_bytes()),
            }
        })
        .collect()
}
pub fn transaction_json(tx: &Transaction) -> Value {
    json!({"txid":tx.txid, "version":tx.version,
        "ironwood":tx.ironwood.as_ref().map(|b| json!({"actions":b.actions})),
        "orchard":{"actions":[{"cmx":bytes(999),"nullifier":bytes(998)}]}})
}
pub fn block_json(input: &VerifiedBlockInput) -> Value {
    let b = &input.block;
    json!({"hash":b.hash,"height":b.height,"previousblockhash":b.previousblockhash,"time":b.time,"tx":b.tx.iter().map(transaction_json).collect::<Vec<_>>(),"trees":{"ironwood":{"size":b.trees.ironwood.as_ref().unwrap().size}}})
}

pub struct NodeState {
    pub network: String,
    pub genesis: Bytes32,
    pub chain: Vec<VerifiedBlockInput>,
    pub pool: BTreeMap<Bytes32, Option<Value>>,
    pub calls: BTreeMap<String, usize>,
    pub fail_method: Option<String>,
    pub bad_response_id: bool,
    pub omit_ironwood_size: bool,
    pub auth: Vec<String>,
}
pub struct MockNode {
    pub state: Arc<Mutex<NodeState>>,
    pub rpc: Rpc,
    pub config: Config,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MockNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MockNode {
    pub async fn new(chain: Vec<VerifiedBlockInput>) -> Self {
        let state = Arc::new(Mutex::new(NodeState {
            network: "regtest".into(),
            genesis: bytes(0),
            chain,
            pool: BTreeMap::new(),
            calls: BTreeMap::new(),
            fail_method: None,
            bad_response_id: false,
            omit_ironwood_size: false,
            auth: Vec::new(),
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/", post(handle))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = Config::parse_from(["indexer", "--rpc-url", &url, "--network", "regtest"]);
        let rpc = Rpc::new(&config).unwrap();
        Self {
            state,
            rpc,
            config,
            task,
        }
    }
}
async fn handle(
    State(state): State<Arc<Mutex<NodeState>>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let method = request["method"].as_str().unwrap();
    *state.calls.entry(method.into()).or_default() += 1;
    state.auth.push(
        headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_owned(),
    );
    let id = if state.bad_response_id {
        json!(999999)
    } else {
        request["id"].clone()
    };
    let error = |code, message| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"id":id,"result":null,"error":{"code":code,"message":message}})),
        )
    };
    if state.fail_method.as_deref() == Some(method) {
        return error(-28, "node unavailable");
    }
    let params = &request["params"];
    let tip = state.chain.last();
    let result = match method {
        "getblockchaininfo" => {
            json!({"chain":state.network, "blocks":tip.map_or(9, |b| b.block.height), "bestblockhash":tip.map_or(bytes(9), |b| b.block.hash), "upgrades":{"deadbeef":{"name":"Nu6.3","activationheight":10}}})
        }
        "getblockhash" => {
            let h = params[0].as_u64().unwrap();
            if h == 0 {
                json!(state.genesis)
            } else if h == 9 {
                json!(bytes(h))
            } else if let Some(b) = state.chain.iter().find(|b| u64::from(b.block.height) == h) {
                json!(b.block.hash)
            } else {
                return error(-8, "height out of range");
            }
        }
        "getblock" => {
            let h: u32 = params[0].as_str().unwrap().parse().unwrap();
            if h == 9 {
                json!({"height":9,"hash":bytes(9),"time":100,"trees":{}})
            } else if let Some(b) = state.chain.iter().find(|b| b.block.height == h) {
                let mut value = block_json(b);
                if state.omit_ironwood_size {
                    value["trees"] = json!({});
                }
                value
            } else {
                return error(-8, "height out of range");
            }
        }
        "z_gettreestate" => {
            let hash: Bytes32 = serde_json::from_value(params[0].clone()).unwrap();
            if let Some(b) = state.chain.iter().find(|b| b.block.hash == hash) {
                let mut root = b.expected_root;
                root.0.reverse();
                json!({"hash":hash,"height":b.block.height,"ironwood":{"commitments":{"finalRoot":root,"finalState":"00"}}})
            } else {
                return error(-8, "unknown block");
            }
        }
        "getrawmempool" => json!(state.pool.keys().collect::<Vec<_>>()),
        "getrawtransaction" => {
            let txid: Bytes32 = serde_json::from_value(params[0].clone()).unwrap();
            if let Some(Some(tx)) = state.pool.get(&txid) {
                tx.clone()
            } else {
                return error(-5, "transaction disappeared");
            }
        }
        _ => return error(-32601, "method not found"),
    };
    (
        StatusCode::OK,
        Json(json!({"id":id,"result":result,"error":null})),
    )
}
