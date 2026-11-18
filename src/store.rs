use std::path::Path;

use anyhow::{Result, ensure};
use serde::Serialize;
use tokio_rusqlite::{
    Connection,
    rusqlite::{self, OptionalExtension, TransactionBehavior, params},
};

use crate::{model::*, tree};

/// Two dedicated SQLite threads: one writer, one reader. WAL permits reads during sync.
#[derive(Clone)]
pub struct Store {
    writer: Connection,
    reader: Connection,
}

#[derive(Debug, thiserror::Error)]
#[error("snapshot is no longer canonical; restart pagination")]
pub struct SnapshotGone;

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub snapshot: Snapshot,
    pub items: Vec<T>,
    pub next: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct BlockDelta {
    #[serde(flatten)]
    pub checkpoint: Checkpoint,
    pub first_position: u64,
    pub added: u64,
}

#[derive(Debug, Serialize)]
pub struct Node {
    pub snapshot: Snapshot,
    pub level: u8,
    pub index: u64,
    pub hash: Bytes32,
    pub first_position: u64,
    pub capacity: u64,
    pub occupied: u64,
}

fn checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        height: row.get(0)?,
        hash: row.get(1)?,
        time: row.get(2)?,
        leaf_count: row.get(3)?,
        root: row.get(4)?,
    })
}

fn leaf_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Leaf> {
    Ok(Leaf {
        position: row.get(0)?,
        height: row.get(1)?,
        txid: row.get(2)?,
        tx_index: row.get(3)?,
        action_index: row.get(4)?,
        cmx: row.get(5)?,
        nullifier: row.get(6)?,
    })
}

fn tip(conn: &rusqlite::Connection) -> Result<Checkpoint> {
    Ok(conn.query_row(
        "SELECT height, hash, time, leaf_count, root FROM blocks ORDER BY height DESC LIMIT 1",
        [],
        checkpoint_row,
    )?)
}

fn snapshot(conn: &rusqlite::Connection, at: Option<Bytes32>) -> Result<Snapshot> {
    let checkpoint = match at {
        None => tip(conn)?,
        Some(hash) => conn
            .query_row(
                "SELECT height, hash, time, leaf_count, root FROM blocks WHERE hash = ?1",
                [hash],
                checkpoint_row,
            )
            .optional()?
            .ok_or(SnapshotGone)?,
    };
    let (network, start_height, generation) = conn.query_row(
        "SELECT network, start_height, generation FROM metadata WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(Snapshot {
        network,
        start_height,
        generation,
        checkpoint,
    })
}

/// Must run inside the caller's write transaction so every failure is atomic.
fn apply_block(
    conn: &rusqlite::Connection,
    previous: Checkpoint,
    input: VerifiedBlockInput,
) -> Result<Checkpoint> {
    let block = input.block;
    ensure!(
        previous.height.checked_add(1) == Some(block.height)
            && block.previousblockhash == previous.hash,
        "block does not extend local chain"
    );
    let expected_count = block
        .trees
        .ironwood
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing Ironwood tree size"))?
        .size;
    ensure!(
        expected_count <= tree::CAPACITY,
        "RPC tree exceeds capacity"
    );
    conn.execute(
        "INSERT INTO blocks(height, hash, time, first_position, leaf_count, root)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            block.height,
            block.hash,
            block.time,
            previous.leaf_count,
            expected_count,
            input.expected_root
        ],
    )?;
    let mut position = previous.leaf_count;
    for (tx_index, transaction) in block.tx.iter().enumerate() {
        let tx_index = transaction.index.map(u64::from).unwrap_or(tx_index as u64);
        for (action_index, action) in transaction.actions()?.iter().enumerate() {
            tree::decode(action.nullifier)?;
            conn.prepare_cached(
                "INSERT INTO leaves(position, height, txid, tx_index, action_index, cmx, nullifier)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                position,
                block.height,
                transaction.txid,
                tx_index,
                action_index as u64,
                action.cmx,
                action.nullifier
            ])?;
            tree::append(conn, position, action.cmx, block.height)?;
            position += 1;
        }
    }
    ensure!(
        position == expected_count,
        "Ironwood leaf count mismatch at height {}: extracted {position}, node reports {expected_count}",
        block.height
    );
    let root = if position == previous.leaf_count {
        previous.root
    } else {
        Bytes32(tree::node(conn, tree::DEPTH, 0, position)?.to_bytes())
    };
    ensure!(
        root == input.expected_root,
        "Ironwood root mismatch at height {}",
        block.height
    );
    Ok(Checkpoint {
        height: block.height,
        hash: block.hash,
        time: block.time,
        leaf_count: position,
        root,
    })
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        ensure!(
            path != Path::new(":memory:"),
            "use a file database; reader and writer have separate connections"
        );
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        let writer = Connection::open(path).await?;
        writer.call_raw(|conn| -> Result<()> {
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-16384;")?;
            let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(version <= 1, "database schema is newer than this binary");
            if version == 0 {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute_batch(include_str!("schema.sql"))?;
                tx.commit()?;
            }
            Ok(())
        }).await??;
        let reader =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).await?;
        reader
            .call_raw(|conn| -> Result<()> {
                conn.busy_timeout(std::time::Duration::from_secs(5))?;
                conn.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-8192;")?;
                Ok(())
            })
            .await??;
        Ok(Self { writer, reader })
    }

    pub async fn initialize(
        &self,
        network: String,
        genesis: Bytes32,
        start_height: u32,
        baseline: Checkpoint,
    ) -> Result<()> {
        self.writer.call_raw(move |conn| -> Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(String, Bytes32, u32)> = tx.query_row("SELECT network, genesis, start_height FROM metadata", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            if let Some(existing) = existing {
                ensure!(existing == (network, genesis, start_height), "database network, genesis, or start height differs from configuration");
            } else {
                ensure!(baseline.height.checked_add(1) == Some(start_height) && baseline.leaf_count == 0 && baseline.root == tree::empty_root(), "invalid empty baseline");
                tx.execute("INSERT INTO metadata(id, network, genesis, start_height) VALUES (1, ?1, ?2, ?3)", params![network, genesis, start_height])?;
                tx.execute("INSERT INTO blocks(height, hash, time, first_position, leaf_count, root) VALUES (?1, ?2, ?3, 0, 0, ?4)", params![baseline.height, baseline.hash, baseline.time, baseline.root])?;
            }
            tx.commit()?;
            Ok(())
        }).await?
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.reader
            .call_raw(move |conn| {
                let tx = conn.transaction()?;
                f(&tx)
            })
            .await?
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.read(|conn| snapshot(conn, None)).await
    }

    pub async fn checkpoint(&self, height: u32) -> Result<Option<Checkpoint>> {
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT height, hash, time, leaf_count, root FROM blocks WHERE height = ?1",
                    [height],
                    checkpoint_row,
                )
                .optional()?)
        })
        .await
    }

    /// Insert one block in a durable transaction.
    pub async fn apply(&self, input: VerifiedBlockInput) -> Result<Checkpoint> {
        self.apply_batch(vec![input]).await
    }

    /// Commit a verified range with one disk flush, retaining every block checkpoint.
    /// A failure in any block rolls back the complete range.
    pub async fn apply_batch(&self, inputs: Vec<VerifiedBlockInput>) -> Result<Checkpoint> {
        ensure!(
            !inputs.is_empty() && inputs.len() <= 2000,
            "invalid database batch size"
        );
        self.writer
            .call_raw(move |conn| -> Result<Checkpoint> {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut previous = tip(&tx)?;
                for input in inputs {
                    previous = apply_block(&tx, previous, input)?;
                }
                tx.commit()?;
                Ok(previous)
            })
            .await?
    }

    /// Cascades remove orphan actions and every subtree completed by those blocks.
    pub async fn rollback(&self, height: u32) -> Result<()> {
        self.writer
            .call_raw(move |conn| -> Result<()> {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                ensure!(
                    tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM blocks WHERE height = ?1)",
                        [height],
                        |r| r.get::<_, bool>(0)
                    )?,
                    "rollback target not indexed"
                );
                let removed = tx.execute("DELETE FROM blocks WHERE height > ?1", [height])?;
                if removed != 0 {
                    tx.execute(
                        "UPDATE metadata SET generation = generation + 1 WHERE id = 1",
                        [],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?
    }

    pub async fn leaves(&self, from: u64, limit: u32, at: Option<Bytes32>) -> Result<Page<Leaf>> {
        ensure!(
            (1..=1000).contains(&limit) && from <= tree::CAPACITY,
            "invalid pagination"
        );
        self.read(move |conn| {
            let snapshot = snapshot(conn, at)?;
            let items = conn.prepare_cached("SELECT position, height, txid, tx_index, action_index, cmx, nullifier FROM leaves WHERE position >= ?1 AND position < ?2 ORDER BY position LIMIT ?3")?
                .query_map(params![from, snapshot.checkpoint.leaf_count, limit], leaf_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            let next = items.last().map(|l| l.position + 1).filter(|n| *n < snapshot.checkpoint.leaf_count);
            Ok(Page { snapshot, items, next })
        }).await
    }

    pub async fn nullifier(&self, value: Bytes32) -> Result<(Snapshot, Option<Leaf>)> {
        self.read(move |conn| {
            let snapshot = snapshot(conn, None)?;
            let leaf = conn.query_row("SELECT position, height, txid, tx_index, action_index, cmx, nullifier FROM leaves WHERE nullifier = ?1", [value], leaf_row).optional()?;
            Ok((snapshot, leaf))
        }).await
    }

    pub async fn blocks(
        &self,
        from: u32,
        limit: u32,
        at: Option<Bytes32>,
    ) -> Result<Page<BlockDelta>> {
        ensure!((1..=1000).contains(&limit), "invalid pagination");
        self.read(move |conn| {
            let snapshot = snapshot(conn, at)?;
            let items = conn.prepare_cached("SELECT height, hash, time, leaf_count, root, first_position FROM blocks WHERE height >= ?1 AND height <= ?2 ORDER BY height LIMIT ?3")?
                .query_map(params![from, snapshot.checkpoint.height, limit], |row| {
                    let checkpoint = checkpoint_row(row)?;
                    let first_position = row.get(5)?;
                    Ok(BlockDelta { added: checkpoint.leaf_count - first_position, checkpoint, first_position })
                })?.collect::<rusqlite::Result<Vec<_>>>()?;
            let next = items.last().map(|b| u64::from(b.checkpoint.height) + 1).filter(|n| *n <= u64::from(snapshot.checkpoint.height));
            Ok(Page { snapshot, items, next })
        }).await
    }

    pub async fn node(&self, level: u8, index: u64, at: Option<Bytes32>) -> Result<Node> {
        ensure!(
            level <= tree::DEPTH && index < (1u64 << (tree::DEPTH - level)),
            "invalid node coordinates"
        );
        self.read(move |conn| {
            let snapshot = snapshot(conn, at)?;
            let hash =
                Bytes32(tree::node(conn, level, index, snapshot.checkpoint.leaf_count)?.to_bytes());
            let capacity = 1u64 << level;
            let first_position = index * capacity;
            let occupied = snapshot
                .checkpoint
                .leaf_count
                .saturating_sub(first_position)
                .min(capacity);
            Ok(Node {
                snapshot,
                level,
                index,
                hash,
                first_position,
                capacity,
                occupied,
            })
        })
        .await
    }
}
