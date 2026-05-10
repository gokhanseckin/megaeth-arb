//! Binance public market-data WS subscriber.
//!
//! Connects to the combined-stream endpoint for one or more symbols, decodes
//! `aggTrade` and `bookTicker` events, normalizes them into [`CexEvent`], and
//! pushes them onto an mpsc channel. Reconnects with exponential backoff and
//! schedules a graceful reconnect after ~23h to dodge Binance's 24h kick.
//!
//! Watch-only. No auth, no orders.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const RECONNECT_INITIAL_MS: u64 = 200;
const RECONNECT_MAX_MS: u64 = 5_000;
/// Proactive reconnect a bit before the documented 24h server-side kick.
const PROACTIVE_RECONNECT_SECS: u64 = 23 * 3600;

#[derive(Debug, Clone)]
pub struct BinanceConfig {
    pub ws_url: String,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `m=true` on Binance ⇒ buyer is the maker ⇒ aggressor is the seller.
    Sell,
    /// `m=false` ⇒ buyer is the taker ⇒ aggressor is the buyer.
    Buy,
}

#[derive(Debug, Clone)]
pub enum CexEvent {
    Trade {
        symbol: String,
        ts_ms: i64,
        agg_id: u64,
        px: f64,
        qty: f64,
        side: Side,
    },
    BookTicker {
        symbol: String,
        recv_ms: i64,
        update_id: u64,
        bid: f64,
        bid_qty: f64,
        ask: f64,
        ask_qty: f64,
    },
}

#[derive(Debug, Deserialize)]
struct CombinedFrame {
    stream: String,
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AggTradeMsg {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "a")]
    agg_id: u64,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    qty: String,
    #[serde(rename = "T")]
    trade_time_ms: i64,
    #[serde(rename = "m")]
    buyer_is_maker: bool,
}

#[derive(Debug, Deserialize)]
struct BookTickerMsg {
    #[serde(rename = "u")]
    update_id: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    bid: String,
    #[serde(rename = "B")]
    bid_qty: String,
    #[serde(rename = "a")]
    ask: String,
    #[serde(rename = "A")]
    ask_qty: String,
}

/// Build a combined-stream URL: `<ws_url>/stream?streams=ethusdt@aggTrade/ethusdt@bookTicker/...`.
fn build_combined_url(cfg: &BinanceConfig) -> Result<String> {
    if cfg.symbols.is_empty() {
        return Err(anyhow!("binance.symbols is empty"));
    }
    let mut streams: Vec<String> = Vec::with_capacity(cfg.symbols.len() * 2);
    for s in &cfg.symbols {
        let lower = s.to_lowercase();
        streams.push(format!("{lower}@aggTrade"));
        streams.push(format!("{lower}@bookTicker"));
    }
    Ok(format!(
        "{}/stream?streams={}",
        cfg.ws_url.trim_end_matches('/'),
        streams.join("/")
    ))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run the Binance subscriber. Resolves only when `tx` is closed (i.e. the
/// receiver is dropped) — otherwise it reconnects forever.
pub async fn run_binance(cfg: BinanceConfig, tx: mpsc::Sender<CexEvent>) -> Result<()> {
    let url = build_combined_url(&cfg)?;
    tracing::info!(url = %url, symbols = ?cfg.symbols, "binance subscriber starting");

    let mut backoff_ms = RECONNECT_INITIAL_MS;
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        match connect_and_pump(&url, &tx).await {
            Ok(()) => {
                tracing::info!("binance ws closed cleanly, reconnecting");
                backoff_ms = RECONNECT_INITIAL_MS;
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff_ms, "binance ws error, reconnecting");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
            }
        }
    }
}

async fn connect_and_pump(url: &str, tx: &mpsc::Sender<CexEvent>) -> Result<()> {
    let (ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("connect_async {url}"))?;
    let (mut sink, mut stream) = ws.split();
    tracing::info!("binance ws connected");

    let mut proactive = tokio::time::interval(Duration::from_secs(PROACTIVE_RECONNECT_SECS));
    proactive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately — burn it.
    proactive.tick().await;

    loop {
        tokio::select! {
            _ = proactive.tick() => {
                tracing::info!("proactive 23h reconnect");
                let _ = sink.send(Message::Close(None)).await;
                return Ok(());
            }
            msg = stream.next() => {
                let Some(msg) = msg else { return Ok(()); };
                match msg.context("ws read")? {
                    Message::Text(text) => {
                        if let Err(e) = dispatch_text(&text, tx).await {
                            tracing::warn!(error = %e, "binance frame decode failed");
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(p) => {
                        sink.send(Message::Pong(p)).await.context("pong")?;
                    }
                    Message::Pong(_) => {}
                    Message::Close(frame) => {
                        tracing::info!(?frame, "binance ws server closed");
                        return Ok(());
                    }
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

async fn dispatch_text(text: &str, tx: &mpsc::Sender<CexEvent>) -> Result<()> {
    let frame: CombinedFrame = serde_json::from_str(text).context("combined frame")?;
    if frame.stream.contains("@aggTrade") {
        let m: AggTradeMsg = serde_json::from_value(frame.data).context("aggTrade")?;
        let px: f64 = m.price.parse().context("aggTrade px")?;
        let qty: f64 = m.qty.parse().context("aggTrade qty")?;
        let side = if m.buyer_is_maker {
            Side::Sell
        } else {
            Side::Buy
        };
        let _ = tx
            .send(CexEvent::Trade {
                symbol: m.symbol,
                ts_ms: m.trade_time_ms,
                agg_id: m.agg_id,
                px,
                qty,
                side,
            })
            .await;
    } else if frame.stream.contains("@bookTicker") {
        let m: BookTickerMsg = serde_json::from_value(frame.data).context("bookTicker")?;
        let bid: f64 = m.bid.parse().context("bid")?;
        let bid_qty: f64 = m.bid_qty.parse().context("bidQty")?;
        let ask: f64 = m.ask.parse().context("ask")?;
        let ask_qty: f64 = m.ask_qty.parse().context("askQty")?;
        let _ = tx
            .send(CexEvent::BookTicker {
                symbol: m.symbol,
                recv_ms: now_ms(),
                update_id: m.update_id,
                bid,
                bid_qty,
                ask,
                ask_qty,
            })
            .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_builds_combined() {
        let cfg = BinanceConfig {
            ws_url: "wss://data-stream.binance.vision".into(),
            symbols: vec!["ETHUSDT".into(), "BTCUSDT".into()],
        };
        let url = build_combined_url(&cfg).unwrap();
        assert_eq!(
            url,
            "wss://data-stream.binance.vision/stream?streams=ethusdt@aggTrade/ethusdt@bookTicker/btcusdt@aggTrade/btcusdt@bookTicker"
        );
    }

    #[test]
    fn url_rejects_empty_symbols() {
        let cfg = BinanceConfig {
            ws_url: "wss://x".into(),
            symbols: vec![],
        };
        assert!(build_combined_url(&cfg).is_err());
    }

    #[test]
    fn agg_trade_decodes() {
        let raw = r#"{"e":"aggTrade","E":1700000000000,"s":"ETHUSDT","a":12345,"p":"3000.50","q":"0.5","f":1,"l":2,"T":1700000000123,"m":false,"M":true}"#;
        let m: AggTradeMsg = serde_json::from_str(raw).unwrap();
        assert_eq!(m.symbol, "ETHUSDT");
        assert_eq!(m.agg_id, 12345);
        assert_eq!(m.trade_time_ms, 1_700_000_000_123);
        assert!(!m.buyer_is_maker);
    }

    #[test]
    fn book_ticker_decodes() {
        let raw = r#"{"u":111,"s":"BTCUSDT","b":"60000.0","B":"1.5","a":"60001.0","A":"2.0"}"#;
        let m: BookTickerMsg = serde_json::from_str(raw).unwrap();
        assert_eq!(m.symbol, "BTCUSDT");
        assert_eq!(m.update_id, 111);
    }
}
