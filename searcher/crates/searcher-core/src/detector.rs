//! Cross-venue arbitrage opportunity detection.
//!
//! Pure: no I/O, no async, no allocations beyond the result vectors. The wiring
//! crate (`searcher-bin`) feeds in pool-state snapshots and timestamps; this
//! module returns deterministic events.
//!
//! Identity is a stable string `{pair}@{fee_bps}bps:{dex_a}->{dex_b}:{borrow}`
//! so log consumers can correlate `Opened` / `Closed` events without parsing
//! addresses.

use std::collections::HashMap;

use alloy_primitives::{Address, U256};

use crate::v3::{v3_amount_out_single_tick, V3Error};

// ---- inputs ------------------------------------------------------------

/// One swap leg of a candidate cycle.
#[derive(Debug, Clone)]
pub struct V3Leg {
    pub pool: Address,
    pub zero_for_one: bool,
    pub fee_pips: u32,
}

/// A 2-leg cross-venue cycle: borrow `borrow_token`, swap leg 0, swap leg 1, repay.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Stable, log-friendly identity. See module docs.
    pub id: String,
    pub borrow_token: Address,
    pub borrow_decimals: u8,
    pub legs: [V3Leg; 2],
}

/// Subset of `V3PoolState` that the detector needs. Defined here (not in
/// `searcher-pools`) to keep `searcher-core` a leaf crate.
#[derive(Debug, Clone, Copy)]
pub struct PoolStateView {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
}

/// Detector parameters. All USD amounts are in **USD micros**: 1 unit = $1e-6,
/// so $1.00 = 1_000_000 and $0.01 = 10_000. Integer-only on the hot path; floats
/// are for logging.
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Loan sizes to sweep, in whole USD (e.g. `[100, 500, 1000]`).
    pub loan_sizes_usd: Vec<u64>,
    /// Aave V3 flash-loan premium in basis points (5 = 0.05%).
    pub aave_premium_bps: u32,
    /// Constant gas-cost estimate, in USD micros. MegaETH is cheap; $0.01 = 10_000.
    pub gas_cost_usd_micros: u64,
    /// Minimum acceptable profit in USD micros. $1.00 = 1_000_000.
    pub min_profit_usd_micros: u64,
    /// Safety margin in basis points (50 = 0.5%) of the loan size. Effective
    /// margin is `max(min_profit, loan * margin_bps / 10_000)`.
    pub safety_margin_bps: u32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            loan_sizes_usd: vec![100, 500, 1000],
            aave_premium_bps: 5,
            gas_cost_usd_micros: 10_000,
            min_profit_usd_micros: 1_000_000,
            safety_margin_bps: 50,
        }
    }
}

// ---- outputs -----------------------------------------------------------

/// One profitable evaluation of a candidate at a specific loan size.
#[derive(Debug, Clone)]
pub struct OpportunityQuote {
    pub loan_wei: U256,
    pub gross_out_wei: U256,
    /// `(gross_out - loan) / loan` in basis points, saturating at u32::MAX.
    pub gross_edge_bps: u32,
    pub net_profit_wei: U256,
    pub net_profit_usd_micros: u64,
    pub block_number: u64,
}

#[derive(Debug, Clone)]
pub enum OpportunityEvent {
    Opened {
        id: String,
        ts_ms: u64,
        quote: OpportunityQuote,
    },
    Closed {
        id: String,
        ts_ms: u64,
        lifetime_ms: u64,
        peak_profit_usd_micros: u64,
        mean_profit_usd_micros: u64,
        samples: u32,
    },
}

// ---- evaluation --------------------------------------------------------

/// Evaluate one candidate against a snapshot of its two pools' state.
///
/// Returns `Ok(Some(q))` with the best loan-size sweep result if any size
/// clears the bar (`net_profit_wei > 0`); `Ok(None)` if none clears.
/// Errors propagate from the V3 math.
///
/// `block_number` is stamped onto the returned quote for log correlation.
pub fn evaluate_candidate(
    cand: &Candidate,
    state_a: &PoolStateView,
    state_b: &PoolStateView,
    cfg: &DetectorConfig,
    block_number: u64,
) -> Result<Option<OpportunityQuote>, V3Error> {
    let mut best: Option<OpportunityQuote> = None;

    for loan_usd in &cfg.loan_sizes_usd {
        let loan_wei = usd_whole_to_wei(*loan_usd, cand.borrow_decimals);
        if loan_wei.is_zero() {
            continue;
        }

        let leg_a = &cand.legs[0];
        let q_a = v3_amount_out_single_tick(
            state_a.sqrt_price_x96,
            state_a.liquidity,
            loan_wei,
            leg_a.fee_pips,
            leg_a.zero_for_one,
        )?;

        if q_a.amount_out.is_zero() {
            continue;
        }

        let leg_b = &cand.legs[1];
        let q_b = v3_amount_out_single_tick(
            state_b.sqrt_price_x96,
            state_b.liquidity,
            q_a.amount_out,
            leg_b.fee_pips,
            leg_b.zero_for_one,
        )?;

        // No gross profit, nothing to do.
        if q_b.amount_out <= loan_wei {
            continue;
        }
        let gross_profit = q_b.amount_out - loan_wei;

        // costs = aave premium + gas + safety margin.
        let aave_premium = mul_div_floor(
            loan_wei,
            U256::from(cfg.aave_premium_bps),
            U256::from(10_000u32),
        );
        let gas_cost_wei = usd_micros_to_wei(cfg.gas_cost_usd_micros, cand.borrow_decimals);
        let margin_from_loan = mul_div_floor(
            loan_wei,
            U256::from(cfg.safety_margin_bps),
            U256::from(10_000u32),
        );
        let margin_from_min = usd_micros_to_wei(cfg.min_profit_usd_micros, cand.borrow_decimals);
        let safety_margin = if margin_from_loan > margin_from_min {
            margin_from_loan
        } else {
            margin_from_min
        };
        let total_costs = aave_premium + gas_cost_wei + safety_margin;

        if gross_profit <= total_costs {
            continue;
        }
        let net_profit_wei = gross_profit - total_costs;

        let gross_edge_bps = bps_floor(gross_profit, loan_wei);
        let net_profit_usd_micros = wei_to_usd_micros(net_profit_wei, cand.borrow_decimals);

        let cand_q = OpportunityQuote {
            loan_wei,
            gross_out_wei: q_b.amount_out,
            gross_edge_bps,
            net_profit_wei,
            net_profit_usd_micros,
            block_number,
        };

        // Pick the loan size with the highest net profit (USD).
        match &best {
            None => best = Some(cand_q),
            Some(prev) if cand_q.net_profit_usd_micros > prev.net_profit_usd_micros => {
                best = Some(cand_q);
            }
            _ => {}
        }
    }

    Ok(best)
}

// ---- lifetime tracker --------------------------------------------------

#[derive(Debug, Clone)]
struct OpenState {
    opened_at_ms: u64,
    peak_profit_micros: u64,
    sum_profit_micros: u128,
    samples: u32,
}

/// Stateful tracker that turns per-tick `(id, Option<quote>)` results into
/// `Opened` / `Closed` events.
///
/// - `Some(q)` on a previously closed id ⇒ `Opened`
/// - `Some(q)` on an open id ⇒ update peak/mean, no event
/// - `None` on an open id ⇒ `Closed` with lifetime + peak + mean
/// - `None` on a closed id ⇒ no event
///
/// IDs absent from `results` are left alone; a candidate skipped due to stale
/// pool state at one tick should not close an opportunity that opened earlier.
#[derive(Debug, Default)]
pub struct OpportunityTracker {
    open: HashMap<String, OpenState>,
}

impl OpportunityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    pub fn record(
        &mut self,
        results: Vec<(String, Option<OpportunityQuote>)>,
        now_ms: u64,
    ) -> Vec<OpportunityEvent> {
        let mut events = Vec::new();
        for (id, maybe_quote) in results {
            match (self.open.get_mut(&id), maybe_quote) {
                (None, Some(q)) => {
                    self.open.insert(
                        id.clone(),
                        OpenState {
                            opened_at_ms: now_ms,
                            peak_profit_micros: q.net_profit_usd_micros,
                            sum_profit_micros: q.net_profit_usd_micros as u128,
                            samples: 1,
                        },
                    );
                    events.push(OpportunityEvent::Opened {
                        id,
                        ts_ms: now_ms,
                        quote: q,
                    });
                }
                (Some(s), Some(q)) => {
                    if q.net_profit_usd_micros > s.peak_profit_micros {
                        s.peak_profit_micros = q.net_profit_usd_micros;
                    }
                    s.sum_profit_micros = s
                        .sum_profit_micros
                        .saturating_add(q.net_profit_usd_micros as u128);
                    s.samples = s.samples.saturating_add(1);
                }
                (Some(_), None) => {
                    let s = self.open.remove(&id).expect("guarded by Some match");
                    let lifetime_ms = now_ms.saturating_sub(s.opened_at_ms);
                    let mean_micros = if s.samples == 0 {
                        0
                    } else {
                        (s.sum_profit_micros / s.samples as u128) as u64
                    };
                    events.push(OpportunityEvent::Closed {
                        id,
                        ts_ms: now_ms,
                        lifetime_ms,
                        peak_profit_usd_micros: s.peak_profit_micros,
                        mean_profit_usd_micros: mean_micros,
                        samples: s.samples,
                    });
                }
                (None, None) => {}
            }
        }
        events
    }
}

// ---- helpers -----------------------------------------------------------

/// `loan_usd × 10^decimals` — whole-dollar loan size to wei. Saturates only on
/// pathological decimals (>= ~77 for u64 amounts), unreachable in practice.
fn usd_whole_to_wei(loan_usd: u64, decimals: u8) -> U256 {
    let pow = pow10(decimals);
    U256::from(loan_usd).saturating_mul(pow)
}

/// `usd_micros × 10^decimals / 1_000_000` — micros are $1e-6.
fn usd_micros_to_wei(micros: u64, decimals: u8) -> U256 {
    if decimals == 6 {
        // Common shortcut for USDT0/USDe: 1 micro = 1 wei.
        return U256::from(micros);
    }
    if decimals >= 6 {
        let pow = pow10(decimals - 6);
        U256::from(micros).saturating_mul(pow)
    } else {
        let pow = pow10(6 - decimals);
        U256::from(micros) / pow
    }
}

/// `wei × 1_000_000 / 10^decimals` — saturates the result to u64.
fn wei_to_usd_micros(wei: U256, decimals: u8) -> u64 {
    let micros = if decimals == 6 {
        wei
    } else if decimals >= 6 {
        let pow = pow10(decimals - 6);
        wei / pow
    } else {
        let pow = pow10(6 - decimals);
        wei.saturating_mul(pow)
    };
    // `try_into::<u64>` would error on overflow; saturate instead so logging never panics.
    micros.to::<u128>().try_into().unwrap_or(u64::MAX)
}

/// `numerator × 10_000 / denominator` clamped to `u32::MAX`.
fn bps_floor(numerator: U256, denominator: U256) -> u32 {
    if denominator.is_zero() {
        return 0;
    }
    let bps = mul_div_floor(numerator, U256::from(10_000u32), denominator);
    bps.to::<u128>().try_into().unwrap_or(u32::MAX)
}

/// Floor multiplication-division. Inputs fit in U256 (no 512-bit promotion
/// needed at our scale: loan_wei × 10_000 stays well below 2^256 for any
/// realistic stable-pair loan).
fn mul_div_floor(a: U256, b: U256, denom: U256) -> U256 {
    if denom.is_zero() {
        return U256::ZERO;
    }
    a.saturating_mul(b) / denom
}

fn pow10(exp: u8) -> U256 {
    U256::from(10u64).pow(U256::from(exp))
}

// ---- tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn pool_a() -> Address {
        address!("6c8E5D463a2473b1A8bcd87e1cEA2724203A1D8f")
    }

    fn pool_b() -> Address {
        address!("41cb3dd6824bb9c4cfc0cc4e15675f7a2e6af869")
    }

    fn usdt0() -> Address {
        address!("aabbccddeeff00112233445566778899aabbccdd")
    }

    fn balanced_candidate() -> Candidate {
        Candidate {
            id: "USDT0/USDm@1bps:kumbaya->prismfi:borrow=USDT0".into(),
            borrow_token: usdt0(),
            borrow_decimals: 6,
            legs: [
                V3Leg {
                    pool: pool_a(),
                    zero_for_one: true,
                    fee_pips: 100,
                },
                V3Leg {
                    pool: pool_b(),
                    zero_for_one: false,
                    fee_pips: 100,
                },
            ],
        }
    }

    /// `sqrt_price_x96 = 2^96` corresponds to price = 1.
    fn unit_price() -> U256 {
        U256::from(1u8) << 96
    }

    #[test]
    fn balanced_pools_no_profit() {
        let cand = balanced_candidate();
        let cfg = DetectorConfig::default();
        let state = PoolStateView {
            sqrt_price_x96: unit_price(),
            liquidity: 9_000_000_000_000_000_000_000u128, // 9e21 — Kumbaya-class deep
        };
        let q = evaluate_candidate(&cand, &state, &state, &cfg, 100).unwrap();
        assert!(q.is_none(), "balanced pools must never produce a profit");
    }

    /// Engineer a price dislocation. Candidate borrows token0 (USDT0):
    /// leg 0 sells token0 on A (z4o=true), leg 1 buys token0 on B (z4o=false).
    /// To profit we want pool A to price token0 *higher* than pool B — i.e.
    /// we sell into the rich pool and rebuy from the cheap one.
    /// Bump A's sqrt price by ~2.5% ⇒ ~5% price gap ⇒ ~500 bps gross edge,
    /// well above the ~57 bps clearing bar (5 bps Aave + 2 bps fees + 50 bps margin).
    #[test]
    fn dislocation_clears_threshold() {
        let cand = balanced_candidate();
        let cfg = DetectorConfig::default();

        // 2.5% sqrt bump = ~5% price gap.
        let bump = unit_price() / U256::from(40u32);
        let state_a = PoolStateView {
            sqrt_price_x96: unit_price() + bump,
            liquidity: 9_000_000_000_000_000_000_000u128,
        };
        let state_b = PoolStateView {
            sqrt_price_x96: unit_price(),
            liquidity: 4_300_000_000_000_000_000_000u128,
        };

        let q = evaluate_candidate(&cand, &state_a, &state_b, &cfg, 555)
            .expect("v3 math ok")
            .expect("dislocation should clear");

        assert!(
            q.gross_edge_bps > 100,
            "expected >100 bps gross, got {}",
            q.gross_edge_bps
        );
        assert_eq!(q.block_number, 555);
        assert!(q.net_profit_wei > U256::ZERO);
    }

    #[test]
    fn loan_sweep_picks_best() {
        // With a clear dislocation and deep liquidity, larger loans pay more
        // in absolute terms — so $1000 should win the sweep.
        let cand = balanced_candidate();
        let cfg = DetectorConfig::default();
        let bump = unit_price() / U256::from(40u32);
        let state_a = PoolStateView {
            sqrt_price_x96: unit_price() + bump,
            liquidity: 9_000_000_000_000_000_000_000u128,
        };
        let state_b = PoolStateView {
            sqrt_price_x96: unit_price(),
            liquidity: 4_300_000_000_000_000_000_000u128,
        };

        let q = evaluate_candidate(&cand, &state_a, &state_b, &cfg, 1)
            .unwrap()
            .unwrap();
        assert_eq!(q.loan_wei, U256::from(1_000_000_000u64)); // 1000 * 1e6 (USDT0 6 dp)
    }

    fn quote_with_profit(micros: u64) -> OpportunityQuote {
        OpportunityQuote {
            loan_wei: U256::from(1_000_000_000u64),
            gross_out_wei: U256::from(1_000_500_000u64),
            gross_edge_bps: 5,
            net_profit_wei: U256::from(micros),
            net_profit_usd_micros: micros,
            block_number: 1,
        }
    }

    #[test]
    fn tracker_open_close_lifetime() {
        let mut t = OpportunityTracker::new();
        let id = "abc".to_string();

        let evs = t.record(
            vec![(id.clone(), Some(quote_with_profit(1_500_000)))],
            1_000,
        );
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], OpportunityEvent::Opened { .. }));
        assert_eq!(t.open_count(), 1);

        // Mid-life update — no event, but peak/mean accumulate.
        let evs = t.record(
            vec![(id.clone(), Some(quote_with_profit(3_000_000)))],
            1_050,
        );
        assert!(evs.is_empty());

        let evs = t.record(vec![(id.clone(), None)], 1_100);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            OpportunityEvent::Closed {
                lifetime_ms,
                peak_profit_usd_micros,
                mean_profit_usd_micros,
                samples,
                ..
            } => {
                assert_eq!(*lifetime_ms, 100);
                assert_eq!(*peak_profit_usd_micros, 3_000_000);
                // mean = (1.5 + 3.0) / 2 = 2.25
                assert_eq!(*mean_profit_usd_micros, 2_250_000);
                assert_eq!(*samples, 2);
            }
            _ => panic!("expected Closed"),
        }
        assert_eq!(t.open_count(), 0);
    }

    #[test]
    fn tracker_no_event_when_persistently_closed() {
        let mut t = OpportunityTracker::new();
        let id = "x".to_string();
        for ts in [1, 2, 3, 4u64] {
            assert!(t.record(vec![(id.clone(), None)], ts).is_empty());
        }
        assert_eq!(t.open_count(), 0);
    }

    #[test]
    fn tracker_skipped_tick_does_not_close() {
        // An open id absent from `results` (e.g. stale state skip) must not close.
        let mut t = OpportunityTracker::new();
        let id = "y".to_string();
        t.record(vec![(id.clone(), Some(quote_with_profit(1_000_000)))], 0);
        let evs = t.record(vec![], 50); // empty results
        assert!(evs.is_empty());
        assert_eq!(t.open_count(), 1);
        // Now close.
        let evs = t.record(vec![(id.clone(), None)], 100);
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn usd_conversions_roundtrip_at_decimals_6_and_18() {
        // Decimals 6 (USDT0): 1 micro == 1 wei.
        let w6 = usd_micros_to_wei(1_000_000, 6);
        assert_eq!(w6, U256::from(1_000_000u64));
        assert_eq!(wei_to_usd_micros(w6, 6), 1_000_000);

        // Decimals 18 (USDm): $1.00 = 1e18 wei = 1_000_000 micros.
        let w18 = usd_micros_to_wei(1_000_000, 18);
        assert_eq!(w18, U256::from(10u64).pow(U256::from(18u8)));
        assert_eq!(wei_to_usd_micros(w18, 18), 1_000_000);
    }

    #[test]
    fn bps_floor_basic() {
        // 50 bps of 10_000_000 = 50_000.
        assert_eq!(
            bps_floor(U256::from(50_000u64), U256::from(10_000_000u64)),
            50
        );
    }
}
