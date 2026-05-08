# Handoff — Phase 2 (V3 swap math + watch-only MVP)

> **Use this file to start a new session.** Paste the prompt block at the bottom into a fresh Claude Code session in this repo.

---

## Where we are

Phase 0 (scaffold) and Phase 1 (real addresses) are done. Branch `phase-1/addresses` is open as [PR #1](https://github.com/gokhanseckin/megaeth-arb/pull/1) — verify it's merged into `main` before starting Phase 2 work, or rebase Phase 2 onto whichever branch is current.

What's already in:

- **Foundry workspace** in [contracts/](../contracts/). `ArbExecutor.sol` has the Aave V3 flash-loan callback wired and a V2-pair swap path. **No V3 swap support yet.** 5/5 forge tests pass.
- **Cargo workspace** in [searcher/](../searcher/), 5 crates. V2 swap math byte-equivalent to the Solidity lib (parity-tested). **No V3 math yet.** 5/5 cargo tests pass.
- **Config** in [config/mainnet.toml](../config/mainnet.toml): chain 4326, Aave V3 Pool, 8 reserve tokens (3 flash-loanable: USDm/USDe/USDT0), 2 DEXs (Kumbaya, Prismfi), 47 pools sorted by fee tier.
- **CI** in [.github/workflows/ci.yml](../.github/workflows/ci.yml): forge test/fmt + cargo test/clippy/fmt on push.

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

### 1. V3 swap math in Rust (`searcher/crates/searcher-core/src/v3.rs`)

- Inputs: `slot0` (`sqrtPriceX96`, `tick`), `liquidity` (active L), `fee_pips`, `tick_spacing`, plus a tick-data window for tick crossings.
- For MVP loan sizes, **single-tick approximation** is acceptable: assume the swap doesn't cross a tick. This is true for the bulk of small swaps in deep pools.
- Add a parity test: simulate vs `eth_call` against a forked node, 100+ random inputs, must match exactly.
- Reference implementation to crib from: [Uniswap v3-core SwapMath.sol](https://github.com/Uniswap/v3-core/blob/main/contracts/libraries/SwapMath.sol). There's also the [`uniswap-v3-math`](https://crates.io/crates/uniswap-v3-math) crate; evaluate before pulling in — we want byte-exact and minimal deps.

### 2. State ingestion (`searcher/crates/searcher-net/src/realtime.rs`)

- Confirm the MegaETH Realtime WS endpoint and message format. Docs: <https://docs.megaeth.com/realtime-api>.
- For MVP, **polling fallback is fine**: every 100-200ms, batch `eth_call` for `slot0()` + `liquidity()` on the watched pool set. Single-sequencer chain, so polling is consistent (no reorgs to worry about). Switch to WS state-diffs only when polling latency becomes the bottleneck.

### 3. Opportunity detector (`searcher/crates/searcher-core/src/cycle.rs` extension)

- For each cross-venue same-pair-same-fee match in [config/mainnet.toml](../config/mainnet.toml), enumerate both directions of the cycle.
- For a sweep of loan sizes, compute net edge.
- If net > threshold, emit an `OpportunityOpened` event.
- On state change, re-evaluate; if the same cycle no longer clears, emit `OpportunityClosed { lifetime_ms, peak_profit_usd, mean_profit_usd }`.

### 4. Watch-only binary mode

Add a `--watch-only` CLI flag (already pseudo-supported by the existing `--dry-run`; either rename or alias). In watch-only mode the binary skips any tx-building paths. Output one JSON line per event to stdout via `tracing` (already configured for JSON sink).

### 5. ArbExecutor V3 support — defer

Don't add V3 to `ArbExecutor.sol` yet. We'll do that in Phase 3 once the watcher proves opportunities exist. Tracking it in [docs/PLAN.md](PLAN.md) Phase 3.

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

Phase 0 (scaffold) and Phase 1 (real addresses) are done. We have:
- Aave V3 Pool, 8 reserve tokens, and 47 pools across Kumbaya + Prismfi
  in config/mainnet.toml.
- A V2 math lib + Solidity ArbExecutor that handles the Aave callback for
  V2-pair routes. Both DEXs are V3 forks though, so V3 support is the
  critical path.
- Cargo workspace builds cleanly with V2 math + parity tests.

The MVP is **watch-only**: a passive observer that detects cross-venue
arb opportunities, logs them with theoretical profit + loan size, and
tracks how long each opportunity stayed profitable before closing. No
transactions, no signing, no Aave calls. We're isolating the detection
problem from the execution problem.

Primary cycle to validate: USDT0/USDm @ 1bps Kumbaya ⇄ Prismfi (clearing
bar ~7 bps + gas).

Acceptance criteria are in docs/HANDOFF.md. The recommended order:

1. V3 swap math in searcher-core (single-tick approximation), with a
   parity test against eth_call against the forked mainnet node.
2. Polling-based pool state cache (100-200ms cadence is fine for MVP;
   skip Realtime WS until polling becomes the bottleneck).
3. Opportunity detector that emits OpportunityOpened/OpportunityClosed
   events with lifetimes and theoretical profit, JSON-logged to stdout.
4. Run for an hour against mainnet, confirm structured logs look right,
   commit.

Please use Opus 4.7 at high effort (not max effort). When you need
non-canonical project data (addresses, DEX URLs, etc), ask me directly
rather than spawning a research agent — I'll paste it.

Start by reading docs/HANDOFF.md, then propose a concrete plan for step 1
(V3 math) and confirm the approach with me before writing code.
```
