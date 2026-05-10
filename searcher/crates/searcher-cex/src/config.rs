//! TOML config for the CEX-DEX correlation watcher.
//!
//! Intentionally separate from `config/mainnet.toml` so changes here cannot
//! break the running flash-loan binary. The DEX-side pool/token registry is
//! read from the existing `mainnet.toml` via the `dex_config_path` field —
//! pool addresses are NOT duplicated.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct CexWatcherConfig {
    /// Path (relative to the cex-watcher.toml file or absolute) of the
    /// shared chain config. We re-use its `[network]`, `[[tokens]]`, and
    /// `[[pools]]` sections.
    pub dex_config_path: String,
    pub binance: BinanceSection,
    #[serde(default)]
    pub pair_map: Vec<PairMap>,
}

#[derive(Debug, Deserialize)]
pub struct BinanceSection {
    pub ws_url: String,
    #[serde(default = "default_rest_url")]
    pub rest_url: String,
    pub symbols: Vec<String>,
    /// Aggregated trades below this notional are still streamed but flagged
    /// `small=true` in logs. Zero ⇒ keep everything large.
    #[serde(default = "default_min_trade_usd")]
    pub min_trade_usd: f64,
}

fn default_rest_url() -> String {
    "https://api.binance.com".into()
}
fn default_min_trade_usd() -> f64 {
    5_000.0
}

#[derive(Debug, Deserialize, Clone)]
pub struct PairMap {
    pub binance: String,
    pub dex_pair: String,
    /// Token symbol (matching `[[tokens]]` in the DEX config) that corresponds
    /// to the Binance *base* asset. e.g. for `ETHUSDT`, `base="WETH"`.
    pub base: String,
    /// Token symbol corresponding to the Binance *quote* asset. Used only as
    /// a sanity check — the price is derived from base side.
    pub quote: String,
}

/// Resolve `dex_config_path` against the directory containing the cex-watcher
/// config file.
pub fn resolve_dex_path(cex_path: &Path, dex_relative: &str) -> PathBuf {
    let p = Path::new(dex_relative);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(dir) = cex_path.parent() {
        dir.join(p)
    } else {
        p.to_path_buf()
    }
}

pub fn load_cex_config(path: &Path) -> Result<CexWatcherConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read cex-watcher config {}", path.display()))?;
    let cfg: CexWatcherConfig = toml::from_str(&text).context("parse cex-watcher.toml")?;
    if cfg.binance.symbols.is_empty() {
        return Err(anyhow!("binance.symbols is empty"));
    }
    if cfg.pair_map.is_empty() {
        return Err(anyhow!("pair_map is empty — nothing to correlate"));
    }
    Ok(cfg)
}
