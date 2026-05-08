//! `searcher` — top-level binary. Wires Realtime/poller → Pools → Core → Exec.
//!
//! Phase 2: in `--watch-only` mode, builds a V3 pool registry from the config,
//! spawns the polling state cache, and JSON-logs tick events. Detector and
//! executor wiring follow in steps 3-4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use searcher_net::abis::IUniswapV3Pool;
use searcher_net::PoolPoller;
use searcher_pools::{PoolRegistry, V3PoolMeta};
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "searcher", about = "MegaETH flash-loan arbitrage searcher")]
struct Cli {
    /// Path to a TOML config (see config/*.toml)
    #[arg(long, env = "SEARCHER_CONFIG")]
    config: PathBuf,

    /// Detect & log opportunities only; never submit transactions.
    #[arg(long, default_value_t = false, alias = "dry-run")]
    watch_only: bool,

    /// Poll cadence in milliseconds (default 150).
    #[arg(long, env = "SEARCHER_POLL_MS", default_value_t = 150)]
    poll_ms: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields wired into runtime in later phases
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

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct TokenConfig {
    symbol: String,
    address: String,
    decimals: u8,
}

/// One pool entry. `fee` is the V3-native value (hundredths of bps; 100 = 0.01%, 3000 = 0.30%).
#[derive(Debug, Deserialize, Clone)]
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
                .unwrap_or_else(|_| "searcher=info,searcher_core=info,searcher_net=info".into()),
        )
        .with_target(true)
        .json()
        .init();
}

/// Cross-venue arb candidates: pools whose `(pair, fee)` group spans ≥ 2 DEXs
/// and fee ≤ 30 bps. Anything wider can't clear the bar.
fn cross_venue_pools(pools: &[PoolConfig]) -> Vec<&PoolConfig> {
    let mut groups: HashMap<(String, u32), Vec<&PoolConfig>> = HashMap::new();
    for p in pools {
        if p.fee > 3000 {
            continue;
        }
        groups.entry((p.pair.clone(), p.fee)).or_default().push(p);
    }
    let mut out = Vec::new();
    for (_, group) in groups {
        let dexes: std::collections::HashSet<&str> = group.iter().map(|p| p.dex.as_str()).collect();
        if dexes.len() >= 2 {
            out.extend(group);
        }
    }
    out.sort_by(|a, b| a.address.cmp(&b.address));
    out
}

/// Build a pool registry from the config. For each watched pool, queries
/// `token0()` / `token1()` once at startup so the metadata captures on-chain
/// ordering rather than guessing from the symbolic pair string.
async fn build_registry<P, T>(provider: &P, cfg: &Config) -> Result<Arc<PoolRegistry>>
where
    P: Provider<T>,
    T: alloy::transports::Transport + Clone,
{
    let token_by_symbol: HashMap<&str, &TokenConfig> =
        cfg.tokens.iter().map(|t| (t.symbol.as_str(), t)).collect();

    let watched = cross_venue_pools(&cfg.pools);
    info!(count = watched.len(), "building pool registry");

    // Pair strings sometimes carry a display dot (e.g. "BTC.b/USDm") that
    // the on-chain symbol does not (e.g. "BTCb"). Try both forms.
    let resolve = |sym: &str| -> Option<&TokenConfig> {
        if let Some(t) = token_by_symbol.get(sym).copied() {
            return Some(t);
        }
        let stripped: String = sym.chars().filter(|c| *c != '.').collect();
        token_by_symbol.get(stripped.as_str()).copied()
    };

    let mut meta_map: HashMap<Address, V3PoolMeta> = HashMap::new();
    for p in watched {
        let addr: Address = p.address.parse().context("pool address parse")?;

        let symbols: Vec<&str> = p.pair.split('/').collect();
        if symbols.len() != 2 {
            return Err(anyhow!("pair {} not 'X/Y'", p.pair));
        }
        let (Some(t_a), Some(t_b)) = (resolve(symbols[0]), resolve(symbols[1])) else {
            warn!(
                pair = %p.pair,
                pool = %addr,
                "skipping pool — one or both token symbols not in [[tokens]]"
            );
            continue;
        };
        let addr_a: Address = t_a.address.parse().context("token a address parse")?;
        let addr_b: Address = t_b.address.parse().context("token b address parse")?;

        let pool = IUniswapV3Pool::new(addr, provider);
        let token0 = pool
            .token0()
            .call()
            .await
            .with_context(|| format!("token0() for {}", addr))?
            ._0;
        let token1 = pool
            .token1()
            .call()
            .await
            .with_context(|| format!("token1() for {}", addr))?
            ._0;

        let (decimals0, decimals1) = if token0 == addr_a && token1 == addr_b {
            (t_a.decimals, t_b.decimals)
        } else if token0 == addr_b && token1 == addr_a {
            (t_b.decimals, t_a.decimals)
        } else {
            return Err(anyhow!(
                "on-chain (token0,token1)=({},{}) does not match config pair {} ({},{})",
                token0,
                token1,
                p.pair,
                addr_a,
                addr_b
            ));
        };

        meta_map.insert(
            addr,
            V3PoolMeta {
                addr,
                dex: p.dex.clone(),
                pair: p.pair.clone(),
                fee_pips: p.fee,
                token0,
                token1,
                decimals0,
                decimals1,
            },
        );
    }

    Ok(Arc::new(PoolRegistry::new(meta_map)))
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
        watch_only = cli.watch_only,
        poll_ms = cli.poll_ms,
        "searcher booted"
    );

    if let Some(addr) = cfg.metrics.listen.as_deref() {
        let socket: std::net::SocketAddr = addr.parse()?;
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(socket)
            .install()?;
        info!(%addr, "metrics endpoint listening");
    }

    if !cli.watch_only {
        warn!("non-watch-only mode is not implemented yet (phase 3); exiting");
        return Ok(());
    }

    let rpc_url = cfg
        .network
        .rpc_urls
        .first()
        .ok_or_else(|| anyhow!("no rpc_urls configured"))?;
    let provider = ProviderBuilder::new().on_http(rpc_url.parse().context("invalid rpc url")?);

    let registry = build_registry(&provider, &cfg).await?;
    info!(pools = registry.pool_count(), "pool registry built");

    let (block_tx, _block_rx) = watch::channel(0u64);
    let poller = PoolPoller::new(
        provider,
        registry.clone(),
        Duration::from_millis(cli.poll_ms),
        block_tx,
    );

    info!("starting pool poller");
    poller.run().await?;
    Ok(())
}
