//! Networking: MegaETH Realtime WS subscriptions, HTTP RPC submission, and
//! polling-based V3 pool state ingestion.

pub mod abis;
pub mod poller;
pub mod realtime;
pub mod rpc;
pub mod v3_storage;

pub use poller::{PoolPoller, TickStats};
pub use v3_storage::{
    apply_storage_diff, decode_liquidity, decode_slot0, verify_layout, LIQUIDITY_KEY, SLOT0_KEY,
};
