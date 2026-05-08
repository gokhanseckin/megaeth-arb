//! Offline V3 parity regression test.
//!
//! Reads `tests/fixtures/v3_quotes.json` (recorded by `tests/v3_parity.rs`
//! against a pinned mainnet block) and asserts the Rust simulation reproduces
//! every recorded sample byte-exactly. Always runs in CI — no network needed.

use alloy::primitives::{Address, U256};
use searcher_core::v3_amount_out_single_tick;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct Fixture {
    #[allow(dead_code)]
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

#[test]
fn v3_quotes_byte_exact_against_recorded_fixtures() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("v3_quotes.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixtures at {}: {}", path.display(), e));
    let fixtures: Vec<Fixture> = serde_json::from_str(&raw).expect("parse fixtures JSON");
    assert!(
        !fixtures.is_empty(),
        "fixtures file is empty — regenerate via `MEGAETH_RPC=... REGEN_FIXTURES=1 cargo test \
         -p searcher-core --test v3_parity -- --include-ignored`"
    );

    let mut mismatches = 0usize;
    for (i, fx) in fixtures.iter().enumerate() {
        let q = v3_amount_out_single_tick(
            fx.sqrt_price_x96,
            fx.liquidity,
            fx.amount_in,
            fx.fee_pips,
            fx.zero_for_one,
        )
        .expect("v3_amount_out_single_tick on recorded inputs");

        if q.amount_out != fx.expected_amount_out
            || q.amount_in_consumed != fx.expected_amount_in_consumed
            || q.sqrt_price_after_x96 != fx.expected_sqrt_price_after_x96
        {
            eprintln!(
                "fixture[{i}] mismatch: amount_in={} z4o={}\n  out         expected={} got={}\n  in_consumed expected={} got={}\n  sqrtP_after expected={} got={}",
                fx.amount_in,
                fx.zero_for_one,
                fx.expected_amount_out,
                q.amount_out,
                fx.expected_amount_in_consumed,
                q.amount_in_consumed,
                fx.expected_sqrt_price_after_x96,
                q.sqrt_price_after_x96,
            );
            mismatches += 1;
        }
    }
    assert_eq!(mismatches, 0, "{mismatches} byte-exact mismatches");
}
