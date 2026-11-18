//! Ironwood uses the Orchard protocol's depth-32 Sinsemilla commitment tree.
//! Store complete subtrees once; derive partial and empty subtrees at read time.
use anyhow::{Context, Result, ensure};
use incrementalmerkletree::{Hashable, Level};
use orchard::tree::MerkleHashOrchard as Hash;
use rusqlite::{Connection, params};

use crate::model::Bytes32;

pub const DEPTH: u8 = 32;
pub const CAPACITY: u64 = 1u64 << DEPTH;

pub fn empty_root() -> Bytes32 {
    Bytes32(Hash::empty_root(Level::from(DEPTH)).to_bytes())
}

pub fn decode(value: Bytes32) -> Result<Hash> {
    Option::from(Hash::from_bytes(&value.0)).context("noncanonical Pallas field encoding")
}

fn stored(conn: &Connection, level: u8, index: u64) -> Result<Hash> {
    let value = if level == 0 {
        conn.prepare_cached("SELECT cmx FROM leaves WHERE position = ?1")?
            .query_row([index], |row| row.get(0))?
    } else {
        conn.prepare_cached("SELECT hash FROM nodes WHERE level = ?1 AND node_index = ?2")?
            .query_row(params![level, index], |row| row.get(0))?
    };
    decode(value)
}

/// Call after inserting the leaf, inside the same SQLite transaction.
pub fn append(conn: &Connection, position: u64, cmx: Bytes32, height: u32) -> Result<()> {
    ensure!(position < CAPACITY, "Ironwood commitment tree is full");
    let mut hash = decode(cmx)?;
    let mut index = position;
    let mut level = 0;
    while index & 1 == 1 {
        let left = stored(conn, level, index - 1)?;
        hash = Hash::combine(Level::from(level), &left, &hash);
        index >>= 1;
        level += 1;
        conn.prepare_cached(
            "INSERT INTO nodes(level, node_index, hash, height) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![level, index, Bytes32(hash.to_bytes()), height])?;
    }
    Ok(())
}

/// Level 0 means a leaf, level 32 the root. `count` pins a historical snapshot.
pub fn node(conn: &Connection, level: u8, index: u64, count: u64) -> Result<Hash> {
    ensure!(level <= DEPTH && count <= CAPACITY, "invalid tree bounds");
    ensure!(index < (1u64 << (DEPTH - level)), "node index outside tree");
    let width = 1u64 << level;
    let first = index * width;
    if first >= count {
        return Ok(Hash::empty_root(Level::from(level)));
    }
    if first + width <= count {
        return stored(conn, level, index);
    }
    let left = node(conn, level - 1, index * 2, count)?;
    let right = node(conn, level - 1, index * 2 + 1, count)?;
    Ok(Hash::combine(Level::from(level - 1), &left, &right))
}
