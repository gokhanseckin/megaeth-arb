//! Lock-free pool state cache. Reads from the cache must never block a
//! state-diff applier — the hot path goes through `dashmap`'s sharded map.

use alloy_primitives::{Address, U256};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PoolKind {
    UniV2,
    UniV3,
}

#[derive(Debug, Clone)]
pub struct PoolState {
    pub kind: PoolKind,
    pub token0: Address,
    pub token1: Address,
    pub fee_bps: u32,
    /// V2 only — V3 uses `slot0` + tick data, modeled in Phase 4.
    pub reserve0: U256,
    pub reserve1: U256,
}

#[derive(Default)]
pub struct PoolRegistry {
    by_addr: DashMap<Address, PoolState>,
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, addr: Address, state: PoolState) {
        self.by_addr.insert(addr, state);
    }

    pub fn get(&self, addr: &Address) -> Option<PoolState> {
        self.by_addr.get(addr).map(|v| v.clone())
    }

    /// Apply a parsed state diff to the cached pool. Returns true if the pool
    /// is tracked. Phase 2: wire to slot→field maps for V2 reserves.
    pub fn apply_diff(&self, _addr: Address, _slot: alloy_primitives::B256, _value: alloy_primitives::B256) -> bool {
        // TODO(phase-2): map slot → field, patch reserves in place
        false
    }

    pub fn len(&self) -> usize {
        self.by_addr.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_addr.is_empty()
    }
}
