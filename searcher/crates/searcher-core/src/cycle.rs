//! Arbitrage cycle representation and profit evaluation.
//!
//! Phase 0 stub: types and a simple V2-only evaluator. Triangular and V3 support land
//! in Phase 4.

use alloy_primitives::{Address, U256};
use crate::math::v2_amount_out;

/// One leg of a cycle.
#[derive(Debug, Clone)]
pub struct Leg {
    pub pair: Address,
    pub token_in: Address,
    pub token_out: Address,
    /// Reserve of the input token in this pair, as cached by `searcher-pools`.
    pub reserve_in: U256,
    pub reserve_out: U256,
    pub fee_bps: u32,
}

#[derive(Debug, Clone)]
pub struct Cycle {
    pub legs: Vec<Leg>,
}

#[derive(Debug)]
pub struct CycleEval {
    pub amount_out: U256,
    /// gross_profit = amount_out - amount_in (saturating to zero if negative).
    pub gross_profit: U256,
    /// True if the cycle is profitable before considering Aave premium and gas.
    pub gross_profitable: bool,
}

impl Cycle {
    /// Walk every leg under V2 math; return the final output and gross delta.
    pub fn evaluate(&self, amount_in: U256) -> Result<CycleEval, crate::MathError> {
        let mut amt = amount_in;
        for leg in &self.legs {
            amt = v2_amount_out(amt, leg.reserve_in, leg.reserve_out, leg.fee_bps)?;
        }
        let gross_profit = amt.saturating_sub(amount_in);
        Ok(CycleEval {
            amount_out: amt,
            gross_profit,
            gross_profitable: amt > amount_in,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    /// Two equally-priced V2 pools — round-trip should never be profitable.
    #[test]
    fn balanced_round_trip_unprofitable() {
        let cycle = Cycle {
            legs: vec![
                Leg {
                    pair: addr(1),
                    token_in: addr(0xAA),
                    token_out: addr(0xBB),
                    reserve_in: U256::from(1_000_000_000_000_000_000_000u128), // 1000e18
                    reserve_out: U256::from(1_000_000_000_000_000_000_000u128),
                    fee_bps: 30,
                },
                Leg {
                    pair: addr(2),
                    token_in: addr(0xBB),
                    token_out: addr(0xAA),
                    reserve_in: U256::from(1_000_000_000_000_000_000_000u128),
                    reserve_out: U256::from(1_000_000_000_000_000_000_000u128),
                    fee_bps: 30,
                },
            ],
        };
        let eval = cycle.evaluate(U256::from(1_000_000_000_000_000_000u128)).unwrap();
        assert!(!eval.gross_profitable);
    }
}
