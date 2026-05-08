//! V3 pool state and static metadata.

use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// Live state of a V3 pool, refreshed by the poller.
///
/// `block_number` is the block at which the read was pinned — every field in
/// this struct must come from a single `eth_call` block to stay consistent
/// with what the on-chain `pool.swap()` would return at the same block.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct V3PoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub block_number: u64,
}

/// Static config for a watched V3 pool. Loaded once at startup; never mutated.
#[derive(Debug, Clone)]
pub struct V3PoolMeta {
    pub addr: Address,
    pub dex: String,
    /// Display string from config, e.g. `"USDT0/USDm"`. Order is config-side
    /// and may not match the on-chain `(token0, token1)` ordering.
    pub pair: String,
    /// V3-native fee in hundredths of bps (100 = 1bp, 3000 = 30bp).
    pub fee_pips: u32,
    pub token0: Address,
    pub token1: Address,
    pub decimals0: u8,
    pub decimals1: u8,
}
