use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{config::Config, model::*};

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct JsonRpc {
    client: reqwest::Client,
    url: reqwest::Url,
    auth: Option<(String, String)>,
    cookie: Option<PathBuf>,
    id: Arc<AtomicU64>,
}

#[derive(Debug, Deserialize, thiserror::Error)]
#[error("RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Deserialize)]
struct Response<T> {
    id: u64,
    result: Option<T>,
    error: Option<RpcError>,
}

impl JsonRpc {
    pub fn new(config: &Config) -> Result<Self> {
        let url = reqwest::Url::parse(&config.rpc_url).context("invalid RPC URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "RPC URL must use HTTP(S)"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "use RPC credential environment variables or a cookie, not URL credentials"
        );
        ensure!(
            config.rpc_user.is_some() == config.rpc_password.is_some(),
            "set both RPC user and password"
        );
        ensure!(
            config.rpc_cookie.is_none() || config.rpc_user.is_none(),
            "choose cookie or basic authentication"
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.rpc_timeout_seconds))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .gzip(true)
            .build()?;
        Ok(Self {
            client,
            url,
            auth: config.rpc_user.clone().zip(config.rpc_password.clone()),
            cookie: config.rpc_cookie.clone(),
            id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Transport failures leave checkpoints intact; the polling loop retries the pass.
    pub async fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let mut request = self
            .client
            .post(self.url.clone())
            .json(&json!({"jsonrpc":"1.0", "id":id, "method":method, "params":params}));
        let cookie_auth;
        let auth = if let Some(path) = &self.cookie {
            let cookie = tokio::fs::read_to_string(path)
                .await
                .context("cannot read RPC cookie")?;
            let (user, password) = cookie
                .trim()
                .split_once(':')
                .context("invalid RPC cookie")?;
            cookie_auth = (user.to_owned(), password.to_owned());
            Some(&cookie_auth)
        } else {
            self.auth.as_ref()
        };
        if let Some((user, password)) = auth {
            request = request.basic_auth(user, Some(password));
        }
        let mut response = request
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("{method} transport failed"))?;
        let status = response.status();
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_RESPONSE_BYTES as u64,
            "RPC response too large"
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            ensure!(
                body.len() + chunk.len() <= MAX_RESPONSE_BYTES,
                "RPC response too large"
            );
            body.extend_from_slice(&chunk);
        }
        // Zcash nodes can return a valid JSON-RPC error body with HTTP 500.
        let envelope: Response<T> =
            tokio::task::spawn_blocking(move || serde_json::from_slice(&body))
                .await
                .context("RPC decoder task failed")?
                .with_context(|| format!("{method}: invalid RPC response (HTTP {status})"))?;
        ensure!(envelope.id == id, "RPC response id mismatch");
        if let Some(error) = envelope.error {
            return Err(error.into());
        }
        ensure!(status.is_success(), "{method}: HTTP {status}");
        envelope
            .result
            .with_context(|| format!("{method}: missing result"))
    }

    pub async fn chain_info(&self) -> Result<ChainInfo> {
        self.call("getblockchaininfo", json!([])).await
    }
    pub async fn block_hash(&self, height: u32) -> Result<Bytes32> {
        self.call("getblockhash", json!([height])).await
    }
    pub async fn mempool_ids(&self) -> Result<Vec<Bytes32>> {
        self.call("getrawmempool", json!([false])).await
    }
    pub async fn transaction(&self, id: Bytes32) -> Result<Transaction> {
        let tx: Transaction = self.call("getrawtransaction", json!([id, 1])).await?;
        ensure!(tx.txid == id, "mempool transaction id mismatch");
        tx.actions()?;
        Ok(tx)
    }

    pub async fn block(&self, height: u32) -> Result<VerifiedBlockInput> {
        let block: Block = self
            .call("getblock", json!([height.to_string(), 2]))
            .await?;
        ensure!(block.height == height, "RPC returned wrong block height");
        ensure!(
            block.trees.ironwood.is_some(),
            "RPC getblock must expose trees.ironwood.size; use an Ironwood-capable Zebra node"
        );
        let tree: TreeState = self.call("z_gettreestate", json!([block.hash])).await?;
        ensure!(
            tree.hash == block.hash && tree.height == height,
            "tree state does not match requested block"
        );
        let mut root = tree
            .ironwood
            .context("RPC does not expose Ironwood tree state at this height")?
            .commitments
            .final_root;
        root.0.reverse();
        Ok(VerifiedBlockInput {
            block,
            expected_root: root,
        })
    }
}

/// A single upstream transport. Both variants preserve the same storage contract.
#[derive(Clone)]
pub enum Rpc {
    Json(JsonRpc),
    Lightwalletd(crate::lightwalletd::Lightwalletd),
}

impl Rpc {
    pub fn new(config: &Config) -> Result<Self> {
        ensure!(
            !config.public_rpc && config.lightwalletd_url.len() <= 1,
            "use Providers for multiple upstreams"
        );
        match config.lightwalletd_url.first() {
            Some(url) => Ok(Self::Lightwalletd(crate::lightwalletd::Lightwalletd::new(
                url, config,
            )?)),
            None => Ok(Self::Json(JsonRpc::new(config)?)),
        }
    }

    pub async fn chain_info(&self) -> Result<ChainInfo> {
        match self {
            Self::Json(rpc) => rpc.chain_info().await,
            Self::Lightwalletd(rpc) => rpc.chain_info().await,
        }
    }

    pub async fn block_hash(&self, height: u32) -> Result<Bytes32> {
        match self {
            Self::Json(rpc) => rpc.block_hash(height).await,
            Self::Lightwalletd(rpc) => rpc.block_hash(height).await,
        }
    }

    pub async fn block(&self, height: u32) -> Result<VerifiedBlockInput> {
        match self {
            Self::Json(rpc) => rpc.block(height).await,
            Self::Lightwalletd(_) => anyhow::bail!("lightwalletd blocks require a verified range"),
        }
    }

    pub async fn mempool_ids(&self) -> Result<Vec<Bytes32>> {
        match self {
            Self::Json(rpc) => rpc.mempool_ids().await,
            Self::Lightwalletd(_) => anyhow::bail!("lightwalletd uses compact mempool snapshots"),
        }
    }

    pub async fn transaction(&self, id: Bytes32) -> Result<Transaction> {
        match self {
            Self::Json(rpc) => rpc.transaction(id).await,
            Self::Lightwalletd(_) => anyhow::bail!("lightwalletd uses compact mempool snapshots"),
        }
    }

    pub async fn baseline(&self, height: u32) -> Result<Checkpoint> {
        match self {
            Self::Lightwalletd(rpc) => rpc.baseline(height).await,
            Self::Json(rpc) => {
                #[derive(Deserialize)]
                struct Baseline {
                    hash: Bytes32,
                    height: u32,
                    time: u64,
                    trees: Trees,
                }
                let block: Baseline = rpc.call("getblock", json!([height.to_string(), 1])).await?;
                ensure!(block.height == height, "RPC baseline height mismatch");
                ensure!(
                    block.trees.ironwood.as_ref().map_or(0, |t| t.size) == 0,
                    "start height skips existing Ironwood leaves; a full sync is required"
                );
                Ok(Checkpoint {
                    height,
                    hash: block.hash,
                    time: block.time,
                    leaf_count: 0,
                    root: crate::tree::empty_root(),
                })
            }
        }
    }
}
