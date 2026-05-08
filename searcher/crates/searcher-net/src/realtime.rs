//! MegaETH Realtime API WS client.
//!
//! Subscribes to state diffs touching a watched address set and emits parsed
//! events to the rest of the bot. Reconnects with exponential backoff on drop.

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum RealtimeError {
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("stream gap detected (sequence jumped)")]
    Gap,
}

/// One state-diff event for a watched contract.
#[derive(Debug, Clone, Deserialize)]
pub struct StateDiff {
    pub address: alloy_primitives::Address,
    /// (slot, value) pairs that changed in this diff.
    pub changes: Vec<(alloy_primitives::B256, alloy_primitives::B256)>,
    pub mini_block: u64,
}

/// Client interface — implementation lands in Phase 2 once the auth/URL
/// shape is confirmed against `docs.megaeth.com/realtime-api`.
pub struct RealtimeClient {
    url: String,
    watched: Vec<alloy_primitives::Address>,
}

impl RealtimeClient {
    pub fn new(url: impl Into<String>, watched: Vec<alloy_primitives::Address>) -> Self {
        Self {
            url: url.into(),
            watched,
        }
    }

    /// Begin streaming diffs into `tx`. Returns when the channel is closed
    /// or after exhausting reconnect attempts.
    pub async fn run(self, _tx: mpsc::Sender<StateDiff>) -> Result<(), RealtimeError> {
        // TODO(phase-2): connect, subscribe to state-diff topic for `watched`,
        // parse frames into StateDiff, exponential-backoff reconnect, gap detection.
        let _ = (&self.url, &self.watched);
        Ok(())
    }
}
