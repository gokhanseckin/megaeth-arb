//! `searcher-cex` — CEX↔DEX correlation watcher building blocks.
//!
//! Independent of the flash-loan watcher in `searcher-bin`. Phase A scope:
//! a Binance WS subscriber, a sqrt-price helper for deriving DEX quotes from
//! V3 state, and shared event types. The binary is in `searcher-cex-bin`.

pub mod binance;
pub mod config;
pub mod dex_quote;

pub use binance::{run_binance, BinanceConfig, CexEvent, Side};
pub use config::{load_cex_config, CexWatcherConfig, PairMap};
pub use dex_quote::{sqrt_price_x96_to_price, DexQuote};
