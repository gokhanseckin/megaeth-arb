//! Online smoke test for `PoolPoller`. Runs the real poller against MegaETH
//! mainnet for a few ticks against the USDT0/USDm 1bps cross-venue pair and
//! asserts:
//!   1. Both pools get at least one update.
//!   2. The registry watermark advances monotonically.
//!   3. Decoded state agrees with a direct typed `slot0()` call at the same
//!      block (sanity: the batched raw decode path matches the typed path).
//!
//! Gated `#[ignore]` like the V3 parity test — needs `MEGAETH_RPC`.
//!
//! ```bash
//! MEGAETH_RPC=https://mainnet.megaeth.com/rpc \
//!     cargo test -p searcher-net --test poller_online -- --include-ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, U256};
use alloy::providers::ProviderBuilder;
use anyhow::{Context, Result};
use searcher_net::abis::IUniswapV3Pool;
use searcher_net::PoolPoller;
use searcher_pools::{PoolRegistry, V3PoolMeta};
use tokio::sync::watch;

const KUMBAYA_USDT0_USDM: Address = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
const PRISMFI_USDT0_USDM: Address = address!("41cb3dd6824bb9c4cfc0cc4e15675f7a2e6af869");

fn meta_stub(addr: Address, dex: &str) -> V3PoolMeta {
    V3PoolMeta {
        addr,
        dex: dex.into(),
        pair: "USDT0/USDm".into(),
        fee_pips: 100,
        token0: Address::ZERO,
        token1: Address::ZERO,
        decimals0: 6,
        decimals1: 18,
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires MEGAETH_RPC; runs against live mainnet"]
async fn poller_smoke_kumbaya_prismfi_usdt0_usdm() -> Result<()> {
    let rpc = std::env::var("MEGAETH_RPC")
        .context("MEGAETH_RPC env var required for online poller smoke test")?;
    let provider = ProviderBuilder::new().on_http(rpc.parse().context("invalid MEGAETH_RPC")?);

    let mut metas = HashMap::new();
    metas.insert(KUMBAYA_USDT0_USDM, meta_stub(KUMBAYA_USDT0_USDM, "kumbaya"));
    metas.insert(PRISMFI_USDT0_USDM, meta_stub(PRISMFI_USDT0_USDM, "prismfi"));
    let registry = Arc::new(PoolRegistry::new(metas));

    let (block_tx, _rx) = watch::channel(0u64);
    let poller = PoolPoller::new(
        provider.clone(),
        registry.clone(),
        Duration::from_millis(150),
        block_tx,
    );

    // Drive 3 ticks manually so we don't depend on the run loop's sleep.
    let mut last_block = 0u64;
    for i in 0..3 {
        let stats = poller.tick().await.with_context(|| format!("tick {i}"))?;
        eprintln!("tick {i}: {:?}", stats);
        assert_eq!(stats.pools_updated, 2, "expected both pools to update");
        assert_eq!(stats.pools_failed, 0, "no pools should fail");
        assert!(
            stats.block >= last_block,
            "block went backwards: {} < {}",
            stats.block,
            last_block
        );
        last_block = stats.block;
        // small gap between ticks so we likely get a new mini-block
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(registry.last_block(), last_block);
    let kumbaya = registry
        .get(&KUMBAYA_USDT0_USDM)
        .expect("kumbaya state present");
    let prismfi = registry
        .get(&PRISMFI_USDT0_USDM)
        .expect("prismfi state present");
    eprintln!("kumbaya: {:?}", kumbaya);
    eprintln!("prismfi: {:?}", prismfi);

    // Cross-check decoded state against a typed direct call at a pinned block.
    // We pin to the registry's `last_block` so our typed call sees the same
    // state the poller's batched read saw.
    let block = BlockId::Number(BlockNumberOrTag::Number(kumbaya.block_number));
    let pool_typed = IUniswapV3Pool::new(KUMBAYA_USDT0_USDM, &provider);
    let slot0 = pool_typed.slot0().block(block).call().await?;
    let liq = pool_typed.liquidity().block(block).call().await?;
    assert_eq!(
        kumbaya.sqrt_price_x96,
        U256::from(slot0.sqrtPriceX96),
        "sqrtPriceX96 mismatch (kumbaya)"
    );
    assert_eq!(kumbaya.liquidity, liq._0, "liquidity mismatch (kumbaya)");
    assert_eq!(kumbaya.tick, slot0.tick.as_i32(), "tick mismatch (kumbaya)");
    Ok(())
}
