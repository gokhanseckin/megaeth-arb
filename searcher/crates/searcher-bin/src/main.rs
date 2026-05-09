//! `searcher` — top-level binary. Wires Realtime/poller → Pools → Core → Exec.
//!
//! Phase 2 watch-only: builds a V3 pool registry from the config, spawns the
//! polling state cache, enumerates cross-venue arb candidates, and re-evaluates
//! every candidate on each block. Profitable candidates emit `Opened` /
//! `Closed` JSON events via the structured `tracing` sink. No transactions.
//!
//! Detector and executor wiring for live submission lands in Phase 3.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use searcher_core::{
    evaluate_candidate, Candidate, DetectorConfig, OpportunityEvent, OpportunityTracker,
    PoolStateView, V3Leg,
};
use searcher_net::abis::IUniswapV3Pool;
use searcher_net::PoolPoller;
use searcher_pools::{PoolRegistry, V3PoolMeta};
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{debug, info, warn};

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
    #[serde(default)]
    pool_addresses_provider: Option<String>,
    #[serde(default)]
    oracle: Option<String>,
    #[serde(default)]
    data_provider: Option<String>,
    /// Addresses borrowable via `flashLoanSimple` — only these are valid loan
    /// legs in arb cycles. On MegaETH this is the USDm/USDe/USDT0 set.
    #[serde(default)]
    flashloan_assets: Vec<String>,
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
        let dexes: HashSet<&str> = group.iter().map(|p| p.dex.as_str()).collect();
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

/// Enumerate every 2-leg cross-venue arb candidate from the registry.
///
/// For each `(pair, fee_pips)` group of pools spanning ≥ 2 DEXs, every ordered
/// pair `(A, B)` where `dex(A) ≠ dex(B)` becomes one or more candidates — one
/// per flash-loanable borrow token that participates in the pair. The borrow
/// token determines `zero_for_one` for both legs.
fn build_candidates(cfg: &Config, registry: &PoolRegistry) -> Result<Vec<Candidate>> {
    let flashloan: HashSet<Address> = cfg
        .aave
        .flashloan_assets
        .iter()
        .map(|s| s.parse::<Address>().context("flashloan_asset parse"))
        .collect::<Result<_>>()?;

    let symbol_by_addr: HashMap<Address, &str> = cfg
        .tokens
        .iter()
        .filter_map(|t| {
            t.address
                .parse::<Address>()
                .ok()
                .map(|a| (a, t.symbol.as_str()))
        })
        .collect();

    // Group by (pair, fee).
    let mut groups: HashMap<(&str, u32), Vec<&V3PoolMeta>> = HashMap::new();
    for meta in registry.meta().values() {
        groups
            .entry((meta.pair.as_str(), meta.fee_pips))
            .or_default()
            .push(meta);
    }

    let mut out: Vec<Candidate> = Vec::new();
    for ((pair, fee_pips), pools) in groups {
        if pools.len() < 2 {
            continue;
        }
        let dexes: HashSet<&str> = pools.iter().map(|m| m.dex.as_str()).collect();
        if dexes.len() < 2 {
            continue;
        }

        for a in &pools {
            for b in &pools {
                if a.addr == b.addr || a.dex == b.dex {
                    continue;
                }
                // Only one direction per ordered pool pair (a → b); the reverse
                // direction is captured when we hit (b, a) on the next iteration.
                let pool_a_token0 = a.token0;
                let pool_a_token1 = a.token1;
                let pool_b_token0 = b.token0;
                let pool_b_token1 = b.token1;

                if (pool_a_token0, pool_a_token1) != (pool_b_token0, pool_b_token1) {
                    // Same pair label but different on-chain token0/token1 ordering —
                    // shouldn't happen for sanely-built configs, but skip to be safe.
                    warn!(
                        pair = %pair,
                        pool_a = %a.addr,
                        pool_b = %b.addr,
                        "token0/token1 ordering mismatch — skipping candidate"
                    );
                    continue;
                }

                for borrow in [pool_a_token0, pool_a_token1] {
                    if !flashloan.contains(&borrow) {
                        continue;
                    }
                    let borrow_decimals = if borrow == pool_a_token0 {
                        a.decimals0
                    } else {
                        a.decimals1
                    };
                    // z4o on leg A: true if we're selling token0.
                    let z4o_a = borrow == pool_a_token0;
                    // Leg B receives the opposite of borrow and sells it.
                    let z4o_b = !z4o_a;

                    let symbol = symbol_by_addr.get(&borrow).copied().unwrap_or("?");
                    let id = format!(
                        "{pair}@{fee}bps:{da}->{db}:borrow={sym}",
                        pair = pair,
                        fee = fee_pips / 100, // pips → bps
                        da = a.dex,
                        db = b.dex,
                        sym = symbol,
                    );

                    out.push(Candidate {
                        id,
                        borrow_token: borrow,
                        borrow_decimals,
                        legs: [
                            V3Leg {
                                pool: a.addr,
                                zero_for_one: z4o_a,
                                fee_pips: a.fee_pips,
                            },
                            V3Leg {
                                pool: b.addr,
                                zero_for_one: z4o_b,
                                fee_pips: b.fee_pips,
                            },
                        ],
                    });
                }
            }
        }
    }

    out.sort_by(|x, y| x.id.cmp(&y.id));
    Ok(out)
}

fn detector_config_from(risk: &RiskConfig) -> DetectorConfig {
    DetectorConfig {
        loan_sizes_usd: vec![100, 500, 1000],
        aave_premium_bps: 5,
        gas_cost_usd_micros: 10_000, // $0.01 — MegaETH is cheap
        min_profit_usd_micros: (risk.min_profit_usd * 1_000_000.0) as u64,
        safety_margin_bps: 50,
    }
}

/// Emit one `tracing` event for an `OpportunityEvent`. The JSON subscriber
/// serializes structured fields; consumers parse `event` to dispatch.
fn emit_event(ev: OpportunityEvent) {
    match ev {
        OpportunityEvent::Opened { id, ts_ms, quote } => {
            let net_profit_usd = quote.net_profit_usd_micros as f64 / 1_000_000.0;
            let loan_usd = micros_to_usd_f64(quote.loan_wei, decimals_from_loan_id(&id));
            tracing::info!(
                target: "opportunity",
                event = "opened",
                id = %id,
                ts_ms = ts_ms,
                block = quote.block_number,
                loan_usd = loan_usd,
                gross_edge_bps = quote.gross_edge_bps,
                net_profit_usd = net_profit_usd,
                "opportunity opened"
            );
            metrics::counter!("opportunities_opened_total").increment(1);
        }
        OpportunityEvent::Closed {
            id,
            ts_ms,
            lifetime_ms,
            peak_profit_usd_micros,
            mean_profit_usd_micros,
            samples,
        } => {
            let peak_usd = peak_profit_usd_micros as f64 / 1_000_000.0;
            let mean_usd = mean_profit_usd_micros as f64 / 1_000_000.0;
            tracing::info!(
                target: "opportunity",
                event = "closed",
                id = %id,
                ts_ms = ts_ms,
                lifetime_ms = lifetime_ms,
                samples = samples,
                peak_profit_usd = peak_usd,
                mean_profit_usd = mean_usd,
                "opportunity closed"
            );
            metrics::counter!("opportunities_closed_total").increment(1);
            metrics::histogram!("opportunity_lifetime_ms").record(lifetime_ms as f64);
        }
    }
}

/// Conservative `loan_wei → loan_usd` rendering at logging time. The loan
/// amount in the quote is in borrow-token wei; we recover decimals from the
/// candidate id's `borrow=` suffix instead of plumbing through extra state.
fn decimals_from_loan_id(id: &str) -> u8 {
    // `id` ends with `:borrow=<symbol>`. We treat USDT0/USDe as 6dp and
    // anything else as 18dp. This only affects the rendered loan_usd field.
    if id.ends_with("=USDT0") || id.ends_with("=USDe") {
        6
    } else {
        18
    }
}

fn micros_to_usd_f64(loan_wei: alloy::primitives::U256, decimals: u8) -> f64 {
    use alloy::primitives::U256;
    let pow = U256::from(10u64).pow(U256::from(decimals));
    if pow.is_zero() {
        return 0.0;
    }
    let whole = (loan_wei / pow).to::<u64>() as f64;
    let frac_part = loan_wei % pow;
    // Truncate fractional contribution — for whole-dollar loan sizes it's zero.
    let _ = frac_part;
    whole
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
        flashloan_assets = cfg.aave.flashloan_assets.len(),
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

    let candidates = build_candidates(&cfg, &registry)?;
    info!(count = candidates.len(), "detector candidates built");
    if candidates.is_empty() {
        warn!("no cross-venue candidates configured — exiting");
        return Ok(());
    }
    let det_cfg = detector_config_from(&cfg.risk);

    let (block_tx, mut block_rx) = watch::channel(0u64);
    let poller = PoolPoller::new(
        provider,
        registry.clone(),
        Duration::from_millis(cli.poll_ms),
        block_tx,
    );

    info!("starting pool poller");
    let poller_handle = tokio::spawn(async move { poller.run().await });

    let mut tracker = OpportunityTracker::new();
    let mut results: Vec<(String, Option<searcher_core::OpportunityQuote>)> =
        Vec::with_capacity(candidates.len());

    loop {
        if block_rx.changed().await.is_err() {
            warn!("block_rx closed — poller exited");
            break;
        }
        let block = *block_rx.borrow();
        if block == 0 {
            continue; // initial value before first tick
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        results.clear();
        let mut skipped_stale = 0usize;
        for cand in &candidates {
            let s_a_opt = registry.get(&cand.legs[0].pool);
            let s_b_opt = registry.get(&cand.legs[1].pool);
            let (Some(s_a), Some(s_b)) = (s_a_opt, s_b_opt) else {
                continue;
            };
            if s_a.block_number != block || s_b.block_number != block {
                skipped_stale += 1;
                continue;
            }
            let view_a = PoolStateView {
                sqrt_price_x96: s_a.sqrt_price_x96,
                liquidity: s_a.liquidity,
            };
            let view_b = PoolStateView {
                sqrt_price_x96: s_b.sqrt_price_x96,
                liquidity: s_b.liquidity,
            };
            match evaluate_candidate(cand, &view_a, &view_b, &det_cfg, block) {
                Ok(quote) => results.push((cand.id.clone(), quote)),
                Err(e) => {
                    debug!(id = %cand.id, error = %e, "evaluate_candidate failed");
                    metrics::counter!("detector_eval_errors_total").increment(1);
                }
            }
        }
        if skipped_stale > 0 {
            metrics::counter!("detector_skipped_stale_total").increment(skipped_stale as u64);
            debug!(skipped_stale, block, "candidates skipped (stale state)");
        }
        metrics::gauge!("detector_open_opportunities").set(tracker.open_count() as f64);

        let events = tracker.record(std::mem::take(&mut results), now_ms);
        for ev in events {
            emit_event(ev);
        }
    }

    poller_handle.abort();
    Ok(())
}
