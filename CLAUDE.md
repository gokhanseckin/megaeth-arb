# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`megaeth-arb` is a flash-loan arbitrage system for **MegaETH** (EVM L2, single sequencer, 10ms mini-blocks, Realtime API for state diffs). It detects price dislocations across AMM pools, borrows from **Aave V3** flash loans, and executes the cycle atomically — reverting if not profitable.

The repo is a polyglot monorepo:
- **`contracts/`** — Foundry workspace. Solidity executor (`ArbExecutor.sol`) that receives the Aave flash loan, runs swap legs against AMMs directly (no routers), and reverts unless `minProfit` is met.
- **`searcher/`** — Cargo workspace (Rust). Off-chain bot that consumes the MegaETH Realtime WS, maintains a pool state cache, detects opportunities, simulates them in pure Rust math, and submits signed txs.
- **`config/`** — Per-network TOML: chain RPC, Aave/DEX addresses, token allowlist, gas params, caps.

## MegaETH Aave V3 — what's flash-loanable

Aave V3 on MegaETH only allows flash-loan borrowing of **3 stablecoins**: `USDm`, `USDe`, `USDT0`. Other reserves (WETH, BTCb, wstETH, wrsETH, ezETH, plus the borrowable stables) can be supplied/borrowed normally but not flash-loaned.

**Strategy implication.** Every arb cycle must start *and end* in one of the three borrowable stables. Intermediate hops can route through any token on any DEX. Triangular shapes look like `USDT0 → WETH → USDC → USDT0` or `USDe → BTCb → USDT0 → USDe`.

## Fee discipline (central constraint)

Every leg's fee compounds. For a candidate cycle to fire, the **gross edge** must clear:

```
Σ(pool fees on path) + Aave premium (5 bps) + gas_cost + safety_margin
```

- **Prefer 1 bps and 5 bps pools.** These are the stablecoin tiers. A 2-leg cycle in 1bps pools needs only ~7-8 bps of edge to clear fees+premium (still need to add gas + margin).
- **30 bps tolerable on a directional leg**, but not for both sides of a balanced arb — a 30/30/aave cycle needs ~65 bps of edge before it even looks at gas.
- **1% pools generally infeasible.** Don't enumerate cycles that touch them unless investigating low-activity stale-price opportunities (low-prob, high-variance).
- **Log fee total alongside expected output** when proposing cycles, so the user can sanity-check.

The pool registry in [config/mainnet.toml](config/mainnet.toml) is sorted into low-fee, mid-fee, and high-fee buckets accordingly.

### Cross-venue same-pair pools (2-leg arb candidates)

These are the pairs where both DEXs have the same fee tier — pure 2-leg arb works without needing a third hop.

| Pair | Fee | Kumbaya | Prismfi | Why this matters |
|---|---|---|---|---|
| **USDT0/USDm** | **1bps** | `0x6c8E5D…1D8f` | `0x41cb3dd…f869` | **Killer pair.** 2 + 5 (Aave) = 7 bps clearing bar. |
| WETH/USDm | 30bps | `0x587F6e…4b22` | `0xc2fac0…9d32` | 60 + 5 = 65 bps. Only fires on directional dislocation. |
| MEGA/USDm | 30bps | `0xA8275D…7764` | `0x36c062…e9f6` | Same 65 bps bar. |
| MEGA/USDT0 | 30bps | `0x9F4cEa…b2cd` | `0x3a62f0…7c46` | Tiny pools both sides, mostly logged. |
| BTC.b/USDm | 30bps | `0xc1838B…c9db` | `0x2a69d0…2aec` | Prismfi side is sub-$1k volume. |
| MEGA/WETH | 30bps/100bps | `0x549257…00EB` (1%), `0x7a37e1…3d8D` (1%) | `0x8c2a65…04df` (30bps), `0x9fe7a4…f663` (1%) | Cross-fee mismatch — not a clean 2-leg cycle. |
| cUSD/USDm | 100bps/100bps | `0xEDB8a6…99d3` | `0xf428be…28ef` | 2% pool fees; ignore. |

**Phase 1 fork test should target `USDT0/USDm @ 1bps` cross-venue first** — the only cycle where the math is reliably above-water at typical gas prices.

Authoritative addresses live in [config/mainnet.toml](config/mainnet.toml). Source: [bgd-labs/aave-address-book/src/AaveV3MegaEth.sol](https://github.com/bgd-labs/aave-address-book/blob/main/src/AaveV3MegaEth.sol).

## Architectural Conventions

- **Atomic-or-revert.** The Solidity executor enforces `minProfit` on-chain. Off-chain math is the *prediction*; the contract is the *backstop*. Never rely solely on off-chain checks for safety.
- **Direct pool calls, no routers.** `ArbExecutor` calls `IUniswapV2Pair.swap` / `IUniswapV3Pool.swap` directly with off-chain-computed `amountOut`. Routers add gas and an extra trust surface.
- **Pure Rust simulation.** Don't `eth_call` to quote — the Rust math must match `UniV2Math.sol` / `UniV3Math.sol` to the wei. There's a parity test for this; keep it green.
- **Latency budget is real.** In-process pipeline target is **< 1 ms** WS-frame to signed-tx. New code in the hot path must be benchmarked (criterion) and not allocate in steady state.
- **Don't broadcast to public mempool.** MegaETH has a single sequencer — submit only to sequencer RPC endpoints from `config/`.
- **U256 fixed-point only.** No `f64` in profit math — `ruint` / `alloy_primitives::U256`.

## Development Model

Use **Claude Opus 4.7 at high effort (not max)** for implementation work in this project. Switch with `/model claude-opus-4-7[1m]` if the session starts on a different model.

## Git workflow (multi-session, self-verified)

Multiple Claude sessions run in parallel worktrees. The user does not review code — Claude self-verifies before every merge. The hazards this section addresses are the real ones we've hit: stale PRs that drift from main, two sessions adding the same logical thing to a shared file, leftover worktrees blocking checkout.

**Branching**
- One worktree per session under `.claude/worktrees/<slug>`. Never edit another session's worktree.
- Never commit on `main`. Branch first: `claude/<slug>` for session work, `phase-N/<topic>` for multi-session epics, `fix/<topic>` / `chore/<topic>` for short-lived work.
- **Pre-flight before checkout/work**: `git fetch origin && git worktree list`. If the target branch is already checked out elsewhere, do NOT clone it into a new worktree. Stop and resolve (most often: the other worktree is leftover state from a finished session — switch it to `main` and remove it).

**Commits**
- Small, single-concern. Co-author trailer on every Claude commit.
- Don't `--amend` or rebase published commits unless you own the branch lock (see below). Don't `--no-verify` hooks.

**Shared-file edit protocol** (`CLAUDE.md`, `config/*.toml`, `Cargo.lock`, `foundry.toml`)
1. `git fetch origin main && git rebase origin/main` BEFORE the first edit. Not just before PR open — before EACH session that will touch a shared file.
2. After editing: re-run the rebase. If main moved during your edit, rebase again so the PR diff is minimal.
3. Run the config lint (when added: `cargo run --bin lint_config`) before commit. It catches dup token addresses, casing drift, missing token0/token1 entries.
4. `Cargo.lock`: prefer `cargo update -p <crate>` over bare `cargo update`.
5. `[[pools]]` entries are append-only unless the user explicitly asks to remove one. `[[tokens]]` may be deduped.

**Phase-N branch lock**
- `phase-N/*` branches are shared by definition (multi-session epics). Default: only the session that opened the PR force-pushes; others wait.
- A session that needs to rebase a stale `phase-N/*` onto current main MAY force-push iff: (a) it has run the full self-check gate locally on the rebased branch AND (b) `git worktree list` shows no other active worktree on that branch AND (c) the PR has no in-flight commits from another session in the last hour.
- Document the force-push in the PR with `git log --pretty=oneline origin/phase-N/<topic>..HEAD` snapshot.
- `claude/<slug>` branches: free force-push, you own them.

**Self-check gate (must pass before merge)**
Before merging any PR, Claude runs and reports:
1. `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings`
2. `cargo test --workspace` (and `forge test -vvv` if Solidity changed)
3. The Rust↔Solidity parity test, if math files changed
4. CI green on the PR
5. **PR is up-to-date with main**: `gh pr view <N> --json mergeStateStatus` returns `CLEAN` AND the merge-base equals current `origin/main` HEAD. If main moved, rebase + re-run the gate. **A stale PR that GitHub calls `MERGEABLE` can still be semantically wrong** (e.g. two sessions independently adding the same token at different file positions — git's 3-way merge applies both, producing a duplicate).
6. Config lint (when available) on shared files touched in the PR.

If any fails, fix the cause; don't bypass. Report the green checklist in the PR body so the audit trail shows what was verified.

**PRs as audit trail**
- Every change reaches `main` via PR. No direct pushes to `main`.
- One concern per PR — narrow diffs merge cleanly across parallel sessions.
- PR body: what changed, why, self-check results.
- Claude may merge its own PR once the self-check gate passes.
- **Don't let PRs sit stale.** If a PR has been open while main moved by ≥ 1 commit, the next session that touches the area must rebase it onto current main (or merge it first). Stale PRs become semantic-conflict landmines.

**Worktree lifecycle (cleanup is mandatory, not aspirational)**
- After your PR merges: `git worktree remove <path>` AND `git branch -d claude/<slug>`. Don't reuse a merged worktree.
- After a `phase-N/*` branch's PR merges: switch any worktree on it back to `main` (`git switch main` from inside that worktree), then `git branch -D phase-N/<topic>` if no other worktree references it.
- A session encountering a worktree on a merged branch may clean it up (it's leftover state, not active work).

**Handoff between sessions**
- Mid-feature stop → push branch, open draft PR. The PR body is the git-side handoff (what's done, what's left). Cross-session memory of decisions and exploration is captured automatically by claude-mem; don't duplicate it in PR text or scratch files.
- Never leave uncommitted work in a worktree another session might inherit.

## Common Commands

```bash
# Solidity
forge build
forge test -vvv
forge test --fork-url $MEGAETH_RPC          # end-to-end fork tests
forge test --match-test test_FlashArb_2Leg  # single test
forge snapshot                               # gas regression

# Rust
cargo build --release
cargo test --workspace
cargo test -p searcher-core cycle::          # single crate / module
cargo bench -p searcher-core                 # criterion benchmarks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# Watchers
cargo run --bin searcher -- --config config/mainnet.toml --watch-only       # flash-loan watcher
cargo run --bin cex-watcher -- --config config/cex-watcher.toml             # CEX↔DEX correlation logger
cargo run --bin cex-watcher -- --config config/cex-watcher.toml --no-dex    # Binance-only smoke test (no MEGAETH_RPC_KEY needed)
```

## Watchers

Two independent watcher binaries share the workspace:

- **`searcher`** (`searcher-bin`) — flash-loan opportunity detector against MegaETH V3 pools. Uses `config/mainnet.toml`.
- **`cex-watcher`** (`searcher-cex-bin`) — CEX↔DEX correlation logger. Subscribes to Binance public WS (`aggTrade` + `bookTicker`) for ETHUSDT/BTCUSDT and to MegaETH state for the matching pools. Phase A scope is dual-stream JSON logging only — offline analysis decides whether the lead/lag is real before any correlator code lands. Uses `config/cex-watcher.toml`, which references `mainnet.toml` for the on-chain side so pool addresses are not duplicated.

Library code is shared read-only via the `searcher-net` and `searcher-pools` crates; neither watcher edits the other's binary or config file.

## Environment

The bot needs:
- `MEGAETH_RPC` — sequencer HTTP RPC URL
- `MEGAETH_WS` — Realtime WS URL
- `MEGAETH_RPC_KEY` — Alchemy API key. Interpolated into `rpc_urls` in `config/mainnet.toml` via `${MEGAETH_RPC_KEY}` at config-load time; the loader fails fast if unset.
- `MEGAETH_PK` — hot wallet private key (test funds only outside production)

Production hot wallets must be a dedicated key with **no upgrade or sweep authority** — those live on a multisig owner of `ArbExecutor`.

## Phased Roadmap

See [docs/PLAN.md](docs/PLAN.md) for the full plan. **Current scope: watch-only MVP.**

### MVP scope (revised)

The first deliverable is a **passive observer** that does not execute trades. It:

1. Connects to MegaETH Realtime API (or polls if WS isn't ready) and watches the registered pool set.
2. Detects when a cross-venue cycle becomes theoretically profitable (gross edge clears Σ pool fees + Aave 5 bps + estimated gas + safety).
3. Logs each opportunity with: timestamp, cycle path, theoretical profit USD, loan size used in sim, gas estimate.
4. Tracks **opportunity lifetime** — from "first profitable" to "no longer profitable" — and logs that duration on close.

This isolates the *detection* problem (correctness of V3 math, latency of state ingestion, profitability gating) from the *execution* problem (atomic on-chain swaps + flash loan). Once the watcher reliably surfaces real opportunities and we understand their typical lifetime, we extend `ArbExecutor` with V3 swap support and start submitting.

Watch-mode is also the empirical answer to "is this strategy worth shipping" — if the watcher logs zero clearings-the-bar opportunities for a week, fix the strategy before writing any execution code.

### V3 swap math is the critical path

Both DEXs (Kumbaya, Prismfi) are Uniswap V3 forks. The detector needs Rust V3 swap math byte-equivalent to `pool.swap()`. Single-tick approximation is acceptable for the MVP loan sizes ($100s-$1k); full tick-walking lands when loan sizes grow.
