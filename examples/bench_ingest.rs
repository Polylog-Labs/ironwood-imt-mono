//! Synthetic local storage/hash benchmark; excludes RPC and fixture generation.
use anyhow::{Result, ensure};
use incrementalmerkletree::frontier::Frontier;
use ironwood_indexer::{model::*, store::Store, tree};
use orchard::tree::MerkleHashOrchard;
use std::time::Instant;

fn scalar(n: u64) -> Bytes32 {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    Bytes32(bytes)
}

#[tokio::main]
async fn main() -> Result<()> {
    let count: u64 = std::env::args().nth(1).map_or(Ok(10_000), |s| s.parse())?;
    ensure!(
        (1..=1_000_000).contains(&count),
        "benchmark count must be 1..=1000000"
    );
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("bench.sqlite");
    let store = Store::open(&path).await?;
    store
        .initialize(
            "regtest".into(),
            scalar(0),
            1,
            Checkpoint {
                height: 0,
                hash: scalar(0),
                time: 0,
                leaf_count: 0,
                root: tree::empty_root(),
            },
        )
        .await?;
    let mut frontier = Frontier::<MerkleHashOrchard, 32>::empty();
    let mut blocks = Vec::new();
    for (i, first) in (1..=count).step_by(100).enumerate() {
        let actions = (first..=(first + 99).min(count))
            .map(|n| {
                frontier.append(tree::decode(scalar(n)).expect("canonical scalar"));
                Action {
                    cmx: scalar(n),
                    nullifier: scalar(n + 1_000_000),
                }
            })
            .collect();
        blocks.push(VerifiedBlockInput {
            block: Block {
                height: i as u32 + 1,
                hash: scalar(i as u64 + 1),
                previousblockhash: scalar(i as u64),
                time: 0,
                trees: Trees {
                    ironwood: Some(TreeSize {
                        size: frontier.tree_size(),
                    }),
                },
                tx: vec![Transaction {
                    index: None,
                    txid: scalar(i as u64 + 1),
                    version: 6,
                    ironwood: Some(Bundle { actions }),
                }],
            },
            expected_root: Bytes32(frontier.root().to_bytes()),
        });
    }
    let block_count = blocks.len();
    let start = Instant::now();
    for block in blocks {
        store.apply(block).await?;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let conn = rusqlite::Connection::open(&path)?;
    let page_count: u64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"actions":count,"blocks":block_count,"seconds":elapsed,"actions_per_second":count as f64/elapsed,"database_bytes":page_count*page_size,"bytes_per_action":(page_count*page_size) as f64/count as f64,"excludes":"RPC, fixture generation, WAL, backups; synthetic 100-action blocks"})
        )?
    );
    Ok(())
}
