# Handoff — Phase 2 (V3 swap math + watch-only MVP)

> **Use this file to start a new session.** Paste the prompt block at the bottom into a fresh Claude Code session in this repo.

---

## Where we are

Phase 0 (scaffold), Phase 1 (real addresses, 47 pools), Phase 2 step 1 (V3 single-tick swap math + byte-exact parity vs the Kumbaya USDT0/USDm 1bps pool), Phase 2 step 2 (polling V3 pool state cache), and **Phase 2 steps 3-4 (opportunity detector + watch-only binary mode)** are done.

What's already in:

- **Foundry workspace** in [contracts/](../contracts/). `ArbExecutor.sol` handles the Aave V3 flash-loan callback for V2-pair routes; `probe/V3SwapProbe.sol` is the off-chain quoting helper used by the parity test. V3 executor support deferred to Phase 3.
- **Cargo workspace** in [searcher/](../searcher/), 5 crates. V2 + V3 swap math byte-equivalent to canonical Uniswap, both parity-tested. `searcher-core` has 17 cargo tests (15 unit/property + 1 V2-cycle + 1 offline-fixture) plus 1 ignored online parity test.
- **V3 math**: [searcher/crates/searcher-core/src/v3.rs](../searcher/crates/searcher-core/src/v3.rs), public surface `v3_amount_out_single_tick(sqrtPriceX96, L, amount_in, fee_pips, zero_for_one) -> V3Quote`. Single-tick approximation valid at MVP loan sizes; full tick-walking is Phase 4.
- **Parity infrastructure**: [tests/v3_parity.rs](../searcher/crates/searcher-core/tests/v3_parity.rs) drives the live USDT0/USDm 1bps Kumbaya pool via `eth_call` + `stateOverride`, injecting `V3SwapProbe` runtime bytecode at a virtual address. 100/100 byte-exact against the chain at a pinned block. Recorded fixtures replay deterministically in CI via [tests/v3_offline.rs](../searcher/crates/searcher-core/tests/v3_offline.rs).
- **Config** in [config/mainnet.toml](../config/mainnet.toml): chain 4326, Aave V3 Pool, 8 reserve tokens (3 flash-loanable: USDm/USDe/USDT0), 2 DEXs (Kumbaya, Prismfi), 47 pools sorted by fee tier.
- **CI** in [.github/workflows/ci.yml](../.github/workflows/ci.yml): forge test/fmt + cargo test/clippy/fmt on push.

**Critical gotcha** (saved to memory; flagging here too): when comparing Rust pool-math to live `eth_call`, **pin every call to the same `BlockId`**. MegaETH's 10ms mini-blocks otherwise drift the pool state mid-test and surface ~5e-11 relative deltas that look like math bugs but are pure state drift.

## Strategy in one paragraph

MegaETH runs a single sequencer with 10ms mini-blocks. The "speed game" is **reaction time to sequencer state diffs**, not gas auctions. Aave V3 lets us flash-borrow USDm/USDe/USDT0 — every cycle must start and end in one of those. Two DEXs (Kumbaya + Prismfi, both Uniswap V3 forks) give us a real cross-venue arb on **USDT0/USDm @ 1bps** (deep on both: L≈9e21 and L≈4.3e21). Total fees on that 2-leg cycle = 2 bps pool + 5 bps Aave = 7 bps — clearing bar before gas + safety.

## Phase 2 goal — watch-only MVP

Build a passive observer that connects to the chain, watches the registered pools, and logs every cross-venue opportunity that crosses the profitability threshold. **No transactions, no signing, no Aave calls.** We isolate detection correctness from execution before writing any executor code.

For each opportunity, log:

- **timestamp_ms** (mini-block timestamp at first detection)
- **cycle**: ordered list of `(dex, pool, token_in → token_out, fee_bps)`
- **loan_amount** used for the sim (sweep a few sizes: $100, $500, $1k)
- **expected_out** (Rust V3 simulation — must match `eth_call` quote within 1 wei in unit tests)
- **gross_edge_bps**, **fee_total_bps**, **net_edge_bps_after_aave**
- **gas_estimate_usd** (from forge gas snapshot once we add V3 in the executor; for MVP use a reasonable constant like $0.01 — gas on MegaETH is cheap)
- **profit_usd** (net of all fees)

Then track **opportunity lifetime**: from first detection to first sim where the cycle no longer clears the bar. Log the lifetime in milliseconds and the time-weighted average profit during that window.

### Acceptance criteria

- `cargo test --workspace` includes a V3 math parity test against `eth_call`-quoted swaps for the USDT0/USDm 1bps Kumbaya pool. **Match within 1 wei** for at least 100 random inputs in the realistic loan-size range.
- `cargo run -p searcher-bin -- --config config/mainnet.toml --watch-only` connects, ingests state, and logs structured JSON for opportunities + closures. Runs cleanly for at least 1 hour without crashing.
- One end-to-end log line per detected opportunity *and* per closure (with lifetime).

## What needs to be built

### 1. V3 swap math in Rust (`searcher/crates/searcher-core/src/v3.rs`) — ✅ done

Landed: hand-written port of Uniswap V3 `SwapMath.computeSwapStep` (single-tick), `SqrtPriceMath`, and `FullMath`. U256-only, ~280 LOC, no `uniswap-v3-math` dep. Public surface is a single function:

```rust
pub fn v3_amount_out_single_tick(
    sqrt_price_x96: U256, liquidity: u128, amount_in: U256,
    fee_pips: u32, zero_for_one: bool,
) -> Result<V3Quote, V3Error>;
```

Parity vs the live USDT0/USDm 1bps Kumbaya pool (`0x6c8E5D…1D8f`) is **byte-exact 100/100** at the latest pinned block, via a custom `V3SwapProbe.sol` injected at a virtual address through `eth_call` + `stateOverride`. CI replays 100 recorded fixtures deterministically through [tests/v3_offline.rs](../searcher/crates/searcher-core/tests/v3_offline.rs); regenerate via:

```bash
MEGAETH_RPC=https://mainnet.megaeth.com/rpc REGEN_FIXTURES=1 \
    cargo test -p searcher-core --test v3_parity -- --include-ignored --nocapture
```

Multi-tick walking is still Phase 4 — at MVP loan sizes ($100–$1k in deep stablecoin pools) the swap stays inside the active tick.

### 2. State ingestion (polling MVP) — ✅ done

[searcher-net/src/poller.rs](../searcher/crates/searcher-net/src/poller.rs) batches `slot0()` + `liquidity()` for every watched pool, pinned to the same `BlockId`, decodes raw return bytes, and writes into `PoolRegistry` keyed by pool address. After all per-pool writes for a block land, it bumps the `last_block` watermark with release ordering. Exponential backoff on whole-batch failures (200ms → 5s). Per-pool failures retain prior cached state and log at `warn`. Realtime WS in [realtime.rs](../searcher/crates/searcher-net/src/realtime.rs) is still a stub — switch over once polling latency becomes the bottleneck.

### 3. Opportunity detector — ✅ done

[searcher-core/src/detector.rs](../searcher/crates/searcher-core/src/detector.rs) is a pure module — no I/O, no async. Public surface:

- `Candidate { id, borrow_token, borrow_decimals, legs: [V3Leg; 2] }` — one 2-leg cross-venue cycle.
- `evaluate_candidate(cand, &state_a, &state_b, cfg, block) -> Result<Option<OpportunityQuote>, V3Error>` — sweeps loan sizes in `cfg.loan_sizes_usd`, returns the size with the highest net profit, or `None` if none clears the bar.
- Net = `gross_profit - aave_premium(5bps) - gas_cost - max(min_profit, loan × safety_margin_bps/10_000)`. All comparisons in U256; floats only at log time.
- `OpportunityTracker::record(results, now_ms) -> Vec<OpportunityEvent>` — turns per-tick `(id, Option<quote>)` into `Opened` / `Closed` events with lifetime, peak, mean, sample count. IDs absent from `results` (e.g. stale state skip) don't close.

### 4. Watch-only binary mode — ✅ done

[searcher-bin/src/main.rs](../searcher/crates/searcher-bin/src/main.rs) builds candidates from the registry (groups by `(pair, fee_pips)`, requires ≥ 2 DEXs, filters borrow token to `aave.flashloan_assets`), spawns `PoolPoller::run` as a tokio task, and runs the detector loop on `block_rx.changed()`. Per-tick: snapshot state, skip candidates whose pool blocks aren't pinned to the current `last_block`, evaluate, feed results into the tracker, emit each event via `tracing::info!(target: "opportunity", …)`. The existing JSON subscriber serializes structured fields. Metrics: `opportunities_opened_total`, `opportunities_closed_total`, `opportunity_lifetime_ms`, `detector_skipped_stale_total`, `detector_open_opportunities`, `detector_eval_errors_total`.

Run:
```bash
MEGAETH_RPC=https://carrot.megaeth.com/rpc \
    cargo run -p searcher-bin --release -- \
      --config config/mainnet.toml --watch-only --poll-ms 150
```

### 5. Next — Phase 3

ArbExecutor V3 support, tx building/signing in [searcher-exec](../searcher/crates/searcher-exec/), and live submission. Don't start until the watcher logs prove opportunities exist with a usable lifetime distribution. Tracking in [docs/PLAN.md](PLAN.md) Phase 3.

## Useful references

- [docs/PLAN.md](PLAN.md) — full plan, including phase boundaries and verification steps.
- [CLAUDE.md](../CLAUDE.md) — architectural conventions and fee discipline. The "Cross-venue same-pair pools" table is the cycle catalog.
- [config/mainnet.toml](../config/mainnet.toml) — addresses + pool registry. The `[[pools]]` entries are the input to cycle enumeration.
- [bgd-labs/aave-address-book/src/AaveV3MegaEth.sol](https://github.com/bgd-labs/aave-address-book/blob/main/src/AaveV3MegaEth.sol) — Aave V3 deployment.
- MegaETH explorer: <https://mega.etherscan.io>.
- Public RPC: `https://mainnet.megaeth.com/rpc` (chain 4326).

## Conventions to follow (from CLAUDE.md)

- Atomic-or-revert when execution lands in Phase 3.
- Pure Rust simulation matches Solidity to the wei. Parity test or it didn't happen.
- U256 only — no `f64` in profit math.
- Latency budget < 1 ms in-process; benchmark hot paths with `criterion`.
- No public-mempool broadcasts.
- Use **Claude Opus 4.7 at high effort (not max)** for development. Switch via `/model claude-opus-4-7[1m]`.
- Don't spawn research subagents to find addresses or DEX info — ask the user; they prefer to feed authoritative data directly.

---

## Prompt to paste into a new session

```text
I'm continuing work on the MegaETH flash-loan arbitrage bot in this repo
(github.com/gokhanseckin/megaeth-arb). Read docs/HANDOFF.md and CLAUDE.md
first — they have the full context.

Where we are: Phase 0 (scaffold) ✓, Phase 1 (real addresses, 47 pools)
✓, Phase 2 step 1 (V3 single-tick swap math + byte-exact parity vs the
Kumbaya USDT0/USDm 1bps pool via eth_call + stateOverride) ✓ — see
searcher/crates/searcher-core/src/v3.rs and tests/v3_parity.rs.

The MVP is **watch-only**: a passive observer that detects cross-venue
arb opportunities, logs theoretical profit + loan size, and tracks how
long each opportunity stayed profitable before closing. No transactions,
no signing, no Aave calls. We're isolating detection from execution.

Primary cycle to validate: USDT0/USDm @ 1bps Kumbaya ⇄ Prismfi
(clearing bar ~7 bps + gas).

Recommended next order (Phase 2 steps 2-4 from docs/HANDOFF.md):

1. Polling-based pool state cache (searcher-pools extension). At
   100-200ms cadence, batch eth_call slot0() + liquidity() for every V3
   pool in config/mainnet.toml. Pin every batch to the same block —
   MegaETH 10ms blocks otherwise drift state mid-batch and create ghost
   mismatches (this lesson is in the auto-memory; HANDOFF.md flags it).
2. Opportunity detector (searcher-core::cycle extension). For each
   cross-venue same-pair-same-fee match in the config, enumerate both
   directions, sweep loan sizes ($100/$500/$1k), compute net edge with
   v3_amount_out_single_tick. Emit OpportunityOpened / OpportunityClosed
   events with lifetimes + theoretical profit.
3. Watch-only binary mode: --watch-only flag in searcher-bin wires
   cache + detector together, JSON-logs every event to stdout. Run
   cleanly for 1h against mainnet, confirm logs look right, commit.

Deployment: target is a Hetzner Cloud CX43 (region TBD). After step 3
lands and the watcher proves opportunities are real, write a
Terraform-imported infra/ + Makefile + searcher.service systemd unit
in a follow-up commit. Skip until there's a real binary to deploy.

Use Opus 4.7 at high effort (not max). Ask me for non-canonical project
data (addresses, DEX URLs, sequencer region) instead of spawning a
research agent — I'll paste it.

Start by reading docs/HANDOFF.md, then propose a concrete plan for
step 2 (polling pool state cache) — module shape, target pool selection
from config, polling cadence, error handling on batch slot0/liquidity
fetches — and confirm with me before writing code.
```
