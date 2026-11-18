use clap::Parser;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(version, about)]
pub struct Config {
    /// Use the four built-in free mainnet lightwalletd providers with automatic failover.
    #[arg(long, conflicts_with_all = ["lightwalletd_url", "rpc_url", "rpc_user", "rpc_password", "rpc_cookie"])]
    pub public_rpc: bool,
    /// Ordered lightwalletd endpoints (TLS). Repeat the flag or separate URLs with commas.
    #[arg(long, env = "ZCASH_LIGHTWALLETD_URL", value_delimiter = ',', conflicts_with_all = ["rpc_url", "rpc_user", "rpc_password", "rpc_cookie"])]
    pub lightwalletd_url: Vec<String>,
    /// Maximum blocks per verified lightwalletd batch.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u16).range(1..=2000))]
    pub batch_size: u16,
    #[arg(
        long,
        env = "ZCASH_RPC_URL",
        default_value = "http://127.0.0.1:8232",
        hide_env_values = true
    )]
    pub rpc_url: String,
    #[arg(long, env = "ZCASH_RPC_USER", hide_env_values = true)]
    pub rpc_user: Option<String>,
    #[arg(long, env = "ZCASH_RPC_PASSWORD", hide_env_values = true)]
    pub rpc_password: Option<String>,
    /// Node cookie file; reread on requests so node restarts rotate credentials safely.
    #[arg(long, env = "ZCASH_RPC_COOKIE")]
    pub rpc_cookie: Option<PathBuf>,
    #[arg(long, env = "IRONWOOD_DB", default_value = "data/ironwood.sqlite")]
    pub database: PathBuf,
    #[arg(long, env = "IRONWOOD_BIND", default_value = "127.0.0.1:3000")]
    pub bind: SocketAddr,
    /// Exact proxy IPs allowed to supply an overwritten X-Real-IP header.
    #[arg(long, env = "IRONWOOD_TRUSTED_PROXIES", value_delimiter = ',')]
    pub trusted_proxy: Vec<IpAddr>,
    /// Shared secret for Vercel's authenticated client-IP forwarding (32+ characters).
    #[arg(long, env = "IRONWOOD_PROXY_TOKEN", hide_env_values = true, value_parser = proxy_token)]
    pub proxy_token: Option<String>,
    /// Required expected network: main, test, or regtest.
    #[arg(long, env = "ZCASH_NETWORK", default_value = "main", value_parser = ["main", "test", "regtest"])]
    pub network: String,
    /// Exact activation height. Discovered via JSON-RPC; built in for public main/test gRPC.
    #[arg(long, env = "IRONWOOD_START_HEIGHT", value_parser = clap::value_parser!(u32).range(1..))]
    pub start_height: Option<u32>,
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=32))]
    pub concurrency: u16,
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    pub poll_seconds: u64,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub rpc_timeout_seconds: u64,
    /// Finish a catch-up pass and a mempool sample, then exit without serving HTTP.
    #[arg(long)]
    pub once: bool,
}

fn proxy_token(value: &str) -> Result<String, &'static str> {
    if value.len() >= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        Ok(value.to_owned())
    } else {
        Err("proxy token must contain at least 32 ASCII letters, digits, hyphens or underscores")
    }
}
