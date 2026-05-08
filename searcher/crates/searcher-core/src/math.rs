//! AMM swap math. MUST stay byte-equivalent to the on-chain Solidity libs.
//! See `contracts/src/libs/UniV2Math.sol` — the parity test compares both.

use alloy_primitives::U256;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MathError {
    #[error("amount in must be > 0")]
    ZeroAmountIn,
    #[error("reserves must be > 0")]
    ZeroReserve,
    #[error("fee bps must be < 10_000")]
    BadFee,
    #[error("u256 overflow")]
    Overflow,
}

/// Constant-product swap output for Uniswap V2-style pools.
///
/// Mirror of `UniV2Math.getAmountOut`:
///   amountInWithFee = amountIn * (10_000 - feeBps)
///   numerator       = amountInWithFee * reserveOut
///   denominator     = reserveIn * 10_000 + amountInWithFee
///   amountOut       = numerator / denominator
pub fn v2_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> Result<U256, MathError> {
    if amount_in.is_zero() {
        return Err(MathError::ZeroAmountIn);
    }
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(MathError::ZeroReserve);
    }
    if fee_bps >= 10_000 {
        return Err(MathError::BadFee);
    }

    let fee_complement = U256::from(10_000u32 - fee_bps);
    let amount_in_with_fee = amount_in
        .checked_mul(fee_complement)
        .ok_or(MathError::Overflow)?;
    let numerator = amount_in_with_fee
        .checked_mul(reserve_out)
        .ok_or(MathError::Overflow)?;
    let reserve_in_scaled = reserve_in
        .checked_mul(U256::from(10_000u32))
        .ok_or(MathError::Overflow)?;
    let denominator = reserve_in_scaled
        .checked_add(amount_in_with_fee)
        .ok_or(MathError::Overflow)?;

    Ok(numerator / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same vector as `UniV2MathTest::test_KnownVector_30bps` in Solidity.
    /// amountIn=1e18, reserveIn=reserveOut=10e18, fee=30bps.
    /// numerator   = (1e18 * 9970) * 10e18 = 9.97e40
    /// denominator = 10e18 * 10000 + 9.97e21 = 1.0997e23
    /// out         = 9.97e40 / 1.0997e23 ≈ 9.066109e17
    #[test]
    fn known_vector_30bps() {
        let out = v2_amount_out(
            U256::from(1_000_000_000_000_000_000u128),  // 1e18
            U256::from(10_000_000_000_000_000_000u128), // 10e18
            U256::from(10_000_000_000_000_000_000u128), // 10e18
            30,
        )
        .unwrap();
        assert_eq!(out, U256::from(906_610_893_880_149_131u128));
    }

    #[test]
    fn rejects_zero_in() {
        assert_eq!(
            v2_amount_out(U256::ZERO, U256::from(1u32), U256::from(1u32), 30),
            Err(MathError::ZeroAmountIn)
        );
    }

    #[test]
    fn rejects_zero_reserve() {
        assert_eq!(
            v2_amount_out(U256::from(1u32), U256::ZERO, U256::from(1u32), 30),
            Err(MathError::ZeroReserve)
        );
    }

    #[test]
    fn rejects_bad_fee() {
        assert_eq!(
            v2_amount_out(U256::from(1u32), U256::from(1u32), U256::from(1u32), 10_000),
            Err(MathError::BadFee)
        );
    }
}
