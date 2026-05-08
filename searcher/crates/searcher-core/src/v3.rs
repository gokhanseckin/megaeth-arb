//! Uniswap V3 swap math, single-tick approximation.
//!
//! Mirrors `SwapMath.computeSwapStep` (with the tick-target case skipped),
//! `SqrtPriceMath`, and `FullMath` from <https://github.com/Uniswap/v3-core>.
//! For any swap whose input amount keeps the price inside the active tick,
//! the output is byte-exact to `pool.swap()`. The parity test in
//! `tests/v3_parity.rs` enforces that against `eth_call`-driven simulations.
//!
//! Multi-tick walking lands in a later phase — at MVP loan sizes ($100–$1k)
//! in the deep stablecoin pools we target, the active-tick assumption holds.
//!
//! All arithmetic is `U256`. Intermediate 512-bit products use `ruint::Uint<512, 8>`.

use alloy_primitives::U256;
use ruint::uint;
use thiserror::Error;

type U512 = ruint::Uint<512, 8>;

// ---- constants ---------------------------------------------------------

/// Q96 = 2^96 — Uniswap V3's price fixed-point base.
const Q96: U256 = uint!(79228162514264337593543950336_U256); // 1 << 96

/// Minimum sqrt price (exclusive) — Uniswap's `TickMath.MIN_SQRT_RATIO`.
pub const MIN_SQRT_RATIO: U256 = uint!(4295128739_U256);

/// Maximum sqrt price (exclusive) — Uniswap's `TickMath.MAX_SQRT_RATIO`.
pub const MAX_SQRT_RATIO: U256 = uint!(1461446703485210103287273052203988822378723970342_U256);

const ONE_MILLION: U256 = uint!(1000000_U256);

// ---- types -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V3Quote {
    /// How much input the pool actually consumed (`amount_in - fee_amount`).
    pub amount_in_consumed: U256,
    /// How much output the swap produced.
    pub amount_out: U256,
    /// `sqrtPriceX96` after the swap.
    pub sqrt_price_after_x96: U256,
    /// Fee retained by the pool: `amount_in - amount_in_consumed`.
    pub fee_amount: U256,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum V3Error {
    #[error("amount in must be > 0")]
    ZeroAmountIn,
    #[error("liquidity must be > 0")]
    ZeroLiquidity,
    #[error("fee pips must be < 1_000_000")]
    BadFee,
    #[error("u256 overflow during V3 math")]
    Overflow,
    #[error("u256 underflow during V3 math")]
    Underflow,
    #[error("sqrt price out of [MIN_SQRT_RATIO, MAX_SQRT_RATIO]")]
    SqrtPriceOutOfBounds,
}

// ---- public API --------------------------------------------------------

/// Single-tick V3 amount-out simulation.
///
/// Byte-exact to `pool.swap()` whenever the swap stays within the active tick.
/// Caller invariants:
/// * `amount_in > 0`, `liquidity > 0`, `fee_pips < 1_000_000`,
///   `MIN_SQRT_RATIO < sqrt_price_x96 < MAX_SQRT_RATIO`.
/// * The implied `sqrt_price_after_x96` does not cross the next initialized
///   tick. (Caller's responsibility — at MVP loan sizes in deep pools, true.)
pub fn v3_amount_out_single_tick(
    sqrt_price_x96: U256,
    liquidity: u128,
    amount_in: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<V3Quote, V3Error> {
    if amount_in.is_zero() {
        return Err(V3Error::ZeroAmountIn);
    }
    if liquidity == 0 {
        return Err(V3Error::ZeroLiquidity);
    }
    if fee_pips >= 1_000_000 {
        return Err(V3Error::BadFee);
    }
    if sqrt_price_x96 <= MIN_SQRT_RATIO || sqrt_price_x96 >= MAX_SQRT_RATIO {
        return Err(V3Error::SqrtPriceOutOfBounds);
    }

    // amount_in_less_fee = mulDiv(amount_in, 1e6 - fee_pips, 1e6) — round down.
    let one_million_less_fee = U256::from(1_000_000u32 - fee_pips);
    let amount_in_less_fee =
        mul_div(amount_in, one_million_less_fee, ONE_MILLION).ok_or(V3Error::Overflow)?;

    // sqrt_price_next = getNextSqrtPriceFromInput(...).
    let sqrt_price_next = get_next_sqrt_price_from_input(
        sqrt_price_x96,
        liquidity,
        amount_in_less_fee,
        zero_for_one,
    )?;

    // amount_in_consumed (round up) and amount_out (round down) per SwapMath.
    let (amount_in_consumed, amount_out) = if zero_for_one {
        let in_consumed = get_amount_0_delta(sqrt_price_next, sqrt_price_x96, liquidity, true)?;
        let out = get_amount_1_delta(sqrt_price_next, sqrt_price_x96, liquidity, false)?;
        (in_consumed, out)
    } else {
        let in_consumed = get_amount_1_delta(sqrt_price_x96, sqrt_price_next, liquidity, true)?;
        let out = get_amount_0_delta(sqrt_price_x96, sqrt_price_next, liquidity, false)?;
        (in_consumed, out)
    };

    let fee_amount = amount_in
        .checked_sub(amount_in_consumed)
        .ok_or(V3Error::Underflow)?;

    Ok(V3Quote {
        amount_in_consumed,
        amount_out,
        sqrt_price_after_x96: sqrt_price_next,
        fee_amount,
    })
}

// ---- FullMath ----------------------------------------------------------

/// Narrow a U512 down to U256, returning `None` if the value doesn't fit.
/// (Ruint's TryFrom across widths isn't impl'd, so we go via limbs.)
fn u512_to_u256(x: U512) -> Option<U256> {
    let limbs = x.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

/// Full 512-bit `(a * b) / denom`, rounding down. Mirrors `FullMath.mulDiv`.
fn mul_div(a: U256, b: U256, denominator: U256) -> Option<U256> {
    if denominator.is_zero() {
        return None;
    }
    let prod = U512::from(a) * U512::from(b);
    let q = prod / U512::from(denominator);
    u512_to_u256(q)
}

/// Same, rounding up. Mirrors `FullMath.mulDivRoundingUp`.
fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> Option<U256> {
    if denominator.is_zero() {
        return None;
    }
    let prod = U512::from(a) * U512::from(b);
    let denom_512 = U512::from(denominator);
    let q = prod / denom_512;
    let r = prod % denom_512;
    let q_256 = u512_to_u256(q)?;
    if r.is_zero() {
        Some(q_256)
    } else {
        q_256.checked_add(U256::from(1u8))
    }
}

/// `ceil(x / y)`. Mirrors `UnsafeMath.divRoundingUp` — both args must be > 0.
fn div_rounding_up(x: U256, y: U256) -> U256 {
    let q = x / y;
    if (x % y).is_zero() {
        q
    } else {
        q + U256::from(1u8)
    }
}

// ---- SqrtPriceMath -----------------------------------------------------

/// Mirror of `SqrtPriceMath.getNextSqrtPriceFromAmount0RoundingUp`.
///
/// For token0 input/output: the price moves down on input, up on output.
/// `add` selects input (true) vs output (false).
fn get_next_sqrt_price_from_amount_0_rounding_up(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, V3Error> {
    if amount.is_zero() {
        return Ok(sqrt_p_x96);
    }
    let numerator1: U256 = U256::from(liquidity) << 96; // L << 96 always fits in U256

    if add {
        // Try the unrolled `mulDivRoundingUp(L<<96, sqrtP, L<<96 + amount*sqrtP)` path
        // when `amount * sqrtP` doesn't overflow U256 (matches the Solidity check
        // `(amount * sqrtP) / amount == sqrtP`).
        if let Some(product) = amount.checked_mul(sqrt_p_x96) {
            if let Some(denominator) = numerator1.checked_add(product) {
                if denominator >= numerator1 {
                    return mul_div_rounding_up(numerator1, sqrt_p_x96, denominator)
                        .ok_or(V3Error::Overflow);
                }
            }
        }
        // Fallback: ceil(L<<96 / (L<<96/sqrtP + amount)) — overflow-safe.
        let inner = (numerator1 / sqrt_p_x96)
            .checked_add(amount)
            .ok_or(V3Error::Overflow)?;
        Ok(div_rounding_up(numerator1, inner))
    } else {
        let product = amount.checked_mul(sqrt_p_x96).ok_or(V3Error::Overflow)?;
        if numerator1 <= product {
            return Err(V3Error::Underflow);
        }
        let denominator = numerator1 - product;
        mul_div_rounding_up(numerator1, sqrt_p_x96, denominator).ok_or(V3Error::Overflow)
    }
}

/// Mirror of `SqrtPriceMath.getNextSqrtPriceFromAmount1RoundingDown`.
///
/// For token1 input/output: the price moves up on input, down on output.
fn get_next_sqrt_price_from_amount_1_rounding_down(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, V3Error> {
    let liquidity_u256 = U256::from(liquidity);
    let max_uint160: U256 = (U256::from(1u8) << 160) - U256::from(1u8);

    if add {
        let quotient = if amount <= max_uint160 {
            (amount << 96) / liquidity_u256
        } else {
            mul_div(amount, Q96, liquidity_u256).ok_or(V3Error::Overflow)?
        };
        sqrt_p_x96.checked_add(quotient).ok_or(V3Error::Overflow)
    } else {
        let quotient = if amount <= max_uint160 {
            div_rounding_up(amount << 96, liquidity_u256)
        } else {
            mul_div_rounding_up(amount, Q96, liquidity_u256).ok_or(V3Error::Overflow)?
        };
        if sqrt_p_x96 <= quotient {
            return Err(V3Error::Underflow);
        }
        Ok(sqrt_p_x96 - quotient)
    }
}

/// Mirror of `SqrtPriceMath.getNextSqrtPriceFromInput`.
fn get_next_sqrt_price_from_input(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256, V3Error> {
    if zero_for_one {
        get_next_sqrt_price_from_amount_0_rounding_up(sqrt_p_x96, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount_1_rounding_down(sqrt_p_x96, liquidity, amount_in, true)
    }
}

/// Mirror of `SqrtPriceMath.getAmount0Delta`.
fn get_amount_0_delta(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, V3Error> {
    let (sqrt_a, sqrt_b) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    if sqrt_a.is_zero() {
        return Err(V3Error::SqrtPriceOutOfBounds);
    }
    let numerator1: U256 = U256::from(liquidity) << 96;
    let numerator2 = sqrt_b - sqrt_a;
    if round_up {
        let inner = mul_div_rounding_up(numerator1, numerator2, sqrt_b).ok_or(V3Error::Overflow)?;
        Ok(div_rounding_up(inner, sqrt_a))
    } else {
        let inner = mul_div(numerator1, numerator2, sqrt_b).ok_or(V3Error::Overflow)?;
        Ok(inner / sqrt_a)
    }
}

/// Mirror of `SqrtPriceMath.getAmount1Delta`.
fn get_amount_1_delta(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, V3Error> {
    let (sqrt_a, sqrt_b) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    let diff = sqrt_b - sqrt_a;
    let liquidity_u256 = U256::from(liquidity);
    if round_up {
        mul_div_rounding_up(liquidity_u256, diff, Q96).ok_or(V3Error::Overflow)
    } else {
        mul_div(liquidity_u256, diff, Q96).ok_or(V3Error::Overflow)
    }
}

// ---- tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A representative point taken from a USDT0/USDm-style 1bps stablecoin pool:
    /// price ≈ 1.0 (sqrtP ≈ 2^96), L ≈ 3e21. With these we can hand-check the
    /// output for a small input.
    ///
    /// `sqrt_price_x96 = 79228162514264337593543950336` corresponds to price = 1.
    /// For amount_in = 1_000_000 (1 USDT0, 6 decimals), fee = 1bps:
    ///   amount_in_less_fee = 1_000_000 * 999900 / 1_000_000 = 999_900
    ///   With L = 3_000_000_000_000_000_000_000 (3e21), token1 (18 decimals)
    ///   output should be ~ 999_900 * 1e12 ≈ 9.999e17. We just check the
    ///   structural invariants here; byte-exact parity is the parity test.
    #[test]
    fn small_swap_produces_positive_output_at_unit_price() {
        let q = v3_amount_out_single_tick(
            Q96,
            3_000_000_000_000_000_000_000u128, // 3e21
            U256::from(1_000_000u64),          // 1 USDT0
            100,                               // 1 bps
            true,                              // sell token0 for token1
        )
        .unwrap();
        assert!(q.amount_out > U256::ZERO);
        assert!(q.amount_in_consumed <= U256::from(1_000_000u64));
        assert_eq!(
            q.fee_amount,
            U256::from(1_000_000u64) - q.amount_in_consumed
        );
        // zero_for_one ⇒ price decreases.
        assert!(q.sqrt_price_after_x96 < Q96);
    }

    #[test]
    fn one_for_zero_increases_sqrt_price() {
        let q = v3_amount_out_single_tick(
            Q96,
            3_000_000_000_000_000_000_000u128,
            U256::from(1_000_000u64),
            100,
            false, // sell token1 for token0
        )
        .unwrap();
        assert!(q.amount_out > U256::ZERO);
        assert!(q.sqrt_price_after_x96 > Q96);
    }

    #[test]
    fn rejects_zero_amount_in() {
        assert_eq!(
            v3_amount_out_single_tick(Q96, 1_000_000u128, U256::ZERO, 100, true),
            Err(V3Error::ZeroAmountIn)
        );
    }

    #[test]
    fn rejects_zero_liquidity() {
        assert_eq!(
            v3_amount_out_single_tick(Q96, 0u128, U256::from(1u64), 100, true),
            Err(V3Error::ZeroLiquidity)
        );
    }

    #[test]
    fn rejects_bad_fee() {
        assert_eq!(
            v3_amount_out_single_tick(Q96, 1_000_000u128, U256::from(1u64), 1_000_000, true),
            Err(V3Error::BadFee)
        );
    }

    #[test]
    fn rejects_sqrt_price_at_min_or_max() {
        for sqrt_p in [MIN_SQRT_RATIO, MAX_SQRT_RATIO] {
            assert_eq!(
                v3_amount_out_single_tick(sqrt_p, 1_000_000u128, U256::from(1u64), 100, true),
                Err(V3Error::SqrtPriceOutOfBounds)
            );
        }
    }

    #[test]
    fn fee_amount_complements_consumed() {
        let q = v3_amount_out_single_tick(
            Q96,
            5_000_000_000_000_000_000_000u128,
            U256::from(500_000_000u64), // 500 USDT0
            500,                        // 5 bps
            true,
        )
        .unwrap();
        assert_eq!(
            q.fee_amount + q.amount_in_consumed,
            U256::from(500_000_000u64)
        );
    }

    /// FullMath spot-check: 6 * 7 / 4 = 10 (round down), 11 (round up).
    #[test]
    fn mul_div_round_directions() {
        let a = U256::from(6u64);
        let b = U256::from(7u64);
        let d = U256::from(4u64);
        assert_eq!(mul_div(a, b, d), Some(U256::from(10u64)));
        assert_eq!(mul_div_rounding_up(a, b, d), Some(U256::from(11u64)));
    }

    /// `mul_div` for an exact division: 6 * 8 / 4 = 12, no rounding adjustment.
    #[test]
    fn mul_div_exact() {
        let a = U256::from(6u64);
        let b = U256::from(8u64);
        let d = U256::from(4u64);
        assert_eq!(mul_div(a, b, d), Some(U256::from(12u64)));
        assert_eq!(mul_div_rounding_up(a, b, d), Some(U256::from(12u64)));
    }

    /// Sample range covering 1bps, 5bps, 30bps, 100bps with deep liquidity and
    /// small inputs — the realistic MVP regime.
    fn small_input_strategy() -> impl Strategy<Value = (u128, u128, u32, bool)> {
        (
            // liquidity in [1e18, 1e25]
            1_000_000_000_000_000_000u128..=10_000_000_000_000_000_000_000_000u128,
            // amount_in in [1e3, 1e9] — keeps swap inside one tick on deep L
            1_000u128..=1_000_000_000u128,
            prop_oneof![Just(100u32), Just(500u32), Just(3000u32), Just(10000u32)],
            any::<bool>(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// With `sqrtP = Q96` (price = 1), a deep pool, and small inputs:
        ///   * fee_amount + amount_in_consumed = amount_in
        ///   * amount_in_consumed ≤ amount_in
        ///   * direction sign on sqrt_price_after is correct
        ///   * function never panics
        #[test]
        fn invariants_at_unit_price(
            (l, amt, fee_pips, z4o) in small_input_strategy()
        ) {
            let amount_in = U256::from(amt);
            let q = v3_amount_out_single_tick(Q96, l, amount_in, fee_pips, z4o).unwrap();
            prop_assert!(q.amount_in_consumed <= amount_in);
            prop_assert_eq!(q.fee_amount + q.amount_in_consumed, amount_in);
            if z4o {
                prop_assert!(q.sqrt_price_after_x96 < Q96);
            } else {
                prop_assert!(q.sqrt_price_after_x96 > Q96);
            }
        }
    }
}
