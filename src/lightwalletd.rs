//! Read-only lightwalletd transport. Compact ranges are verified before persistence.
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use incrementalmerkletree::frontier::{CommitmentTree, Frontier};
use orchard::tree::MerkleHashOrchard as Hash;
use prost::Message;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::{config::Config, model::*, tree};

#[allow(clippy::all)]
mod proto {
    tonic::include_proto!("cash.z.wallet.sdk.rpc");
}
use proto::compact_tx_streamer_client::CompactTxStreamerClient;

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Lightwalletd {
    client: CompactTxStreamerClient<Channel>,
    network: String,
    batch_size: u16,
    timeout: Duration,
}

impl Lightwalletd {
    pub fn new(url: &str, config: &Config) -> Result<Self> {
        let parsed = reqwest::Url::parse(url).context("invalid lightwalletd URL")?;
        ensure!(
            parsed.scheme() == "https",
            "lightwalletd requires HTTPS with certificate verification"
        );
        ensure!(
            parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "lightwalletd URL must not contain credentials, a query, or a fragment"
        );
        let timeout = Duration::from_secs(config.rpc_timeout_seconds);
        let channel = Endpoint::from_shared(url.to_owned())?
            .tls_config(ClientTlsConfig::new().with_webpki_roots())?
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .connect_lazy();
        Ok(Self {
            client: CompactTxStreamerClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            network: config.network.clone(),
            batch_size: config.batch_size,
            timeout,
        })
    }

    pub async fn block_usage(&self, start: u32, end: u32) -> Result<Vec<crate::usage::BlockUsage>> {
        ensure!(
            end >= start && u64::from(end - start) < u64::from(self.batch_size),
            "invalid usage range"
        );
        let request = proto::BlockRange {
            start: Some(block_id(start)),
            end: Some(block_id(end)),
            pool_types: vec![
                proto::PoolType::Transparent as i32,
                proto::PoolType::Sapling as i32,
                proto::PoolType::Orchard as i32,
                proto::PoolType::Ironwood as i32,
            ],
        };
        let mut stream = self
            .client
            .clone()
            .get_block_range(request)
            .await?
            .into_inner();
        let mut rows = Vec::new();
        // Bound idle time per message. Dense historical ranges can take longer
        // than one request timeout while still delivering blocks continuously.
        while let Some(block) = tokio::time::timeout(self.timeout, stream.message())
            .await
            .context("timed out waiting for an activity block")??
        {
            ensure!(
                block.height == u64::from(start) + rows.len() as u64
                    && block.height <= u64::from(end),
                "unexpected usage block"
            );
            let mut row =
                crate::usage::BlockUsage::new(block.height.try_into()?, display_hash(&block.hash)?);
            for tx in block.vtx {
                row.count(
                    tx.index == 0,
                    tx.ironwood_actions.len(),
                    tx.actions.len() + tx.spends.len() + tx.outputs.len(),
                    !tx.vin.is_empty() || !tx.vout.is_empty(),
                );
            }
            rows.push(row);
        }
        ensure!(
            rows.len() as u64 == u64::from(end - start) + 1,
            "incomplete block usage range"
        );
        Ok(rows)
    }

    pub async fn raw_transaction(&self, txid: Bytes32) -> Result<(Vec<u8>, u64)> {
        let mut hash = txid.0;
        hash.reverse();
        let raw = self
            .client
            .clone()
            .get_transaction(proto::TxFilter {
                hash: hash.to_vec(),
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok((raw.data, raw.height))
    }

    pub fn batch_size(&self) -> u16 {
        self.batch_size
    }

    pub async fn chain_info(&self) -> Result<ChainInfo> {
        let info = self
            .client
            .clone()
            .get_lightd_info(proto::Empty {})
            .await?
            .into_inner();
        ensure!(
            info.chain_name == self.network,
            "lightwalletd network mismatch"
        );
        // Pool filtering was introduced in protocol 0.6. Older servers may silently ignore it.
        let version = info.lightwallet_protocol_version.trim_start_matches('v');
        let mut parts = version.split('.');
        let major: u32 = parts.next().context("missing protocol version")?.parse()?;
        let minor: u32 = parts
            .next()
            .context("missing protocol minor version")?
            .parse()?;
        ensure!(
            (major, minor) >= (0, 6),
            "lightwalletd protocol 0.6 or newer is required for Ironwood"
        );
        let tip = self
            .client
            .clone()
            .get_latest_block(proto::ChainSpec {})
            .await?
            .into_inner();
        let upgrades = activation(&self.network)
            .map(|height| {
                (
                    "NU6.3".to_owned(),
                    Upgrade {
                        name: "NU6.3".to_owned(),
                        activationheight: height,
                    },
                )
            })
            .into_iter()
            .collect();
        Ok(ChainInfo {
            chain: info.chain_name,
            blocks: tip.height.try_into()?,
            bestblockhash: display_hash(&tip.hash)?,
            upgrades,
        })
    }

    pub async fn block_hash(&self, height: u32) -> Result<Bytes32> {
        // Height zero means an unspecified identifier in GetBlock. Block 1 commits to genesis.
        if height == 0 {
            let block = self
                .client
                .clone()
                .get_block(block_id(1))
                .await?
                .into_inner();
            ensure!(block.height == 1, "wrong genesis successor height");
            display_hash(&block.prev_hash)
        } else {
            self.tree_state(height)
                .await?
                .hash
                .parse()
                .context("invalid block hash")
        }
    }

    async fn tree_state(&self, height: u32) -> Result<proto::TreeState> {
        let state = self
            .client
            .clone()
            .get_tree_state(block_id(height))
            .await?
            .into_inner();
        ensure!(
            state.height == u64::from(height) && state.network == self.network,
            "tree state height/network mismatch"
        );
        state
            .hash
            .parse::<Bytes32>()
            .context("invalid tree state block hash")?;
        Ok(state)
    }

    pub async fn baseline(&self, height: u32) -> Result<Checkpoint> {
        let state = self.tree_state(height).await?;
        // Older tree states omit the pool entirely before activation.
        let frontier = if state.ironwood_tree.is_empty() {
            Frontier::empty()
        } else {
            decode_frontier(&state.ironwood_tree)?
        };
        ensure!(
            frontier.tree_size() == 0,
            "start height skips existing Ironwood leaves"
        );
        Ok(Checkpoint {
            height,
            hash: state.hash.parse()?,
            time: u64::from(state.time),
            leaf_count: 0,
            root: tree::empty_root(),
        })
    }

    pub async fn blocks(&self, previous: &Checkpoint, end: u32) -> Result<Vec<VerifiedBlockInput>> {
        ensure!(
            end > previous.height && end - previous.height <= u32::from(self.batch_size),
            "invalid compact block range"
        );
        tokio::time::timeout(self.timeout, async {
            let start = previous.height + 1;
            let before = self.tree_state(previous.height).await?;
            let frontier = if before.ironwood_tree.is_empty() && previous.leaf_count == 0 {
                Frontier::empty()
            } else {
                decode_frontier(&before.ironwood_tree)?
            };
            ensure!(
                before.hash.parse::<Bytes32>()? == previous.hash
                    && frontier.tree_size() == previous.leaf_count
                    && Bytes32(frontier.root().to_bytes()) == previous.root,
                "starting tree state differs from persisted checkpoint"
            );
            let request = proto::BlockRange {
                start: Some(block_id(start)),
                end: Some(block_id(end)),
                pool_types: vec![proto::PoolType::Ironwood as i32],
            };
            let mut stream = self
                .client
                .clone()
                .get_block_range(request)
                .await?
                .into_inner();
            let mut blocks = Vec::new();
            let mut bytes = 0;
            while let Some(block) = stream.message().await? {
                bytes += block.encoded_len();
                ensure!(
                    bytes <= MAX_BATCH_BYTES,
                    "compact batch exceeds 64 MiB; reduce --batch-size"
                );
                ensure!(
                    blocks.len() < (end - previous.height) as usize,
                    "too many compact blocks"
                );
                blocks.push(block);
            }
            ensure!(
                blocks.len() == (end - previous.height) as usize,
                "truncated compact block stream"
            );
            let after = self.tree_state(end).await?;
            let previous = previous.clone();
            tokio::task::spawn_blocking(move || verify_range(&previous, frontier, blocks, after))
                .await?
        })
        .await
        .context("lightwalletd block range timed out")?
    }

    /// A complete bounded snapshot; publication happens only after the stream finishes.
    pub async fn mempool(&self) -> Result<BTreeMap<Bytes32, Vec<Action>>> {
        tokio::time::timeout(self.timeout, async {
            let request = proto::GetMempoolTxRequest {
                exclude_txid_suffixes: vec![],
                pool_types: vec![proto::PoolType::Ironwood as i32],
            };
            let mut stream = self
                .client
                .clone()
                .get_mempool_tx(request)
                .await?
                .into_inner();
            let mut transactions = BTreeMap::new();
            let mut bytes = 0;
            let mut count = 0;
            while let Some(tx) = stream.message().await? {
                bytes += tx.encoded_len();
                ensure!(bytes <= MAX_BATCH_BYTES, "compact mempool exceeds 64 MiB");
                count += tx.ironwood_actions.len();
                ensure!(
                    transactions.len() < crate::mempool::MAX_MEMPOOL_TRANSACTIONS
                        && count <= crate::mempool::MAX_MEMPOOL_ACTIONS,
                    "compact mempool exceeds configured bounds"
                );
                let tx = transaction(tx)?;
                ensure!(
                    transactions
                        .insert(
                            tx.txid,
                            tx.ironwood.context("missing normalized bundle")?.actions
                        )
                        .is_none(),
                    "duplicate mempool transaction"
                );
            }
            Ok(transactions)
        })
        .await
        .context("lightwalletd mempool timed out")?
    }
}

// NU6.3 constants from zcash_protocol::consensus, checked 2026-11-18.
// lightwalletd advertises the next upgrade only, not historical activation heights.
fn activation(network: &str) -> Option<u32> {
    match network {
        "main" => Some(3_428_143),
        "test" => Some(4_134_000),
        _ => None,
    }
}

fn block_id(height: u32) -> proto::BlockId {
    proto::BlockId {
        height: u64::from(height),
        hash: vec![],
    }
}

fn field(bytes: &[u8]) -> Result<Bytes32> {
    let value = Bytes32(bytes.try_into().context("field must be exactly 32 bytes")?);
    tree::decode(value)?;
    Ok(value)
}
fn display_hash(bytes: &[u8]) -> Result<Bytes32> {
    let mut value: [u8; 32] = bytes
        .try_into()
        .context("identifier must be exactly 32 bytes")?;
    value.reverse();
    Ok(Bytes32(value))
}
fn transaction(tx: proto::CompactTx) -> Result<Transaction> {
    let actions = tx
        .ironwood_actions
        .into_iter()
        .map(|action| {
            Ok(Action {
                cmx: field(&action.cmx)?,
                nullifier: field(&action.nullifier)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Transaction {
        index: Some(tx.index.try_into()?),
        txid: display_hash(&tx.txid)?,
        // Compact transactions omit the version; an Ironwood bundle implies v6 or later.
        version: 6,
        ironwood: Some(Bundle { actions }),
    })
}

fn verify_range(
    previous: &Checkpoint,
    mut frontier: Frontier<Hash, 32>,
    blocks: Vec<proto::CompactBlock>,
    after: proto::TreeState,
) -> Result<Vec<VerifiedBlockInput>> {
    let mut checkpoint = previous.clone();
    let mut verified = Vec::with_capacity(blocks.len());
    for compact in blocks {
        ensure!(
            compact.height == u64::from(checkpoint.height) + 1
                && display_hash(&compact.prev_hash)? == checkpoint.hash,
            "compact blocks are out of order or cross a reorg"
        );
        let count = u64::from(
            compact
                .chain_metadata
                .context("missing compact chain metadata")?
                .ironwood_commitment_tree_size,
        );
        let mut last_index = None;
        let mut transactions = Vec::new();
        for tx in compact.vtx {
            ensure!(
                last_index.is_none_or(|last| tx.index > last),
                "compact transaction indexes must increase"
            );
            last_index = Some(tx.index);
            if tx.ironwood_actions.is_empty() {
                continue;
            }
            let tx = transaction(tx)?;
            for action in tx.actions()? {
                ensure!(
                    frontier.append(tree::decode(action.cmx)?),
                    "commitment tree is full"
                );
            }
            transactions.push(tx);
        }
        ensure!(
            frontier.tree_size() == count,
            "compact block tree size mismatch at {}",
            compact.height
        );
        let root = if count == checkpoint.leaf_count {
            checkpoint.root
        } else {
            Bytes32(frontier.root().to_bytes())
        };
        let block = Block {
            height: compact.height.try_into()?,
            hash: display_hash(&compact.hash)?,
            previousblockhash: checkpoint.hash,
            time: u64::from(compact.time),
            tx: transactions,
            trees: Trees {
                ironwood: Some(TreeSize { size: count }),
            },
        };
        checkpoint = Checkpoint {
            height: block.height,
            hash: block.hash,
            time: block.time,
            leaf_count: count,
            root,
        };
        verified.push(VerifiedBlockInput {
            block,
            expected_root: root,
        });
    }
    ensure!(
        after.height == u64::from(checkpoint.height)
            && after.hash.parse::<Bytes32>()? == checkpoint.hash,
        "ending tree state block mismatch"
    );
    let ending = decode_frontier(&after.ironwood_tree)?;
    ensure!(
        ending.tree_size() == checkpoint.leaf_count
            && Bytes32(ending.root().to_bytes()) == checkpoint.root,
        "Ironwood range root/count differs from server tree state"
    );
    Ok(verified)
}

/// Legacy commitment-tree encoding used by zcash_primitives::merkle_tree.
/// Bound and validate all parts before entering upstream code with invariant assumptions.
fn decode_frontier(encoded: &str) -> Result<Frontier<Hash, 32>> {
    ensure!(
        encoded.len() <= 2 * (3 + 33 * 33),
        "tree state exceeds depth-32 encoding bound"
    );
    let bytes = hex::decode(encoded).context("invalid tree state hex")?;
    let mut reader = Cursor::new(bytes.as_slice());
    fn byte(reader: &mut Cursor<&[u8]>) -> Result<u8> {
        let mut value = [0];
        reader.read_exact(&mut value)?;
        Ok(value[0])
    }
    fn optional(reader: &mut Cursor<&[u8]>) -> Result<Option<Hash>> {
        match byte(reader)? {
            0 => Ok(None),
            1 => {
                let mut value = [0; 32];
                reader.read_exact(&mut value)?;
                Ok(Some(tree::decode(Bytes32(value))?))
            }
            _ => bail!("invalid optional tree hash tag"),
        }
    }
    let left = optional(&mut reader)?;
    let right = optional(&mut reader)?;
    let n = byte(&mut reader)?;
    // Every valid depth-32 parent vector has length < 32, so its CompactSize is one byte.
    ensure!(n < 32, "invalid tree parent count");
    let parents: Vec<_> = (0..n)
        .map(|_| optional(&mut reader))
        .collect::<Result<_>>()?;
    ensure!(
        reader.position() == bytes.len() as u64,
        "trailing tree state bytes"
    );
    ensure!(
        left.is_some() || (right.is_none() && parents.iter().all(Option::is_none)),
        "invalid tree state: parents/right without a leaf"
    );
    let tree = CommitmentTree::<Hash, 32>::from_parts(left, right, parents)
        .map_err(|_| anyhow::anyhow!("invalid tree state"))?;
    Ok(tree.to_frontier())
}

#[cfg(test)]
mod tests;
