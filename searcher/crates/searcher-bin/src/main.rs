//! `searcher` — top-level binary. Wires Realtime → Pools → Core → Exec.
//!
//! Phase 0: parses CLI/config, sets up tracing/metrics, exits. Wiring lands in Phase 2.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use serde::Deserialize;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "searcher", about = "MegaETH flash-loan arbitrage searcher")]
struct Cli {
    /// Path to a TOML config (see config/*.toml)
    #[arg(long, env = "SEARCHER_CONFIG")]
    config: PathBuf,

    /// Detect & log opportunities only; never submit transactions.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields wired into runtime in phase 2
struct Config {
    network: NetworkConfig,
    aave: AaveConfig,
    #[serde(default)]
    dexes: Vec<DexConfig>,
    #[serde(default)]
    tokens: Vec<TokenConfig>,
    #[serde(default)]
    pools: Vec<PoolConfig>,
    risk: RiskConfig,
    #[serde(default)]
    metrics: MetricsConfig,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct NetworkConfig {
    name: String,
    chain_id: u64,
    rpc_urls: Vec<String>,
    realtime_ws: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AaveConfig {
    pool: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DexConfig {
    name: String,
    kind: String,
    factory: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TokenConfig {
    symbol: String,
    address: String,
    decimals: u8,
}

/// One pool entry. `fee` is the V3-native value (hundredths of bps; 100 = 0.01%, 3000 = 0.30%).
/// For V2 pools we'll convert at registry-load time (3000 → 30 bps for the V2 math lib).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PoolConfig {
    address: String,
    dex: String,
    pair: String,
    fee: u32,
    vol_24h_usd: f64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RiskConfig {
    max_loan_usd: f64,
    min_profit_usd: f64,
    daily_loss_cap_usd: f64,
}

#[derive(Debug, Default, Deserialize)]
struct MetricsConfig {
    #[serde(default)]
    listen: Option<String>,
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "searcher=info,searcher_core=info".into()),
        )
        .with_target(true)
        .json()
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg_text = std::fs::read_to_string(&cli.config)?;
    let cfg: Config = toml::from_str(&cfg_text)?;

    let pools_low_fee = cfg.pools.iter().filter(|p| p.fee <= 500).count();
    info!(
        network = %cfg.network.name,
        chain_id = cfg.network.chain_id,
        dexes = cfg.dexes.len(),
        tokens = cfg.tokens.len(),
        pools = cfg.pools.len(),
        pools_low_fee = pools_low_fee,
        dry_run = cli.dry_run,
        "searcher booted"
    );

    if let Some(addr) = cfg.metrics.listen.as_deref() {
        let socket: std::net::SocketAddr = addr.parse()?;
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(socket)
            .install()?;
        info!(%addr, "metrics endpoint listening");
    }

    // TODO(phase-2): start Realtime client, pool registry, detector, executor.
    info!("phase 0 boot complete — wiring lands in phase 2");
    Ok(())
}
