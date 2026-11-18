use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Exactly 32 bytes. Hex preserves RPC order; field elements are little-endian,
/// while block/transaction identifiers use the node's display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Bytes32(pub [u8; 32]);

impl rusqlite::types::ToSql for Bytes32 {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(
            rusqlite::types::ValueRef::Blob(&self.0),
        ))
    }
}

impl rusqlite::types::FromSql for Bytes32 {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let bytes = value.as_blob()?;
        let array =
            bytes
                .try_into()
                .map_err(|_| rusqlite::types::FromSqlError::InvalidBlobSize {
                    expected_size: 32,
                    blob_size: bytes.len(),
                })?;
        Ok(Self(array))
    }
}

impl fmt::Display for Bytes32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl FromStr for Bytes32 {
    type Err = hex::FromHexError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(s, &mut bytes)?;
        Ok(Self(bytes))
    }
}

impl Serialize for Bytes32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Bytes32 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChainInfo {
    pub chain: String,
    pub blocks: u32,
    pub bestblockhash: Bytes32,
    pub upgrades: BTreeMap<String, Upgrade>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Upgrade {
    pub name: String,
    pub activationheight: u32,
}

impl ChainInfo {
    pub fn ironwood_activation(&self) -> Option<u32> {
        self.upgrades
            .values()
            .find(|u| {
                let name: String = u.name.chars().filter(char::is_ascii_alphanumeric).collect();
                name.eq_ignore_ascii_case("nu63") || name.eq_ignore_ascii_case("ironwood")
            })
            .map(|u| u.activationheight)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Block {
    pub hash: Bytes32,
    pub height: u32,
    pub previousblockhash: Bytes32,
    pub time: u64,
    pub tx: Vec<Transaction>,
    pub trees: Trees,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Trees {
    pub ironwood: Option<TreeSize>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TreeSize {
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Transaction {
    /// Original block index when a compact response omits unrelated transactions.
    #[serde(skip)]
    pub index: Option<u32>,
    pub txid: Bytes32,
    pub version: u32,
    pub ironwood: Option<Bundle>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Bundle {
    // Deliberately required: malformed/unsupported RPC data must not look empty.
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Action {
    pub nullifier: Bytes32,
    pub cmx: Bytes32,
}

impl Transaction {
    pub fn actions(&self) -> anyhow::Result<&[Action]> {
        anyhow::ensure!(
            self.version <= 6,
            "unsupported transaction version {}",
            self.version
        );
        anyhow::ensure!(
            self.version >= 6 || self.ironwood.is_none(),
            "Ironwood bundle in a pre-v6 transaction"
        );
        Ok(self.ironwood.as_ref().map_or(&[], |bundle| &bundle.actions))
    }
}

#[derive(Debug, Deserialize)]
pub struct TreeState {
    pub hash: Bytes32,
    pub height: u32,
    pub ironwood: Option<PoolState>,
}

#[derive(Debug, Deserialize)]
pub struct PoolState {
    pub commitments: Commitments,
}

#[derive(Debug, Deserialize)]
pub struct Commitments {
    #[serde(rename = "finalRoot")]
    pub final_root: Bytes32,
}

#[derive(Clone, Debug)]
pub struct VerifiedBlockInput {
    pub block: Block,
    /// Canonical field encoding, converted from the RPC's reversed root display order.
    pub expected_root: Bytes32,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Checkpoint {
    pub height: u32,
    pub hash: Bytes32,
    pub time: u64,
    pub leaf_count: u64,
    pub root: Bytes32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub network: String,
    pub start_height: u32,
    pub generation: u64,
    pub checkpoint: Checkpoint,
}

#[derive(Clone, Debug, Serialize)]
pub struct Leaf {
    pub position: u64,
    pub height: u32,
    /// Timestamp of the containing block, not a wall-clock arrival time.
    pub time: u64,
    pub txid: Bytes32,
    pub tx_index: u32,
    pub action_index: u32,
    pub cmx: Bytes32,
    pub nullifier: Bytes32,
}

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
