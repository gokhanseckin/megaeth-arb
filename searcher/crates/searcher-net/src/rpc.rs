//! HTTP/2 RPC client for `eth_sendRawTransaction` and queries.
//!
//! Phase 0 stub — multi-endpoint racing lands in Phase 3.

use anyhow::Result;

#[derive(Clone)]
pub struct RpcClient {
    pub urls: Vec<String>,
}

impl RpcClient {
    pub fn new(urls: Vec<String>) -> Self {
        Self { urls }
    }

    /// Submit a signed raw tx, racing across configured endpoints. Phase 3.
    pub async fn send_raw(&self, _tx_bytes: &[u8]) -> Result<alloy_primitives::B256> {
        anyhow::bail!("send_raw not implemented yet (phase 3)")
    }
}
