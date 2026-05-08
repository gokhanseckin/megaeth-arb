# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`megaeth-arb` is a flash-loan arbitrage system for **MegaETH** (EVM L2, single sequencer, 10ms mini-blocks, Realtime API for state diffs). It detects price dislocations across AMM pools, borrows from **Aave V3** flash loans, and executes the cycle atomically — reverting if not profitable.

The repo is a polyglot monorepo:
- **`contracts/`** — Foundry workspace. Solidity executor (`ArbExecutor.sol`) that receives the Aave flash loan, runs swap legs against AMMs directly (no routers), and reverts unless `minProfit` is met.
- **`searcher/`** — Cargo workspace (Rust). Off-chain bot that consumes the MegaETH Realtime WS, maintains a pool state cache, detects opportunities, simulates them in pure Rust math, and submits signed txs.
- **`config/`** — Per-network TOML: chain RPC, Aave/DEX addresses, token allowlist, gas params, caps.

## Architectural Conventions

- **Atomic-or-revert.** The Solidity executor enforces `minProfit` on-chain. Off-chain math is the *prediction*; the contract is the *backstop*. Never rely solely on off-chain checks for safety.
- **Direct pool calls, no routers.** `ArbExecutor` calls `IUniswapV2Pair.swap` / `IUniswapV3Pool.swap` directly with off-chain-computed `amountOut`. Routers add gas and an extra trust surface.
- **Pure Rust simulation.** Don't `eth_call` to quote — the Rust math must match `UniV2Math.sol` / `UniV3Math.sol` to the wei. There's a parity test for this; keep it green.
- **Latency budget is real.** In-process pipeline target is **< 1 ms** WS-frame to signed-tx. New code in the hot path must be benchmarked (criterion) and not allocate in steady state.
- **Don't broadcast to public mempool.** MegaETH has a single sequencer — submit only to sequencer RPC endpoints from `config/`.
- **U256 fixed-point only.** No `f64` in profit math — `ruint` / `alloy_primitives::U256`.

## Development Model

Use **Claude Opus 4.7 at high effort (not max)** for implementation work in this project. Switch with `/model claude-opus-4-7[1m]` if the session starts on a different model.

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
```

## Environment

The bot needs:
- `MEGAETH_RPC` — sequencer HTTP RPC URL
- `MEGAETH_WS` — Realtime WS URL
- `MEGAETH_PK` — hot wallet private key (test funds only outside production)

Production hot wallets must be a dedicated key with **no upgrade or sweep authority** — those live on a multisig owner of `ArbExecutor`.

## Phased Roadmap

See [.claude/plans/purring-plotting-lollipop.md](/Users/gokhanseckin/.claude/plans/purring-plotting-lollipop.md) for the full plan. Current phase: **Phase 0 — setup**.
