//! Opportunity detection, simulation, and profit math.
//!
//! This crate is intentionally pure — no I/O, no async. All inputs (pool state, gas price,
//! profit threshold) flow in via plain types so the hot path can be deterministically
//! benchmarked and tested in isolation.

pub mod cycle;
pub mod math;
pub mod v3;

pub use cycle::{Cycle, CycleEval, Leg};
pub use math::{v2_amount_out, MathError};
pub use v3::{v3_amount_out_single_tick, V3Error, V3Quote, MAX_SQRT_RATIO, MIN_SQRT_RATIO};
