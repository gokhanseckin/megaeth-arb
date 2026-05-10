//! DEX-side quote derivation.
//!
//! Converts a V3 `sqrtPriceX96` to a human-scale price (`quote per base`),
//! given which token is the base. Watch-only; not used in execution math.

use alloy_primitives::{Address, U256};
use searcher_pools::V3PoolMeta;

#[derive(Debug, Clone)]
pub struct DexQuote {
    pub pool: Address,
    pub pair: String,
    pub dex: String,
    pub block: u64,
    pub ts_ms: i64,
    /// Quote-per-base (e.g. USDm per WETH). Approximate `f64` — fine for
    /// correlation scoring, NOT for execution.
    pub px: f64,
}

/// Convert V3 `sqrtPriceX96` into a `quote per base` price.
///
/// V3 stores `sqrtPriceX96 = sqrt(token1/token0) * 2^96` in raw token wei
/// units. We:
///   1. square it (drop the sqrt) and unscale by `2^192`,
///   2. apply the decimals delta to get the human ratio,
///   3. invert if `base` is `token1` rather than `token0`.
pub fn sqrt_price_x96_to_price(meta: &V3PoolMeta, sqrt_price_x96: U256, base: Address) -> f64 {
    if sqrt_price_x96.is_zero() {
        return 0.0;
    }
    // sp = sqrtPriceX96 / 2^96, convert to f64 via shifting down to fit.
    // U256 → f64 is lossy but adequate for correlation logging.
    let sp_f = u256_to_f64(sqrt_price_x96) / 2f64.powi(96);
    let raw = sp_f * sp_f; // token1/token0 in raw-wei terms
    let scale = 10f64.powi(meta.decimals0 as i32 - meta.decimals1 as i32);
    let token1_per_token0 = raw * scale;

    if base == meta.token0 {
        token1_per_token0
    } else if base == meta.token1 {
        if token1_per_token0 == 0.0 {
            0.0
        } else {
            1.0 / token1_per_token0
        }
    } else {
        // Caller misconfiguration — base must be one of the pool's tokens.
        f64::NAN
    }
}

fn u256_to_f64(x: U256) -> f64 {
    // alloy_primitives::U256 implements `to::<f64>()` via its limbs.
    // Use the raw limbs to avoid pulling in `ruint` features here.
    let limbs = x.into_limbs();
    let mut acc = 0f64;
    for (i, l) in limbs.iter().enumerate() {
        acc += (*l as f64) * 2f64.powi(64 * i as i32);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn meta(t0: Address, d0: u8, t1: Address, d1: u8) -> V3PoolMeta {
        V3PoolMeta {
            addr: Address::ZERO,
            dex: "kumbaya".into(),
            pair: "WETH/USDm".into(),
            fee_pips: 3000,
            token0: t0,
            decimals0: d0,
            token1: t1,
            decimals1: d1,
        }
    }

    #[test]
    fn weth_usdm_at_3000_usd() {
        // token0 = WETH (18d), token1 = USDm (6d), 1 WETH = 3000 USDm.
        // raw token1/token0 = 3000 * 10^(6-18) = 3e-9
        // sqrt(raw) = sqrt(3e-9) ≈ 5.477e-5
        // sqrtPriceX96 ≈ 5.477e-5 * 2^96
        let weth = address!("000000000000000000000000000000000000beef");
        let usdm = address!("0000000000000000000000000000000000005d3a");
        let m = meta(weth, 18, usdm, 6);

        let sp_f = (3e-9_f64).sqrt() * 2f64.powi(96);
        let sp = U256::from(sp_f as u128);
        let px = sqrt_price_x96_to_price(&m, sp, weth);
        assert!((px - 3000.0).abs() / 3000.0 < 0.01, "got {px}");
    }

    #[test]
    fn inverted_when_base_is_token1() {
        let weth = address!("000000000000000000000000000000000000beef");
        let usdm = address!("0000000000000000000000000000000000005d3a");
        // Pool ordering: token0=USDm, token1=WETH.
        let m = meta(usdm, 6, weth, 18);
        // 1 WETH = 3000 USDm ⇒ token1/token0 (raw) = (1/3000) * 10^(18-6) = 1e12/3000
        let raw = 1e12_f64 / 3000.0;
        let sp_f = raw.sqrt() * 2f64.powi(96);
        let sp = U256::from(sp_f as u128);
        let px = sqrt_price_x96_to_price(&m, sp, weth);
        assert!((px - 3000.0).abs() / 3000.0 < 0.01, "got {px}");
    }
}
