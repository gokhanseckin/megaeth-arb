//! V3 pool registry: immutable per-pool metadata + lock-free live state cache.
//!
//! The poller writes into `state` keyed by pool address; the detector reads
//! from it. `last_block` is bumped after every per-pool write for a given
//! block lands, so a detector that reads `last_block == N` is guaranteed to
//! see at-least-N state for every pool in the registry.

pub mod v3_state;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::Address;
use dashmap::DashMap;

pub use v3_state::{V3PoolMeta, V3PoolState};

pub struct PoolRegistry {
    meta: HashMap<Address, V3PoolMeta>,
    state: DashMap<Address, V3PoolState>,
    last_block: AtomicU64,
}

impl PoolRegistry {
    pub fn new(meta: HashMap<Address, V3PoolMeta>) -> Self {
        Self {
            meta,
            state: DashMap::new(),
            last_block: AtomicU64::new(0),
        }
    }

    pub fn meta(&self) -> &HashMap<Address, V3PoolMeta> {
        &self.meta
    }

    pub fn meta_for(&self, addr: &Address) -> Option<&V3PoolMeta> {
        self.meta.get(addr)
    }

    pub fn get(&self, addr: &Address) -> Option<V3PoolState> {
        self.state.get(addr).map(|v| *v)
    }

    pub fn set(&self, addr: Address, state: V3PoolState) {
        self.state.insert(addr, state);
    }

    pub fn last_block(&self) -> u64 {
        self.last_block.load(Ordering::Acquire)
    }

    /// Bump the watermark. Caller must have completed all per-pool writes for
    /// `block` before calling this — release ordering pairs with the
    /// detector's acquire load on `last_block`.
    pub fn set_last_block(&self, block: u64) {
        self.last_block.store(block, Ordering::Release);
    }

    pub fn pool_count(&self) -> usize {
        self.meta.len()
    }

    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};

    fn meta(addr: Address) -> V3PoolMeta {
        V3PoolMeta {
            addr,
            dex: "kumbaya".into(),
            pair: "USDT0/USDm".into(),
            fee_pips: 100,
            token0: Address::ZERO,
            token1: Address::ZERO,
            decimals0: 6,
            decimals1: 18,
        }
    }

    #[test]
    fn set_get_roundtrip() {
        let addr = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
        let mut metas = HashMap::new();
        metas.insert(addr, meta(addr));
        let reg = PoolRegistry::new(metas);

        assert!(reg.get(&addr).is_none());
        let s = V3PoolState {
            sqrt_price_x96: U256::from(1u64) << 96,
            liquidity: 1_000_000,
            tick: 0,
            block_number: 42,
        };
        reg.set(addr, s);
        assert_eq!(reg.get(&addr), Some(s));
    }

    #[test]
    fn last_block_watermark() {
        let reg = PoolRegistry::new(HashMap::new());
        assert_eq!(reg.last_block(), 0);
        reg.set_last_block(100);
        assert_eq!(reg.last_block(), 100);
    }
}
