//! Read-only live compatibility check; no database is opened or modified.
use anyhow::{Result, ensure};
use clap::Parser;
use futures::{StreamExt, stream};
use ironwood_indexer::{
    config::Config, lightwalletd::Lightwalletd, providers::PUBLIC_MAINNET_PROVIDERS,
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();
    ensure!(
        config.network == "main",
        "this check uses the first ten mainnet Ironwood blocks"
    );
    let urls = if config.lightwalletd_url.is_empty() {
        PUBLIC_MAINNET_PROVIDERS
            .iter()
            .map(|url| (*url).to_owned())
            .collect()
    } else {
        config.lightwalletd_url.clone()
    };
    let mut checks = stream::iter(urls).map(|url| {
        let config = &config;
        async move {
            let label = reqwest::Url::parse(&url).map(|url| url.origin().ascii_serialization()).unwrap_or_else(|_| "invalid URL".into());
            let started = std::time::Instant::now();
            let result: Result<_> = async {
                let client = Lightwalletd::new(&url, config)?;
                let info = client.chain_info().await?;
                let genesis = client.block_hash(0).await?;
                let baseline = client.baseline(3_428_142).await?;
                let blocks = client.blocks(&baseline, 3_428_152).await?;
                let last = blocks.last().ok_or_else(|| anyhow::anyhow!("empty range"))?;
                let count = last.block.trees.ironwood.as_ref().ok_or_else(|| anyhow::anyhow!("missing tree size"))?.size;
                ensure!(count == 97, "unexpected first-ten-block leaf count");
                let mempool = client.mempool().await?;
                Ok(json!({"height": info.blocks, "genesis": genesis, "historical_blocks": blocks.len(), "verified_leaves": count, "root": last.expected_root, "mempool_transactions": mempool.len(), "mempool_actions": mempool.values().map(Vec::len).sum::<usize>()}))
            }.await;
            match result {
                Ok(details) => (true, json!({"provider": label, "ok": true, "seconds": started.elapsed().as_secs_f64(), "details": details})),
                Err(error) => (false, json!({"provider": label, "ok": false, "seconds": started.elapsed().as_secs_f64(), "error": format!("{error:#}")})),
            }
        }
    }).buffer_unordered(2);
    let mut success = true;
    while let Some((ok, result)) = checks.next().await {
        println!("{}", serde_json::to_string(&result)?);
        success &= ok;
    }
    ensure!(success, "one or more live providers failed the check");
    Ok(())
}
