//! Polling-based V3 pool state ingestion.
//!
//! Every tick:
//!   1. fetch the head block,
//!   2. issue one batched `eth_call` packet covering `slot0()` and
//!      `liquidity()` for every watched pool, all pinned to that block,
//!   3. write decoded state into [`PoolRegistry`] and bump the watermark.
//!
//! Block pinning is non-negotiable: MegaETH's 10ms mini-blocks otherwise
//! drift the pool state mid-batch and surface as ghost mismatches between
//! sqrtPrice and liquidity at the detector.

use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::client::{BatchRequest, Waiter};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use alloy::transports::Transport;
use anyhow::{Context, Result};
use searcher_pools::{PoolRegistry, V3PoolState};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{debug, error, warn};

use crate::abis::IUniswapV3Pool;

/// Raw EthCall params shape — `[tx, blockId]`. Serialize as a 2-elem tuple
/// (matches alloy's `EthCallParams` serialization).
#[derive(Debug, Clone, serde::Serialize)]
struct EthCallReq<'a>(&'a TransactionRequest, BlockId);

pub struct PoolPoller<P, T> {
    provider: P,
    registry: Arc<PoolRegistry>,
    interval: Duration,
    block_tx: Arc<watch::Sender<u64>>,
    _t: std::marker::PhantomData<T>,
}

#[derive(Debug, Clone, Copy)]
pub struct TickStats {
    pub block: u64,
    pub pools_updated: usize,
    pub pools_failed: usize,
}

impl<P, T> PoolPoller<P, T>
where
    P: Provider<T> + Clone + 'static,
    T: Transport + Clone,
{
    pub fn new(
        provider: P,
        registry: Arc<PoolRegistry>,
        interval: Duration,
        block_tx: Arc<watch::Sender<u64>>,
    ) -> Self {
        Self {
            provider,
            registry,
            interval,
            block_tx,
            _t: std::marker::PhantomData,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Initial backoff: doubles up to MAX_BACKOFF_MS on consecutive failures.
        const MIN_BACKOFF_MS: u64 = 200;
        const MAX_BACKOFF_MS: u64 = 5_000;
        let mut backoff_ms = MIN_BACKOFF_MS;
        let mut consecutive_failures: u32 = 0;
        let mut last_tick = Instant::now();

        loop {
            let result = self.tick().await;
            match result {
                Ok(stats) => {
                    consecutive_failures = 0;
                    backoff_ms = MIN_BACKOFF_MS;
                    metrics::counter!("pool_poller_ticks_total").increment(1);
                    metrics::gauge!("pool_poller_last_block").set(stats.block as f64);
                    metrics::gauge!("pool_poller_pools_failed").set(stats.pools_failed as f64);
                    debug!(
                        block = stats.block,
                        updated = stats.pools_updated,
                        failed = stats.pools_failed,
                        "pool_poll_tick"
                    );
                }
                Err(e) => {
                    consecutive_failures += 1;
                    metrics::counter!("pool_poller_failures_total").increment(1);
                    metrics::gauge!("pool_poller_consecutive_failures")
                        .set(consecutive_failures as f64);
                    error!(
                        error = %format!("{e:#}"),
                        consecutive_failures,
                        backoff_ms,
                        "pool_poll_tick_failed"
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms.saturating_mul(2)).min(MAX_BACKOFF_MS);
                    last_tick = Instant::now();
                    continue;
                }
            }

            // Steady-state cadence: sleep the remainder of `interval` since the
            // tick started. If the tick took longer than `interval`, run again
            // immediately.
            let elapsed = last_tick.elapsed();
            if elapsed < self.interval {
                tokio::time::sleep(self.interval - elapsed).await;
            }
            last_tick = Instant::now();
        }
    }

    /// One polling tick. Returns the count of pools updated vs. failed for the
    /// pinned block. Whole-batch errors propagate out; per-pool errors are
    /// logged and the prior cached state is retained.
    pub async fn tick(&self) -> Result<TickStats> {
        let head: u64 = self
            .provider
            .get_block_number()
            .await
            .context("get_block_number")?;

        let prev_block = self.registry.last_block();
        if head < prev_block {
            anyhow::bail!(
                "block went backwards: head={head} < last_block={prev_block} (sequencer issue?)"
            );
        }

        let block_id = BlockId::Number(BlockNumberOrTag::Number(head));

        // Build all txs up front — `EthCallReq` borrows them, so they must
        // outlive the batch.
        let pool_addrs: Vec<Address> = self.registry.meta().keys().copied().collect();
        if pool_addrs.is_empty() {
            return Ok(TickStats {
                block: head,
                pools_updated: 0,
                pools_failed: 0,
            });
        }

        let mut txs: Vec<(TransactionRequest, TransactionRequest)> =
            Vec::with_capacity(pool_addrs.len());
        for addr in &pool_addrs {
            let slot0_data = IUniswapV3Pool::slot0Call {}.abi_encode();
            let liq_data = IUniswapV3Pool::liquidityCall {}.abi_encode();
            let tx_slot = TransactionRequest::default()
                .with_to(*addr)
                .with_input(Bytes::from(slot0_data));
            let tx_liq = TransactionRequest::default()
                .with_to(*addr)
                .with_input(Bytes::from(liq_data));
            txs.push((tx_slot, tx_liq));
        }

        let client = self.provider.client();
        let mut batch = BatchRequest::new(client);

        let mut waiters: Vec<(Waiter<Bytes>, Waiter<Bytes>)> = Vec::with_capacity(pool_addrs.len());
        for (tx_slot, tx_liq) in &txs {
            let w_slot = batch
                .add_call::<_, Bytes>("eth_call", &EthCallReq(tx_slot, block_id))
                .context("add_call slot0")?;
            let w_liq = batch
                .add_call::<_, Bytes>("eth_call", &EthCallReq(tx_liq, block_id))
                .context("add_call liquidity")?;
            waiters.push((w_slot, w_liq));
        }

        // Drive the batch.
        batch.send().await.context("batch send")?;

        let mut updated = 0usize;
        let mut failed = 0usize;
        for (addr, (w_slot, w_liq)) in pool_addrs.iter().zip(waiters) {
            let slot_bytes = match w_slot.await {
                Ok(b) => b,
                Err(e) => {
                    warn!(pool = %addr, kind = "slot0", error = %e, "stale state — call failed");
                    failed += 1;
                    continue;
                }
            };
            let liq_bytes = match w_liq.await {
                Ok(b) => b,
                Err(e) => {
                    warn!(pool = %addr, kind = "liquidity", error = %e, "stale state — call failed");
                    failed += 1;
                    continue;
                }
            };

            let slot0 =
                match IUniswapV3Pool::slot0Call::abi_decode_returns(slot_bytes.as_ref(), false) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(pool = %addr, error = %e, "slot0 decode failed");
                        failed += 1;
                        continue;
                    }
                };
            let liq = match IUniswapV3Pool::liquidityCall::abi_decode_returns(
                liq_bytes.as_ref(),
                false,
            ) {
                Ok(l) => l,
                Err(e) => {
                    warn!(pool = %addr, error = %e, "liquidity decode failed");
                    failed += 1;
                    continue;
                }
            };

            let state = V3PoolState {
                sqrt_price_x96: U256::from(slot0.sqrtPriceX96),
                liquidity: liq._0,
                tick: slot0.tick.as_i32(),
                block_number: head,
            };
            self.registry.set(*addr, state);
            updated += 1;
        }

        // Bump the watermark only after every per-pool write for this block has
        // landed — release ordering pairs with the detector's acquire load.
        self.registry.set_last_block(head);
        // Best-effort; receivers may have dropped.
        let _ = self.block_tx.send(head);

        Ok(TickStats {
            block: head,
            pools_updated: updated,
            pools_failed: failed,
        })
    }
}
