//! `cex-watcher` — Binance-side and DEX-side dual logger.
//!
//! Independent of the flash-loan `searcher` binary. Phase A: subscribe to
//! Binance aggTrade + bookTicker for configured symbols, AND poll/subscribe
//! the matching MegaETH V3 pools, emitting one structured JSON line per event
//! to three tracing targets: `cex_trade`, `cex_quote`, `dex_quote`. The
//! offline analysis decides whether the lag/lead is real before any
//! correlator code lands.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use searcher_cex::{
    load_cex_config, run_binance, sqrt_price_x96_to_price, BinanceConfig, CexEvent, PairMap, Side,
};
use searcher_net::abis::IUniswapV3Pool;
use searcher_net::realtime::{HydrateFn, RealtimeClient};
use searcher_net::PoolPoller;
use searcher_pools::{PoolRegistry, V3PoolMeta};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "cex-watcher",
    about = "Binance ↔ MegaETH DEX correlation logger"
)]
struct Cli {
    /// Path to the cex-watcher TOML config (e.g. config/cex-watcher.toml).
    #[arg(long, env = "CEX_WATCHER_CONFIG")]
    config: PathBuf,

    /// HTTP poll cadence in milliseconds for DEX state. 0 disables polling
    /// and relies on the Realtime WS subscriber alone.
    #[arg(long, default_value_t = 250)]
    poll_ms: u64,

    /// Disable Binance subscriber (DEX-only mode for debugging).
    #[arg(long, default_value_t = false)]
    no_binance: bool,

    /// Disable DEX subscriber (Binance-only mode for debugging).
    #[arg(long, default_value_t = false)]
    no_dex: bool,
}

// --- shared chain config (subset of mainnet.toml; tolerant deserializer) ---

#[derive(Debug, Deserialize)]
struct ChainConfig {
    network: NetworkConfig,
    #[serde(default)]
    tokens: Vec<TokenConfig>,
    #[serde(default)]
    pools: Vec<PoolConfig>,
}

#[derive(Debug, Deserialize)]
struct NetworkConfig {
    rpc_urls: Vec<String>,
    realtime_ws: String,
}

#[derive(Debug, Deserialize, Clone)]
struct TokenConfig {
    symbol: String,
    address: String,
    decimals: u8,
}

#[derive(Debug, Deserialize, Clone)]
struct PoolConfig {
    address: String,
    dex: String,
    pair: String,
    fee: u32,
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "cex_watcher=info,searcher_cex=info,searcher_net=info,cex_trade=info,cex_quote=info,dex_quote=info".into()
            }),
        )
        .with_target(true)
        .json()
        .init();
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn load_chain_config(path: &std::path::Path) -> Result<ChainConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read chain config {}", path.display()))?;
    let mut cfg: ChainConfig = toml::from_str(&text).context("parse chain config")?;
    cfg.network.rpc_urls = cfg
        .network
        .rpc_urls
        .iter()
        .map(|u| {
            shellexpand::env(u)
                .map(|s| s.into_owned())
                .map_err(|e| anyhow!("env expand {u:?}: {e}"))
        })
        .collect::<Result<Vec<_>>>()?;
    cfg.network.realtime_ws = shellexpand::env(&cfg.network.realtime_ws)
        .map(|s| s.into_owned())
        .map_err(|e| anyhow!("env expand realtime_ws: {e}"))?;
    Ok(cfg)
}

/// Build a registry containing only the pools whose `pair` matches one of the
/// configured `pair_map` entries. Mirrors `searcher-bin::build_registry` but
/// scoped tighter — we don't want to pay token0/token1 RPC roundtrips for the
/// 50+ pools the flash-loan watcher cares about.
async fn build_dex_registry<P, T>(
    provider: &P,
    chain: &ChainConfig,
    pair_map: &[PairMap],
) -> Result<Arc<PoolRegistry>>
where
    P: Provider<T>,
    T: alloy::transports::Transport + Clone,
{
    let token_by_symbol: HashMap<&str, &TokenConfig> = chain
        .tokens
        .iter()
        .map(|t| (t.symbol.as_str(), t))
        .collect();
    let resolve = |sym: &str| -> Option<&TokenConfig> {
        if let Some(t) = token_by_symbol.get(sym).copied() {
            return Some(t);
        }
        let stripped: String = sym.chars().filter(|c| *c != '.').collect();
        token_by_symbol.get(stripped.as_str()).copied()
    };

    let wanted_pairs: std::collections::HashSet<&str> =
        pair_map.iter().map(|m| m.dex_pair.as_str()).collect();

    let mut meta_map: HashMap<Address, V3PoolMeta> = HashMap::new();
    for p in chain
        .pools
        .iter()
        .filter(|p| wanted_pairs.contains(p.pair.as_str()))
    {
        let addr: Address = p.address.parse().context("pool address parse")?;
        let symbols: Vec<&str> = p.pair.split('/').collect();
        if symbols.len() != 2 {
            return Err(anyhow!("pair {} not 'X/Y'", p.pair));
        }
        let (Some(t_a), Some(t_b)) = (resolve(symbols[0]), resolve(symbols[1])) else {
            warn!(pair = %p.pair, pool = %addr, "skipping pool — token symbol missing in [[tokens]]");
            continue;
        };
        let addr_a: Address = t_a.address.parse()?;
        let addr_b: Address = t_b.address.parse()?;

        let pool = IUniswapV3Pool::new(addr, provider);
        let token0 = pool
            .token0()
            .call()
            .await
            .with_context(|| format!("token0 {addr}"))?
            ._0;
        let token1 = pool
            .token1()
            .call()
            .await
            .with_context(|| format!("token1 {addr}"))?
            ._0;
        let (decimals0, decimals1) = if token0 == addr_a && token1 == addr_b {
            (t_a.decimals, t_b.decimals)
        } else if token0 == addr_b && token1 == addr_a {
            (t_b.decimals, t_a.decimals)
        } else {
            return Err(anyhow!(
                "on-chain (token0,token1) mismatch for pool {addr} pair {}",
                p.pair
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

    if meta_map.is_empty() {
        return Err(anyhow!(
            "no pools matched any pair_map entry — check dex_pair strings"
        ));
    }
    Ok(Arc::new(PoolRegistry::new(meta_map)))
}

/// Resolve, per pool, which on-chain address is the "base" asset of the
/// matching Binance symbol — used to render `quote per base` prices.
fn build_pool_pricing(
    registry: &PoolRegistry,
    chain: &ChainConfig,
    pair_map: &[PairMap],
) -> Result<HashMap<Address, (String, Address)>> {
    // Map: pool addr → (binance_symbol, base on-chain address).
    let token_addr_by_symbol: HashMap<&str, Address> = chain
        .tokens
        .iter()
        .filter_map(|t| {
            t.address
                .parse::<Address>()
                .ok()
                .map(|a| (t.symbol.as_str(), a))
        })
        .collect();

    let mut map = HashMap::new();
    for (addr, meta) in registry.meta() {
        let Some(pm) = pair_map.iter().find(|m| m.dex_pair == meta.pair) else {
            continue;
        };
        let Some(base_addr) = token_addr_by_symbol.get(pm.base.as_str()).copied() else {
            return Err(anyhow!("pair_map base {} not found in [[tokens]]", pm.base));
        };
        if base_addr != meta.token0 && base_addr != meta.token1 {
            return Err(anyhow!(
                "pair_map base {} not in pool {} on-chain tokens",
                pm.base,
                addr
            ));
        }
        map.insert(*addr, (pm.binance.clone(), base_addr));
    }
    Ok(map)
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cex_cfg = load_cex_config(&cli.config)?;
    let dex_path = searcher_cex::config::resolve_dex_path(&cli.config, &cex_cfg.dex_config_path);
    info!(
        cex_config = %cli.config.display(),
        dex_config = %dex_path.display(),
        symbols = ?cex_cfg.binance.symbols,
        pair_map = cex_cfg.pair_map.len(),
        "cex-watcher booted"
    );
    if cli.no_binance && cli.no_dex {
        return Err(anyhow!("--no-binance and --no-dex set — nothing to do"));
    }
    // Chain config only matters for the DEX side. Skip loading (and thus
    // env-var expansion of MEGAETH_RPC_KEY) when running Binance-only.
    let chain = if !cli.no_dex {
        Some(load_chain_config(&dex_path)?)
    } else {
        None
    };

    let (cex_tx, mut cex_rx) = mpsc::channel::<CexEvent>(4096);
    let mut binance_handle = None;
    if !cli.no_binance {
        let bcfg = BinanceConfig {
            ws_url: cex_cfg.binance.ws_url.clone(),
            symbols: cex_cfg.binance.symbols.clone(),
        };
        binance_handle = Some(tokio::spawn(async move {
            if let Err(e) = run_binance(bcfg, cex_tx).await {
                warn!(error = %e, "binance subscriber exited");
            }
        }));
    }

    // CEX event consumer: log every aggTrade and bookTicker.
    let min_trade_usd = cex_cfg.binance.min_trade_usd;
    let cex_consumer = tokio::spawn(async move {
        while let Some(ev) = cex_rx.recv().await {
            match ev {
                CexEvent::Trade {
                    symbol,
                    ts_ms,
                    agg_id,
                    px,
                    qty,
                    side,
                } => {
                    let notional = px * qty;
                    let small = notional < min_trade_usd;
                    let side_s = match side {
                        Side::Buy => "buy",
                        Side::Sell => "sell",
                    };
                    tracing::info!(
                        target: "cex_trade",
                        event = "trade",
                        symbol = %symbol,
                        ts_ms = ts_ms,
                        agg_id = agg_id,
                        px = px,
                        qty = qty,
                        notional_usd = notional,
                        side = side_s,
                        small = small,
                        "binance aggTrade"
                    );
                }
                CexEvent::BookTicker {
                    symbol,
                    recv_ms,
                    update_id,
                    bid,
                    ask,
                    bid_qty,
                    ask_qty,
                } => {
                    let mid = (bid + ask) / 2.0;
                    tracing::info!(
                        target: "cex_quote",
                        event = "book_ticker",
                        symbol = %symbol,
                        recv_ms = recv_ms,
                        update_id = update_id,
                        bid = bid,
                        ask = ask,
                        bid_qty = bid_qty,
                        ask_qty = ask_qty,
                        mid = mid,
                        "binance bookTicker"
                    );
                }
            }
        }
    });

    let dex_handle = if let Some(chain) = chain {
        let pair_map = cex_cfg.pair_map.clone();
        let realtime_ws = chain.network.realtime_ws.clone();
        let rpc_url = chain
            .network
            .rpc_urls
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("no rpc_urls in chain config"))?;
        let poll_ms = cli.poll_ms;
        Some(tokio::spawn(async move {
            if let Err(e) = run_dex_loop(rpc_url, realtime_ws, chain, pair_map, poll_ms).await {
                warn!(error = %e, "dex loop exited");
            }
        }))
    } else {
        None
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("shutting down"); }
        _ = cex_consumer => { warn!("cex consumer exited"); }
    }
    if let Some(h) = binance_handle {
        h.abort();
    }
    if let Some(h) = dex_handle {
        h.abort();
    }
    Ok(())
}

async fn run_dex_loop(
    rpc_url: String,
    realtime_ws: String,
    chain: ChainConfig,
    pair_map: Vec<PairMap>,
    poll_ms: u64,
) -> Result<()> {
    let provider = ProviderBuilder::new().on_http(rpc_url.parse().context("rpc url")?);
    let registry = build_dex_registry(&provider, &chain, &pair_map).await?;
    info!(pools = registry.pool_count(), "dex pool registry built");
    let pricing = build_pool_pricing(&registry, &chain, &pair_map)?;

    let (block_tx, mut block_rx) = watch::channel(0u64);
    let block_tx = Arc::new(block_tx);
    let poller = Arc::new(PoolPoller::new(
        provider,
        registry.clone(),
        Duration::from_millis(poll_ms.max(50)),
        block_tx.clone(),
    ));

    let _poller_handle = if poll_ms > 0 {
        info!(poll_ms, "starting dex pool poller");
        let p = poller.clone();
        Some(tokio::spawn(async move { p.run().await }))
    } else {
        None
    };

    let watched: Vec<Address> = registry.meta().keys().copied().collect();
    let hydrate: HydrateFn = {
        let p = poller.clone();
        Arc::new(move || {
            let p = p.clone();
            Box::pin(async move { p.tick().await.map(|_| ()) })
        })
    };
    let realtime = RealtimeClient::new(
        realtime_ws.clone(),
        watched,
        registry.clone(),
        block_tx.clone(),
        hydrate,
    );
    let _ws_handle = tokio::spawn(async move { realtime.run().await });

    info!(ws = %realtime_ws, "dex realtime subscriber started");

    let mut last_px: HashMap<Address, f64> = HashMap::new();
    loop {
        if block_rx.changed().await.is_err() {
            return Err(anyhow!("dex block channel closed"));
        }
        let block = *block_rx.borrow();
        if block == 0 {
            continue;
        }
        let ts = now_ms();
        for (addr, (binance_sym, base_addr)) in &pricing {
            let Some(state) = registry.get(addr) else {
                continue;
            };
            if state.block_number != block {
                continue;
            }
            let Some(meta) = registry.meta_for(addr) else {
                continue;
            };
            let px = sqrt_price_x96_to_price(meta, state.sqrt_price_x96, *base_addr);
            if !px.is_finite() || px <= 0.0 {
                continue;
            }
            let prev = last_px.insert(*addr, px).unwrap_or(0.0);
            let move_bps = if prev > 0.0 {
                ((px - prev) / prev) * 10_000.0
            } else {
                0.0
            };
            tracing::info!(
                target: "dex_quote",
                event = "quote",
                pool = %addr,
                pair = %meta.pair,
                dex = %meta.dex,
                fee_pips = meta.fee_pips,
                binance_symbol = %binance_sym,
                block = block,
                ts_ms = ts,
                px = px,
                move_bps = move_bps,
                "dex quote"
            );
        }
    }
}
