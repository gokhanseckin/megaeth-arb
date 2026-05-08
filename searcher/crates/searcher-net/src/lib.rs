//! Networking: MegaETH Realtime WS subscriptions, HTTP RPC submission, and
//! polling-based V3 pool state ingestion.

pub mod abis;
pub mod poller;
pub mod realtime;
pub mod rpc;

pub use poller::{PoolPoller, TickStats};
