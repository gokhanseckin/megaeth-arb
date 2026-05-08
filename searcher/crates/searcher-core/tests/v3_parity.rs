//! Online V3 parity test against MegaETH mainnet.
//!
//! For each (amount_in, direction) sample, we compare the Rust simulation
//! `v3_amount_out_single_tick` against the chain's own `pool.swap()` semantics.
//! We get a ground-truth amount_out from the chain by:
//!   1. Loading `V3SwapProbe`'s runtime bytecode from its forge artifact.
//!   2. Calling `pool.swap(probeAddr, ...)` via `eth_call` with a `stateOverride`
//!      that injects the probe code at `probeAddr`.
//!   3. The pool calls back into the probe; the probe reverts with
//!      `abi.encode(amount0Delta, amount1Delta)`; `eth_call` returns that data.
//!   4. We decode the deltas — the negative side is `amount_out`.
//!
//! This is `#[ignore]`'d because it needs `MEGAETH_RPC` and the live pool's
//! state. Run before merge:
//!
//! ```bash
//! MEGAETH_RPC=https://mainnet.megaeth.com/rpc \
//!     cargo test -p searcher-core --test v3_parity -- --include-ignored --nocapture
//! ```
//!
//! Set `REGEN_FIXTURES=1` alongside to overwrite `tests/fixtures/v3_quotes.json`.

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{address, Address, Bytes, I256, U160, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::state::{AccountOverride, StateOverride};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{bail, Context, Result};
use searcher_core::v3_amount_out_single_tick;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Kumbaya USDT0/USDm 1bps — the primary cross-venue arb pool from CLAUDE.md.
const POOL: Address = address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f");
const POOL_FEE_PIPS: u32 = 100;
const PROBE_ADDR: Address = address!("000000000000000000000000000000000000B0BE");

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function swap(
            address recipient,
            bool zeroForOne,
            int256 amountSpecified,
            uint160 sqrtPriceLimitX96,
            bytes data
        ) external returns (int256 amount0, int256 amount1);
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Fixture {
    pool: Address,
    fee_pips: u32,
    sqrt_price_x96: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
    expected_amount_out: U256,
    expected_amount_in_consumed: U256,
    expected_sqrt_price_after_x96: U256,
}

fn load_probe_runtime_bytecode() -> Result<Bytes> {
    // CARGO_MANIFEST_DIR = .../searcher/crates/searcher-core
    // contracts root     = .../contracts
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest)
        .join("..")
        .join("..")
        .join("..")
        .join("contracts")
        .join("out")
        .join("V3SwapProbe.sol")
        .join("V3SwapProbe.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read forge artifact at {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)?;
    let hex_str = json["deployedBytecode"]["object"]
        .as_str()
        .context("deployedBytecode.object missing in artifact")?;
    let bytes = alloy::hex::decode(hex_str.trim_start_matches("0x"))
        .context("invalid hex in deployedBytecode.object")?;
    Ok(Bytes::from(bytes))
}

/// 50 deterministic amounts representing [$100, $1000] of input. The pool's
/// token decimals differ (USDT0 = 6 dec, USDm = 18 dec) so we scale by side:
/// `zero_for_one=true` sells USDT0 in 6-dec wei; `false` sells USDm in 18-dec wei.
fn deterministic_amounts_for(zero_for_one: bool) -> Vec<U256> {
    if zero_for_one {
        // 100..=1000 USDT0 in 6-dec wei
        (0..50)
            .map(|i| U256::from(100_000_000u128 + (i as u128) * 18_000_000u128))
            .collect()
    } else {
        // 100..=1000 USDm in 18-dec wei
        let base = U256::from(10u64).pow(U256::from(18u64)) * U256::from(100u64);
        let step = U256::from(10u64).pow(U256::from(18u64)) * U256::from(18u64);
        (0..50)
            .map(|i| base + step * U256::from(i as u64))
            .collect()
    }
}

async fn sim_amount_out_via_probe<P, T>(
    provider: &P,
    pool: Address,
    probe_code: &Bytes,
    zero_for_one: bool,
    amount_in: U256,
    block: BlockId,
) -> Result<U256>
where
    P: Provider<T>,
    T: alloy::transports::Transport + Clone,
{
    // Same convention as Uniswap's Quoter: cap with MIN+1 / MAX-1 so the swap
    // is bounded only by `amountSpecified`.
    let sqrt_limit_u160: U160 = if zero_for_one {
        U160::from(4_295_128_740u128) // MIN_SQRT_RATIO + 1
    } else {
        // MAX_SQRT_RATIO - 1
        "1461446703485210103287273052203988822378723970341"
            .parse::<U160>()
            .unwrap()
    };

    let amount_signed = I256::try_from(amount_in).context("amount_in too large for I256")?;

    let calldata = IUniswapV3Pool::swapCall {
        recipient: PROBE_ADDR,
        zeroForOne: zero_for_one,
        amountSpecified: amount_signed,
        sqrtPriceLimitX96: sqrt_limit_u160,
        data: Bytes::default(),
    }
    .abi_encode();

    // The pool calls back into `msg.sender`, NOT into the recipient. To route
    // the callback into our probe code we must spoof `from = PROBE_ADDR`.
    let tx = TransactionRequest::default()
        .with_from(PROBE_ADDR)
        .with_to(pool)
        .with_input(Bytes::from(calldata));

    let mut overrides = StateOverride::default();
    overrides.insert(
        PROBE_ADDR,
        AccountOverride {
            code: Some(probe_code.clone()),
            ..Default::default()
        },
    );

    match provider.call(&tx).overrides(&overrides).block(block).await {
        Ok(bytes) => bail!(
            "expected revert from probe, got success bytes: 0x{}",
            alloy::hex::encode(&bytes)
        ),
        Err(rpc_err) => {
            let revert = rpc_err
                .as_error_resp()
                .and_then(|p| p.as_revert_data())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "rpc error has no revert data — node may not surface revert reasons: {:?}",
                        rpc_err
                    )
                })?;
            if revert.len() < 64 {
                bail!(
                    "revert data too short ({} bytes); raw: 0x{}",
                    revert.len(),
                    alloy::hex::encode(&revert)
                );
            }
            let mut buf0 = [0u8; 32];
            buf0.copy_from_slice(&revert[..32]);
            let mut buf1 = [0u8; 32];
            buf1.copy_from_slice(&revert[32..64]);
            let amount0 = I256::from_be_bytes::<32>(buf0);
            let amount1 = I256::from_be_bytes::<32>(buf1);
            // Pool convention: positive = received from caller, negative = paid to recipient.
            // For exact-input zero_for_one: amount0 > 0 (input), amount1 < 0 (output).
            let amount_out = if zero_for_one {
                if !amount1.is_negative() {
                    bail!("expected amount1 < 0 for zeroForOne, got {}", amount1);
                }
                amount1.unsigned_abs()
            } else {
                if !amount0.is_negative() {
                    bail!("expected amount0 < 0 for !zeroForOne, got {}", amount0);
                }
                amount0.unsigned_abs()
            };
            Ok(amount_out)
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires MEGAETH_RPC; runs against live mainnet RPC"]
async fn v3_parity_kumbaya_usdt0_usdm_1bps() -> Result<()> {
    let rpc = std::env::var("MEGAETH_RPC")
        .context("MEGAETH_RPC env var required for online parity test")?;
    let provider = ProviderBuilder::new().on_http(rpc.parse().context("invalid MEGAETH_RPC url")?);

    let probe_code = load_probe_runtime_bytecode()?;

    // Pin every call to the same block — MegaETH mini-blocks are 10ms, so
    // without pinning the pool state drifts mid-test and parity becomes noise.
    let head = provider
        .get_block_number()
        .await
        .context("get_block_number")?;
    let block = BlockId::Number(BlockNumberOrTag::Number(head));
    eprintln!("pinned block = {head}");

    let pool_contract = IUniswapV3Pool::new(POOL, &provider);
    let slot0 = pool_contract
        .slot0()
        .block(block)
        .call()
        .await
        .context("slot0() call")?;
    let liquidity_ret = pool_contract
        .liquidity()
        .block(block)
        .call()
        .await
        .context("liquidity() call")?;

    let sqrt_price_x96: U256 = U256::from(slot0.sqrtPriceX96);
    let liquidity_u128: u128 = liquidity_ret._0;

    eprintln!(
        "pool={} sqrtPriceX96={} liquidity={} tick={}",
        POOL, sqrt_price_x96, liquidity_u128, slot0.tick
    );

    let mut fixtures = Vec::with_capacity(100);
    let mut mismatches = 0usize;

    for zero_for_one in [true, false] {
        for amount_in in deterministic_amounts_for(zero_for_one) {
            let onchain_amount_out = sim_amount_out_via_probe(
                &provider,
                POOL,
                &probe_code,
                zero_for_one,
                amount_in,
                block,
            )
            .await
            .with_context(|| format!("probe sim amount_in={} z4o={}", amount_in, zero_for_one))?;

            let rust_quote = v3_amount_out_single_tick(
                sqrt_price_x96,
                liquidity_u128,
                amount_in,
                POOL_FEE_PIPS,
                zero_for_one,
            )?;

            if rust_quote.amount_out != onchain_amount_out {
                let delta = if rust_quote.amount_out > onchain_amount_out {
                    rust_quote.amount_out - onchain_amount_out
                } else {
                    onchain_amount_out - rust_quote.amount_out
                };
                eprintln!(
                    "MISMATCH amount_in={} z4o={} rust={} chain={} delta={}",
                    amount_in, zero_for_one, rust_quote.amount_out, onchain_amount_out, delta
                );
                mismatches += 1;
            }

            fixtures.push(Fixture {
                pool: POOL,
                fee_pips: POOL_FEE_PIPS,
                sqrt_price_x96,
                liquidity: liquidity_u128,
                amount_in,
                zero_for_one,
                expected_amount_out: onchain_amount_out,
                expected_amount_in_consumed: rust_quote.amount_in_consumed,
                expected_sqrt_price_after_x96: rust_quote.sqrt_price_after_x96,
            });
        }
    }

    if std::env::var("REGEN_FIXTURES").is_ok() {
        let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("v3_quotes.json");
        std::fs::create_dir_all(out_path.parent().unwrap())?;
        std::fs::write(&out_path, serde_json::to_string_pretty(&fixtures)?)?;
        eprintln!(
            "wrote {} fixtures to {}",
            fixtures.len(),
            out_path.display()
        );
    }

    assert_eq!(
        mismatches,
        0,
        "{} parity mismatches out of {} samples",
        mismatches,
        fixtures.len()
    );
    Ok(())
}
