//! Build, sign, and submit `startArb` transactions to ArbExecutor.
//!
//! Phase 0 stub — pre-signed templates and multi-RPC racing land in Phase 3.

use alloy_primitives::{Address, U256};
use searcher_core::Cycle;

#[derive(Debug, Clone)]
pub struct ArbTxRequest {
    pub executor: Address,
    pub asset: Address,
    pub amount: U256,
    pub min_profit: U256,
    pub cycle: Cycle,
}

/// Encode `ArbExecutor.startArb(asset, amount, route, minProfit)` calldata.
/// Phase 3: actually emit the bytes via `alloy::sol!` bindings.
pub fn encode_start_arb(_req: &ArbTxRequest) -> Vec<u8> {
    // TODO(phase-3): use sol! bindings to encode legs and the outer call.
    Vec::new()
}
