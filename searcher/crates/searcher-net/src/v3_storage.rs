//! V3 pool storage layout — slot decoders and a layout self-check.
//!
//! Uniswap V3 packs `Slot0` (`sqrtPriceX96` + `tick` + observation/protocol/lock
//! fields) into storage slot `0x00`, and stores `liquidity` (uint128) at slot
//! `0x04`. Realtime API push notifications arrive as `(slot, value)` pairs and
//! must be decoded back into [`V3PoolState`] fields.
//!
//! The slot index for `liquidity` depends on the order Solidity declared state
//! vars — Kumbaya/Prismfi are upstream Uniswap V3 forks but a reordered fork
//! would silently break decoding. [`verify_layout`] runs a one-shot check at
//! startup against `eth_call(liquidity())` and fails loudly on mismatch.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use alloy::transports::Transport;
use anyhow::{Context, Result};
use searcher_pools::V3PoolState;

use crate::abis::IUniswapV3Pool;

/// Storage slot holding the packed `Slot0` struct (`sqrtPriceX96`/`tick`/...).
pub const SLOT0_KEY: B256 = B256::ZERO;

/// Storage slot holding `liquidity` (uint128 in the lower 128 bits).
pub const LIQUIDITY_KEY: B256 = B256::with_last_byte(4);

/// Decode a `Slot0` storage word into `(sqrtPriceX96, tick)`.
///
/// Layout (32-byte big-endian word, lower bits at the end):
/// * bytes `[12..32]` — `sqrtPriceX96` (uint160)
/// * bytes `[9..12]` — `tick` (int24, two's complement)
///
/// We ignore the observation/feeProtocol/unlocked fields above bit 184 — the
/// detector does not consume them.
pub fn decode_slot0(value: &B256) -> (U256, i32) {
    let bytes = value.as_slice();

    let mut sqrt_buf = [0u8; 32];
    sqrt_buf[12..32].copy_from_slice(&bytes[12..32]);
    let sqrt_price = U256::from_be_bytes(sqrt_buf);

    // Sign-extend a 24-bit two's-complement value into i32.
    let raw = (u32::from(bytes[9]) << 16) | (u32::from(bytes[10]) << 8) | u32::from(bytes[11]);
    let tick = if raw & 0x80_0000 != 0 {
        (raw | 0xff00_0000) as i32
    } else {
        raw as i32
    };

    (sqrt_price, tick)
}

/// Decode the `liquidity` storage word (uint128 in the lower 16 bytes).
pub fn decode_liquidity(value: &B256) -> u128 {
    let bytes = value.as_slice();
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&bytes[16..32]);
    u128::from_be_bytes(buf)
}

/// Apply a batch of `(slot, value)` diffs to a previous state, returning the
/// updated state. Slots we do not recognize are ignored (other Uniswap V3
/// state vars — feeGrowth, ticks, observations — do not feed the detector).
///
/// `block_number` is always overwritten so the detector's freshness gate
/// works even if no recognized slots changed.
pub fn apply_storage_diff(
    prev: V3PoolState,
    diffs: &[(B256, B256)],
    block_number: u64,
) -> V3PoolState {
    let mut state = prev;
    state.block_number = block_number;
    for (slot, value) in diffs {
        if *slot == SLOT0_KEY {
            let (sqrt, tick) = decode_slot0(value);
            state.sqrt_price_x96 = sqrt;
            state.tick = tick;
        } else if *slot == LIQUIDITY_KEY {
            state.liquidity = decode_liquidity(value);
        }
    }
    state
}

/// One-shot layout sanity check: read storage slot `0x04` and call
/// `liquidity()` on the same pool, fail if they disagree. Catches forks that
/// reordered state vars before we trust slot-based decoding for that pool.
pub async fn verify_layout<P, T>(provider: &P, pool: Address) -> Result<()>
where
    P: Provider<T>,
    T: Transport + Clone,
{
    let raw_slot: U256 = provider
        .get_storage_at(pool, U256::from(4u64))
        .await
        .with_context(|| format!("eth_getStorageAt({pool}, 0x04)"))?;
    let from_slot = decode_liquidity(&B256::from(raw_slot.to_be_bytes::<32>()));

    let call_data = IUniswapV3Pool::liquidityCall {}.abi_encode();
    let tx = TransactionRequest::default()
        .with_to(pool)
        .with_input(Bytes::from(call_data));
    let raw_call = provider
        .call(&tx)
        .await
        .with_context(|| format!("eth_call liquidity() on {pool}"))?;
    let decoded = IUniswapV3Pool::liquidityCall::abi_decode_returns(raw_call.as_ref(), false)
        .with_context(|| format!("decode liquidity() on {pool}"))?;
    let from_call = decoded._0;

    if from_slot != from_call {
        anyhow::bail!(
            "V3 storage layout mismatch on pool {pool}: slot 0x04 = {from_slot}, liquidity() = {from_call} \
             (fork likely reordered state vars; decoder needs an updated LIQUIDITY_KEY)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_slot0(sqrt: U256, tick: i32) -> B256 {
        let mut b = [0u8; 32];
        let sqrt_be = sqrt.to_be_bytes::<32>();
        b[12..32].copy_from_slice(&sqrt_be[12..32]);
        let tick_be = tick.to_be_bytes();
        b[9..12].copy_from_slice(&tick_be[1..4]);
        B256::new(b)
    }

    fn pack_liquidity(liq: u128) -> B256 {
        let mut b = [0u8; 32];
        b[16..32].copy_from_slice(&liq.to_be_bytes());
        B256::new(b)
    }

    fn state(sqrt: U256, liq: u128, tick: i32, block: u64) -> V3PoolState {
        V3PoolState {
            sqrt_price_x96: sqrt,
            liquidity: liq,
            tick,
            block_number: block,
        }
    }

    #[test]
    fn decode_slot0_zero_word() {
        let (s, t) = decode_slot0(&B256::ZERO);
        assert_eq!(s, U256::ZERO);
        assert_eq!(t, 0);
    }

    #[test]
    fn decode_slot0_roundtrip_positive_tick() {
        let sqrt = U256::from(1u64) << 96;
        let tick = 12_345i32;
        let (s, t) = decode_slot0(&pack_slot0(sqrt, tick));
        assert_eq!(s, sqrt);
        assert_eq!(t, tick);
    }

    #[test]
    fn decode_slot0_negative_tick_sign_extends() {
        let sqrt = U256::from(7u64) << 96;
        let tick = -12_345i32;
        let (s, t) = decode_slot0(&pack_slot0(sqrt, tick));
        assert_eq!(s, sqrt);
        assert_eq!(t, tick);
    }

    #[test]
    fn decode_slot0_extreme_values() {
        // Max uint160, min int24.
        let sqrt = (U256::from(1u64) << 160) - U256::from(1u64);
        let tick = -(1i32 << 23);
        let (s, t) = decode_slot0(&pack_slot0(sqrt, tick));
        assert_eq!(s, sqrt);
        assert_eq!(t, tick);
        // Decoder must not let high tick bits leak into sqrtPriceX96.
        assert!(s < (U256::from(1u64) << 160));
    }

    #[test]
    fn decode_slot0_ignores_high_bits() {
        // Pack a slot0 with garbage in the observationIndex/cardinality area
        // (bits 184..240). Decoder must still recover sqrt + tick correctly.
        let sqrt = U256::from(0x1234_5678_u64) << 96;
        let tick = 42i32;
        let mut packed = pack_slot0(sqrt, tick).0;
        packed[1] = 0xab; // unlocked (bit 240) area
        packed[2] = 0xcd; // feeProtocol
        packed[3..9].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed]);
        let (s, t) = decode_slot0(&B256::new(packed));
        assert_eq!(s, sqrt);
        assert_eq!(t, tick);
    }

    #[test]
    fn decode_liquidity_roundtrip() {
        let liq = 1_234_567_890_123_456_789u128;
        assert_eq!(decode_liquidity(&pack_liquidity(liq)), liq);
    }

    #[test]
    fn decode_liquidity_max() {
        assert_eq!(decode_liquidity(&pack_liquidity(u128::MAX)), u128::MAX);
    }

    #[test]
    fn apply_diff_only_slot0_preserves_liquidity() {
        let prev = state(U256::from(100u64), 999, 10, 50);
        let new_sqrt = U256::from(1u64) << 96;
        let diffs = vec![(SLOT0_KEY, pack_slot0(new_sqrt, -1000))];
        let next = apply_storage_diff(prev, &diffs, 51);
        assert_eq!(next.sqrt_price_x96, new_sqrt);
        assert_eq!(next.tick, -1000);
        assert_eq!(next.liquidity, 999);
        assert_eq!(next.block_number, 51);
    }

    #[test]
    fn apply_diff_only_liquidity_preserves_slot0() {
        let sqrt = U256::from(1u64) << 96;
        let prev = state(sqrt, 100, 5, 10);
        let diffs = vec![(LIQUIDITY_KEY, pack_liquidity(999_999))];
        let next = apply_storage_diff(prev, &diffs, 11);
        assert_eq!(next.sqrt_price_x96, sqrt);
        assert_eq!(next.tick, 5);
        assert_eq!(next.liquidity, 999_999);
        assert_eq!(next.block_number, 11);
    }

    #[test]
    fn apply_diff_both_slots() {
        let sqrt = U256::from(1u64) << 96;
        let diffs = vec![
            (SLOT0_KEY, pack_slot0(sqrt, 100)),
            (LIQUIDITY_KEY, pack_liquidity(500)),
        ];
        let next = apply_storage_diff(state(U256::ZERO, 0, 0, 0), &diffs, 1);
        assert_eq!(next.sqrt_price_x96, sqrt);
        assert_eq!(next.tick, 100);
        assert_eq!(next.liquidity, 500);
        assert_eq!(next.block_number, 1);
    }

    #[test]
    fn apply_diff_unrelated_slot_ignored_but_block_advances() {
        let prev = state(U256::from(42u64), 7, 3, 1);
        let diffs = vec![(B256::with_last_byte(99), B256::with_last_byte(0xff))];
        let next = apply_storage_diff(prev, &diffs, 2);
        assert_eq!(next.sqrt_price_x96, U256::from(42u64));
        assert_eq!(next.liquidity, 7);
        assert_eq!(next.tick, 3);
        assert_eq!(next.block_number, 2);
    }

    #[test]
    fn apply_empty_diff_only_advances_block() {
        let prev = state(U256::from(7u64), 7, 7, 7);
        let next = apply_storage_diff(prev, &[], 8);
        assert_eq!(next.sqrt_price_x96, prev.sqrt_price_x96);
        assert_eq!(next.liquidity, prev.liquidity);
        assert_eq!(next.tick, prev.tick);
        assert_eq!(next.block_number, 8);
    }
}
