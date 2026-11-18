use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    blocks: Vec<FixtureBlock>,
    end: FixtureEnd,
    root: Bytes32,
}
#[derive(Deserialize)]
struct FixtureBlock {
    height: u64,
    hash: String,
    prev_hash: String,
    time: u32,
    count: u32,
    transactions: Vec<FixtureTx>,
}
#[derive(Deserialize)]
struct FixtureTx {
    index: u64,
    txid: String,
    actions: Vec<Action>,
}
#[derive(Deserialize)]
struct FixtureEnd {
    hash: String,
    #[serde(rename = "ironwoodTree")]
    tree: String,
}

fn fixture() -> (
    Checkpoint,
    Vec<proto::CompactBlock>,
    proto::TreeState,
    Bytes32,
) {
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../tests/fixtures/compact_blocks.json")).unwrap();
    let blocks: Vec<_> = fixture
        .blocks
        .into_iter()
        .map(|b| proto::CompactBlock {
            height: b.height,
            hash: hex::decode(b.hash).unwrap(),
            prev_hash: hex::decode(b.prev_hash).unwrap(),
            time: b.time,
            chain_metadata: Some(proto::ChainMetadata {
                ironwood_commitment_tree_size: b.count,
                ..Default::default()
            }),
            vtx: b
                .transactions
                .into_iter()
                .map(|tx| proto::CompactTx {
                    index: tx.index,
                    txid: hex::decode(tx.txid).unwrap(),
                    ironwood_actions: tx
                        .actions
                        .into_iter()
                        .map(|a| proto::CompactOrchardAction {
                            cmx: a.cmx.0.to_vec(),
                            nullifier: a.nullifier.0.to_vec(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .collect();
    let previous = Checkpoint {
        height: blocks[0].height as u32 - 1,
        hash: display_hash(&blocks[0].prev_hash).unwrap(),
        time: 0,
        leaf_count: 0,
        root: tree::empty_root(),
    };
    let after = proto::TreeState {
        network: "main".into(),
        height: blocks.last().unwrap().height,
        hash: fixture.end.hash,
        ironwood_tree: fixture.end.tree,
        ..Default::default()
    };
    (previous, blocks, after, fixture.root)
}

#[tokio::test]
async fn mainnet_range_verifies_and_preserves_original_transaction_positions() {
    let (previous, blocks, after, expected_root) = fixture();
    let expected_positions: Vec<_> = blocks
        .iter()
        .flat_map(|b| {
            b.vtx.iter().flat_map(move |tx| {
                tx.ironwood_actions
                    .iter()
                    .enumerate()
                    .map(move |(i, _)| (b.height as u32, tx.index as u32, i as u32))
            })
        })
        .collect();
    let verified = verify_range(&previous, Frontier::empty(), blocks, after).unwrap();
    assert_eq!(verified.last().unwrap().expected_root, expected_root);
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("test.sqlite"))
        .await
        .unwrap();
    store
        .initialize(
            "main".into(),
            Bytes32([0; 32]),
            previous.height + 1,
            previous,
        )
        .await
        .unwrap();
    for block in verified {
        store.apply(block).await.unwrap();
    }
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.checkpoint.leaf_count, 97);
    assert_eq!(snapshot.checkpoint.root, expected_root);
    let rows = store.leaves(0, 100, None).await.unwrap().items;
    assert_eq!(
        rows.iter()
            .map(|l| (l.height, l.tx_index, l.action_index))
            .collect::<Vec<_>>(),
        expected_positions
    );
    assert!(rows.iter().any(|l| l.tx_index > 0));
}

#[test]
fn corrupt_or_incomplete_ranges_never_become_verified_inputs() {
    let (previous, blocks, after, _) = fixture();
    let check = |b, a| verify_range(&previous, Frontier::empty(), b, a).is_err();
    let mut missing = blocks.clone();
    missing.remove(1);
    assert!(check(missing, after.clone()));
    let mut reordered = blocks.clone();
    reordered.swap(0, 1);
    assert!(check(reordered, after.clone()));
    let mut wrong_count = blocks.clone();
    wrong_count[0]
        .chain_metadata
        .as_mut()
        .unwrap()
        .ironwood_commitment_tree_size += 1;
    assert!(check(wrong_count, after.clone()));
    let mut modified = blocks.clone();
    let tx = modified
        .iter_mut()
        .flat_map(|b| &mut b.vtx)
        .find(|tx| !tx.ironwood_actions.is_empty())
        .unwrap();
    tx.ironwood_actions[0].cmx = vec![0; 32];
    assert!(check(modified, after.clone()));
    let mut duplicate_index = blocks.clone();
    let b = duplicate_index
        .iter_mut()
        .find(|b| !b.vtx.is_empty())
        .unwrap();
    b.vtx.push(b.vtx[0].clone());
    assert!(check(duplicate_index, after.clone()));
    let mut wrong_anchor = after.clone();
    wrong_anchor.hash = hex::encode([0; 32]);
    assert!(check(blocks.clone(), wrong_anchor));
    let mut truncated_tree = after;
    truncated_tree.ironwood_tree.pop();
    assert!(check(blocks, truncated_tree));
}

#[test]
fn verified_range_can_resume_from_an_existing_frontier() {
    let (previous, blocks, after, expected_root) = fixture();
    let all = verify_range(&previous, Frontier::empty(), blocks.clone(), after.clone()).unwrap();
    let mut frontier = Frontier::empty();
    for b in &all[..5] {
        for tx in &b.block.tx {
            for a in tx.actions().unwrap() {
                assert!(frontier.append(tree::decode(a.cmx).unwrap()));
            }
        }
    }
    let b = &all[4];
    let checkpoint = Checkpoint {
        height: b.block.height,
        hash: b.block.hash,
        time: b.block.time,
        leaf_count: frontier.tree_size(),
        root: b.expected_root,
    };
    let suffix = verify_range(&checkpoint, frontier, blocks[5..].to_vec(), after).unwrap();
    assert_eq!(suffix.len(), 5);
    assert_eq!(suffix.last().unwrap().expected_root, expected_root);
}

#[test]
fn malformed_frontiers_and_noncanonical_fields_return_errors() {
    assert_eq!(decode_frontier("000000").unwrap().tree_size(), 0);
    for input in ["", "02", "000020", "00000000", "00ff00", "00000101", "zz"] {
        assert!(decode_frontier(input).is_err(), "accepted {input}");
    }
    let right_without_left = format!("0001{}00", "00".repeat(32));
    assert!(decode_frontier(&right_without_left).is_err());
    let parents_without_leaf = format!("00000101{}", "00".repeat(32));
    assert!(decode_frontier(&parents_without_leaf).is_err());
    assert!(field(&[0; 31]).is_err());
    assert!(field(&[255; 32]).is_err());
    assert!(display_hash(&[0; 31]).is_err());
}
