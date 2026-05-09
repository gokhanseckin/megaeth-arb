//! MegaETH Realtime API WebSocket subscriber.
//!
//! Subscribes to `stateChanges` for a fixed set of pool addresses, decodes the
//! storage slot diffs into [`V3PoolState`] updates, and writes them into the
//! same [`PoolRegistry`] the HTTP poller writes into. On every recognized
//! frame the shared `block_tx` watch is bumped so the detector loop wakes up.
//!
//! On (re)connect, the supplied `hydrate` closure runs once before subscribing
//! — typically it triggers an HTTP `PoolPoller::tick()` so partial slot diffs
//! arriving over WS land on top of full known state.
//!
//! Reconnect uses exponential backoff (200ms → 5s).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use searcher_pools::PoolRegistry;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::v3_storage::apply_storage_diff;

const MIN_BACKOFF_MS: u64 = 200;
const MAX_BACKOFF_MS: u64 = 5_000;

#[derive(Debug, Error)]
pub enum RealtimeError {
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("subscribe failed: {0}")]
    Subscribe(String),
}

/// Hydration callback: runs once on every (re)connect, before the
/// subscription is established. Conventionally a one-shot HTTP refresh of
/// every pool in the registry, so subsequent partial WS diffs land on a
/// known-complete baseline.
pub type HydrateFn =
    Arc<dyn Fn() -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> + Send + Sync>;

pub struct RealtimeClient {
    url: String,
    watched: Vec<Address>,
    registry: Arc<PoolRegistry>,
    block_tx: Arc<watch::Sender<u64>>,
    hydrate: HydrateFn,
}

impl RealtimeClient {
    pub fn new(
        url: impl Into<String>,
        watched: Vec<Address>,
        registry: Arc<PoolRegistry>,
        block_tx: Arc<watch::Sender<u64>>,
        hydrate: HydrateFn,
    ) -> Self {
        Self {
            url: url.into(),
            watched,
            registry,
            block_tx,
            hydrate,
        }
    }

    /// Run forever — connects, subscribes, decodes, reconnects on failure.
    /// Only returns if the watched pool set is empty (nothing to subscribe to).
    pub async fn run(self) -> Result<()> {
        if self.watched.is_empty() {
            warn!("realtime: empty watched set; not connecting");
            return Ok(());
        }
        let mut backoff_ms = MIN_BACKOFF_MS;
        loop {
            metrics::gauge!("realtime_ws_connected").set(0.0);
            match self.run_session().await {
                Ok(()) => {
                    info!("realtime ws session ended cleanly; reconnecting");
                    backoff_ms = MIN_BACKOFF_MS;
                }
                Err(e) => {
                    error!(error = %format!("{e:#}"), "realtime ws session failed");
                    metrics::counter!("realtime_ws_reconnects_total").increment(1);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = backoff_ms.saturating_mul(2).min(MAX_BACKOFF_MS);
                }
            }
        }
    }

    async fn run_session(&self) -> Result<()> {
        // Hydrate first so partial WS diffs have a baseline to merge into.
        if let Err(e) = (self.hydrate)().await {
            warn!(error = %format!("{e:#}"), "realtime hydrate failed (non-fatal); proceeding");
        }

        let (mut ws, _resp) = tokio_tungstenite::connect_async(&self.url)
            .await
            .with_context(|| format!("connect_async {}", self.url))?;
        info!(url = %self.url, watched = self.watched.len(), "realtime ws connected");
        metrics::gauge!("realtime_ws_connected").set(1.0);

        // Subscribe.
        let watched_strs: Vec<String> = self.watched.iter().map(|a| format!("{a:#x}")).collect();
        let sub = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_subscribe",
            "params": ["stateChanges", watched_strs],
        });
        ws.send(Message::Text(sub.to_string()))
            .await
            .context("send subscribe")?;

        // Read frames.
        while let Some(frame) = ws.next().await {
            let frame = frame.context("ws stream")?;
            match frame {
                Message::Text(text) => {
                    debug!(bytes = text.len(), preview = %&text.chars().take(160).collect::<String>(), "realtime frame rx");
                    if let Err(e) = self.handle_text(&text).await {
                        warn!(error = %format!("{e:#}"), preview = %&text.chars().take(200).collect::<String>(), "realtime frame decode failed");
                        metrics::counter!("realtime_decode_errors_total").increment(1);
                    }
                }
                Message::Ping(payload) => {
                    let _ = ws.send(Message::Pong(payload)).await;
                }
                Message::Close(_) => {
                    info!("realtime ws received Close; reconnecting");
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(anyhow!("realtime ws stream ended"))
    }

    async fn handle_text(&self, text: &str) -> Result<()> {
        let v: Value = serde_json::from_str(text).context("parse json")?;

        // Subscribe response (id=1 with result string) — log and continue.
        if v.get("id").is_some() {
            if let Some(err) = v.get("error") {
                return Err(RealtimeError::Subscribe(err.to_string()).into());
            }
            if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
                info!(sub_id = %result, "realtime subscription established");
            }
            return Ok(());
        }

        // Notification: {"method":"eth_subscription","params":{"subscription":..,"result":..}}
        if v.get("method").and_then(|m| m.as_str()) != Some("eth_subscription") {
            debug!(?v, "realtime: unrecognized frame");
            return Ok(());
        }

        let result = v
            .pointer("/params/result")
            .ok_or_else(|| anyhow!("missing params.result"))?;

        let payload: StateChangePayload =
            serde_json::from_value(result.clone()).context("decode StateChangePayload")?;

        self.apply_change(payload).await
    }

    async fn apply_change(&self, payload: StateChangePayload) -> Result<()> {
        let StateChangePayload {
            address,
            storage,
            block_number,
        } = payload;

        debug!(pool = %address, slots = storage.len(), block = ?block_number, "apply_change entry");

        // We only care about pools we're watching; the API should already
        // filter, but defensive in case of a sequencer-side broadcast.
        if !self.registry.meta().contains_key(&address) {
            debug!(pool = %address, "apply_change: pool not in meta (early return)");
            return Ok(());
        }

        let Some(prev) = self.registry.get(&address) else {
            // No baseline yet — wait for hydration to land.
            debug!(pool = %address, "apply_change: no baseline (un-hydrated) — skipping");
            return Ok(());
        };

        // Block number: prefer the per-frame field; fall back to the registry
        // watermark if the wire format doesn't include it.
        let block = block_number.unwrap_or_else(|| self.registry.last_block());

        // Storage values arrive as variable-length hex (e.g. "0x10" not the
        // zero-padded 64-char form). Parse with left-padding to 32 bytes.
        let mut diffs: Vec<(B256, B256)> = Vec::with_capacity(storage.len());
        for (slot_str, val_str) in storage {
            let slot = match parse_hex_b256(&slot_str) {
                Ok(s) => s,
                Err(e) => {
                    warn!(slot = %slot_str, error = %e, "ws: bad slot hex; skipping");
                    continue;
                }
            };
            let value = match parse_hex_b256(&val_str) {
                Ok(v) => v,
                Err(e) => {
                    warn!(value = %val_str, error = %e, "ws: bad value hex; skipping");
                    continue;
                }
            };
            diffs.push((slot, value));
        }
        let new_state = apply_storage_diff(prev, &diffs, block);
        self.registry.set(address, new_state);

        // Bump watermark monotonically.
        let cur = *self.block_tx.borrow();
        if block > cur {
            let _ = self.block_tx.send(block);
            self.registry.set_last_block(block);
        }

        debug!(pool = %address, block, applied_diffs = diffs.len(), "apply_change: applied");
        metrics::counter!("realtime_state_changes_total", "pool" => address.to_string())
            .increment(1);
        Ok(())
    }
}

/// Wire format of a single `stateChanges` notification result.
///
/// MegaETH's exact field name for the block number is not yet pinned by their
/// public docs; we accept both `blockNumber` and `block` (hex 0x-prefixed
/// strings) and treat absence as recoverable (fall back to registry watermark).
#[derive(Debug, Deserialize)]
struct StateChangePayload {
    address: Address,
    /// Stored as raw hex strings — values are variable-length (e.g. `"0x10"`)
    /// and the strict B256 deserializer rejects them. Padded to 32 bytes in
    /// [`RealtimeClient::apply_change`] via [`parse_hex_b256`].
    #[serde(default)]
    storage: HashMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_hex_u64_opt", alias = "block")]
    #[serde(rename = "blockNumber")]
    block_number: Option<u64>,
}

/// Parse a `0x`-prefixed hex string of any length (≤ 64 hex chars) into a
/// 32-byte `B256`, left-padding with zeros. Tolerates odd-digit values like
/// `"0x10"` that the strict B256 deserializer rejects.
fn parse_hex_b256(s: &str) -> std::result::Result<B256, String> {
    let trimmed = s.trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.len() > 64 {
        return Err(format!("hex too long: {} chars (max 64)", trimmed.len()));
    }
    let padded = format!("{:0>64}", trimmed);
    let mut buf = [0u8; 32];
    for i in 0..32 {
        buf[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("bad hex at byte {i}: {e}"))?;
    }
    Ok(B256::new(buf))
}

fn deserialize_hex_u64_opt<'de, D>(de: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(de)?;
    let Some(s) = s else { return Ok(None) };
    let trimmed = s.trim_start_matches("0x");
    u64::from_str_radix(trimmed, 16)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};
    use searcher_pools::{V3PoolMeta, V3PoolState};
    use std::collections::HashMap;

    fn make_registry(addr: Address) -> Arc<PoolRegistry> {
        let mut metas = HashMap::new();
        metas.insert(
            addr,
            V3PoolMeta {
                addr,
                dex: "kumbaya".into(),
                pair: "USDT0/USDm".into(),
                fee_pips: 100,
                token0: Address::ZERO,
                token1: Address::ZERO,
                decimals0: 6,
                decimals1: 18,
            },
        );
        Arc::new(PoolRegistry::new(metas))
    }

    #[test]
    fn parse_hex_b256_full_padded() {
        let s = "0x0000000000000000000000000000000000000000000000000000000000000004";
        assert_eq!(parse_hex_b256(s).unwrap(), B256::with_last_byte(4));
    }

    #[test]
    fn parse_hex_b256_short_even() {
        // Wire format often abbreviates: "0x04" instead of 64 chars.
        assert_eq!(parse_hex_b256("0x04").unwrap(), B256::with_last_byte(4));
    }

    #[test]
    fn parse_hex_b256_odd_digits() {
        // The bug from the live smoke test: "0x4" is 1 digit, odd.
        assert_eq!(parse_hex_b256("0x4").unwrap(), B256::with_last_byte(4));
        assert_eq!(parse_hex_b256("0x10").unwrap(), B256::with_last_byte(16));
        // Also handle a longer odd-length value like "0x100".
        let mut want = [0u8; 32];
        want[30] = 0x01;
        want[31] = 0x00;
        assert_eq!(parse_hex_b256("0x100").unwrap(), B256::new(want));
    }

    #[test]
    fn parse_hex_b256_too_long_rejected() {
        let s = format!("0x{}", "f".repeat(65));
        assert!(parse_hex_b256(&s).is_err());
    }

    #[test]
    fn parse_hex_b256_no_prefix() {
        assert_eq!(parse_hex_b256("4").unwrap(), B256::with_last_byte(4));
    }

    #[test]
    fn parse_state_change_with_block() {
        let json = r#"{
            "address": "0x6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f",
            "storage": {
                "0x0000000000000000000000000000000000000000000000000000000000000000": "0x0000000000000000000000000000000000000000010000000000000000000000"
            },
            "blockNumber": "0x10"
        }"#;
        let p: StateChangePayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.block_number, Some(16));
        assert_eq!(p.storage.len(), 1);
    }

    #[test]
    fn parse_state_change_without_block() {
        let json = r#"{
            "address": "0x6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f",
            "storage": {}
        }"#;
        let p: StateChangePayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.block_number, None);
    }

    #[tokio::test]
    async fn apply_change_updates_registry_and_advances_block() {
        let pool = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
        let registry = make_registry(pool);
        // Seed prior state — WS path requires a hydrated baseline.
        registry.set(
            pool,
            V3PoolState {
                sqrt_price_x96: U256::from(1u64) << 96,
                liquidity: 1_000_000,
                tick: 0,
                block_number: 5,
            },
        );
        registry.set_last_block(5);

        let (tx, _rx) = watch::channel(5u64);
        let block_tx = Arc::new(tx);
        let hydrate: HydrateFn = Arc::new(|| Box::pin(async { Ok(()) }));
        let client = RealtimeClient::new(
            "ws://localhost".to_string(),
            vec![pool],
            registry.clone(),
            block_tx.clone(),
            hydrate,
        );

        // Construct a payload that updates only liquidity at slot 0x04.
        // Use the wire-format string shape (variable-length hex) — apply_change
        // pads via parse_hex_b256.
        let mut storage = HashMap::new();
        storage.insert("0x4".to_string(), format!("0x{:x}", 999u128));
        let payload = StateChangePayload {
            address: pool,
            storage,
            block_number: Some(10),
        };
        client.apply_change(payload).await.unwrap();

        let s = registry.get(&pool).unwrap();
        assert_eq!(s.liquidity, 999, "liquidity decoded from slot diff");
        assert_eq!(s.sqrt_price_x96, U256::from(1u64) << 96, "sqrt preserved");
        assert_eq!(s.tick, 0, "tick preserved");
        assert_eq!(s.block_number, 10, "block advanced");
        assert_eq!(*block_tx.borrow(), 10, "watch bumped");
        assert_eq!(registry.last_block(), 10, "registry watermark bumped");
    }

    #[tokio::test]
    async fn apply_change_skips_unhydrated_pool() {
        let pool = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
        let registry = make_registry(pool);
        // Note: no registry.set() — pool is registered but not hydrated.
        let (tx, _rx) = watch::channel(0u64);
        let block_tx = Arc::new(tx);
        let hydrate: HydrateFn = Arc::new(|| Box::pin(async { Ok(()) }));
        let client = RealtimeClient::new(
            "ws://localhost".to_string(),
            vec![pool],
            registry.clone(),
            block_tx.clone(),
            hydrate,
        );

        let payload = StateChangePayload {
            address: pool,
            storage: HashMap::new(),
            block_number: Some(10),
        };
        client.apply_change(payload).await.unwrap();
        assert!(registry.get(&pool).is_none(), "no write before hydration");
        assert_eq!(*block_tx.borrow(), 0, "watch unchanged");
    }

    #[tokio::test]
    async fn apply_change_ignores_unknown_pool() {
        let known = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
        let unknown = address!("0000000000000000000000000000000000000001");
        let registry = make_registry(known);
        let (tx, _rx) = watch::channel(0u64);
        let block_tx = Arc::new(tx);
        let hydrate: HydrateFn = Arc::new(|| Box::pin(async { Ok(()) }));
        let client = RealtimeClient::new(
            "ws://localhost".to_string(),
            vec![known],
            registry.clone(),
            block_tx,
            hydrate,
        );

        let payload = StateChangePayload {
            address: unknown,
            storage: HashMap::new(),
            block_number: Some(10),
        };
        client.apply_change(payload).await.unwrap();
        assert!(registry.get(&unknown).is_none());
    }

    #[tokio::test]
    async fn apply_change_does_not_regress_block_watermark() {
        let pool = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
        let registry = make_registry(pool);
        registry.set(
            pool,
            V3PoolState {
                sqrt_price_x96: U256::from(1u64) << 96,
                liquidity: 1,
                tick: 0,
                block_number: 100,
            },
        );
        registry.set_last_block(100);
        let (tx, _rx) = watch::channel(100u64);
        let block_tx = Arc::new(tx);
        let hydrate: HydrateFn = Arc::new(|| Box::pin(async { Ok(()) }));
        let client = RealtimeClient::new(
            "ws://localhost".to_string(),
            vec![pool],
            registry.clone(),
            block_tx.clone(),
            hydrate,
        );

        // Out-of-order frame at older block.
        let payload = StateChangePayload {
            address: pool,
            storage: HashMap::new(),
            block_number: Some(50),
        };
        client.apply_change(payload).await.unwrap();
        assert_eq!(*block_tx.borrow(), 100, "watch did not regress");
        assert_eq!(registry.last_block(), 100, "registry did not regress");
    }
}
