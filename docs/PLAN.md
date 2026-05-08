# MegaETH Flash-Loan Arbitrage Bot — Architecture & Build Plan

## Context

MegaETH mainnet went live **Feb 9, 2026** — an EVM L2 with a single active sequencer producing **mini-blocks every 10 ms** (100/sec), exposing a **Realtime API** that streams state diffs and pre-confirmed transactions. Aave V3 is deployed on day-one (flash loans available), and the chain has crossed ~$580M TVL with several AMMs live (GTE, Uniswap-style forks, V2-style pools).

The "speed game" on MegaETH is **different from Ethereum L1**:
- No public mempool race / no priority-gas auction in the L1 sense — a single sequencer orders txs.
- The edge comes from (a) **latency to the sequencer RPC**, (b) **reaction time to Realtime API state diffs**, and (c) **simulation/decision speed** to fire before another searcher does.
- Atomicity is still essential — the Solidity executor must revert on unprofitable paths so we never lose principal, only gas.

Goal: an **atomic, flash-loan-funded arbitrage bot** that detects price dislocations between AMM pools on MegaETH, borrows from Aave V3, executes the cycle in one tx, and repays + keeps profit — all reverting if profit < threshold.

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                   OFF-CHAIN SEARCHER (Rust)                     │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐      │
│  │ Realtime │──>│  Pool    │──>│ Opp.     │──>│  Submit  │      │
│  │  WS sub  │   │  State   │   │ Detector │   │  (RPC)   │      │
│  │ (diffs)  │   │  Cache   │   │ + Sim    │   │          │      │
│  └──────────┘   └──────────┘   └──────────┘   └──────────┘      │
│        ▲                            │                           │
│        │                            ▼                           │
│        │                      ┌──────────┐                      │
│        │                      │ Profit   │                      │
│        │                      │ Math     │                      │
│        └──── Pool/Aave ──────│ (gas,fee │                      │
│              addresses        │ slippage)│                      │
│                               └──────────┘                      │
└─────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼  signed tx
┌─────────────────────────────────────────────────────────────────┐
│                  ON-CHAIN EXECUTOR (Solidity)                   │
│                                                                 │
│  Aave V3 Pool ──flashLoan──> ArbExecutor.executeOperation()     │
│                                  │                              │
│                                  ├─> swap leg 1  (DEX A)        │
│                                  ├─> swap leg 2  (DEX B)        │
│                                  ├─> [swap leg 3] (triangular)  │
│                                  ├─> require(profit ≥ minOut)   │
│                                  └─> approve+repay Aave         │
│                                                                 │
│  any failure / unprofitable path → REVERT → only gas lost       │
└─────────────────────────────────────────────────────────────────┘
```

---

## Repository Layout

```
megaeth-arb/
├── contracts/                 # Foundry workspace
│   ├── src/
│   │   ├── ArbExecutor.sol           # Aave receiver + multi-DEX router
│   │   ├── interfaces/
│   │   │   ├── IPool.sol             # Aave V3
│   │   │   ├── IUniswapV2Pair.sol
│   │   │   ├── IUniswapV3Pool.sol
│   │   │   └── IDexRouter.sol
│   │   └── libs/
│   │       ├── UniV2Math.sol         # constant-product math
│   │       └── UniV3Math.sol         # tick math (or use ticklens)
│   ├── test/                         # forge tests + invariant tests
│   ├── script/                       # deploy scripts
│   └── foundry.toml
│
├── searcher/                  # Rust workspace (cargo)
│   ├── crates/
│   │   ├── searcher-core/            # opportunity detection, sim, math
│   │   ├── searcher-net/             # RPC + Realtime WS client
│   │   ├── searcher-pools/           # pool registry, state cache
│   │   ├── searcher-exec/            # tx building, signing, submission
│   │   └── searcher-bin/             # main binary
│   └── Cargo.toml
│
├── config/
│   ├── mainnet.toml                  # addresses, RPC, gas params
│   └── testnet.toml
├── ops/
│   ├── docker/
│   └── monitoring/                   # Prometheus scrape, alerts
├── README.md
└── CLAUDE.md
```

---

## On-Chain: `ArbExecutor.sol`

**Single contract, owner-gated, holds no funds between txs.**

Key surface:
```solidity
function startArb(
    address asset,           // flash-loaned token (e.g. USDC)
    uint256 amount,          // loan size
    bytes calldata route,    // packed swap legs
    uint256 minProfit        // wei of `asset`; revert if not met
) external onlyOwner;

function executeOperation(   // Aave V3 callback
    address asset,
    uint256 amount,
    uint256 premium,         // 0.05% Aave fee
    address initiator,
    bytes calldata params
) external returns (bool);
```

Inside `executeOperation`:
1. Decode `route` → ordered list of `(dex_id, pool, tokenIn, tokenOut, fee)`.
2. Execute swaps with **direct pair calls** (skip routers — saves ~30-50k gas/leg). For Uni V2, `pair.swap()` with pre-computed `amountOut`. For V3, `pool.swap()` with packed callback.
3. After final leg: `require(balanceOf(asset) >= amount + premium + minProfit)`.
4. `IERC20(asset).approve(POOL, amount + premium)` and return true.

Design rules:
- **No external calls outside the route** — no fee-on-transfer tokens v1, no rebasing.
- **Pre-computed `amountOut` off-chain**, on-chain just verifies via `require` — saves gas on quote() simulations.
- **No router dependencies** — direct pool interaction means one less attack surface and lower gas.
- **Owner-only** start, `onlyAavePool` guard on callback, `initiator == address(this)` check.
- **Sweep function** (owner) for any dust left from rounding.

Aave V3 Pool address (MegaETH) — **must verify at deploy time** from Aave governance posts; do not hardcode without confirmation.

---

## Off-Chain Searcher (Rust)

### Crates / dependencies
- **alloy** (`alloy-provider`, `alloy-signer`, `alloy-rpc-client`, `alloy-sol-types`) — modern Rust EVM stack, lower-level than ethers-rs, sub-ms ABI codec.
- **tokio** with `--features rt-multi-thread`, pinned worker count.
- **tokio-tungstenite** for the Realtime WS.
- **dashmap** for the pool state cache (lock-free reads).
- **rust_decimal** or `ruint` (U256 fixed-point) for swap math — **never f64**.
- **tracing** + `tracing-subscriber` for structured logs; **prometheus** for metrics.

### Module responsibilities

**`searcher-net`** — RPC + Realtime
- Persistent WS connection to MegaETH Realtime API (`wss://...`) subscribing to:
  - State diffs touching watched pool addresses
  - New mini-block headers
- Reconnect with exponential backoff; surface gaps to core for resync.
- A second persistent **HTTP/2 client** with connection keep-alive for `eth_sendRawTransaction`.

**`searcher-pools`** — Pool registry & state cache
- At boot: load pool list from `config/mainnet.toml` (≤ 200 pools across 2-3 DEXs initially).
- For each pool: store reserves (V2) or `slot0 + active liquidity + tick bitmap window` (V3).
- On state diff: parse storage slot updates and patch cached state in-place. **Critical: parsing must be O(1) per diff** — pre-compute slot → pool field maps at startup.
- Fallback: full `eth_call` resync on diff parse error or gap.

**`searcher-core`** — Opportunity detection & simulation
- On every state update of a watched pool, recompute reachable cycles touching that pool.
- Two strategies in v1:
  1. **2-DEX, same pair** — token X/Y on DEX A vs DEX B; if implied price gap > fees+gas+slippage, arb.
  2. **Triangular** — A→B→C→A within one DEX or across two.
- Pre-compute a **token graph** at boot; on update, only re-evaluate cycles containing the changed pool.
- **Simulation** is pure Rust math (no `eth_call` — too slow): use canonical V2/V3 swap formulas matching the on-chain libs.
- **Profit threshold**: `gross_out − loan_amount − aave_premium − gas_cost − safety_margin ≥ minProfit`.

**`searcher-exec`** — Transaction build & submit
- Pre-sign template txs with placeholder calldata at boot; only patch the route + minProfit fields per opportunity.
- Maintain a small **nonce manager** (single account v1) — gap-free, with rebroadcast on stuck.
- Submit to **multiple sequencer-region RPC endpoints** in parallel (race), accept first success.
- Track inclusion via Realtime API; on revert, log full receipt + cached pool state at decision time for post-mortem.

**`searcher-bin`** — wiring, config, graceful shutdown, metrics endpoint.

### Latency budget (target end-to-end)
| Stage                          | Budget |
|--------------------------------|--------|
| WS frame → parsed diff         | < 200 µs |
| Cache patch                    | < 50 µs |
| Cycle re-eval (~200 pools)     | < 500 µs |
| Profit check + tx build        | < 200 µs |
| Sign + send (network excluded) | < 100 µs |
| **Total in-process**           | **< ~1 ms** |

Network RTT to sequencer dominates → **co-locate the bot in the same region as the sequencer** (whatever the team publishes; otherwise nearest cloud region).

---

## Profitability Math (must-have correctness)

For each candidate cycle producing `amountOut` from `loan = amountIn` of asset `A`:

```
gross_profit       = amountOut − loan
aave_premium       = loan × 0.0005          // 5 bps
estimated_gas_cost = gas_units × gas_price  // gas_price in MegaETH gwei
safety_margin      = max(min_abs, gross_profit × 0.05)   // 5% buffer or floor
net_profit         = gross_profit − aave_premium − estimated_gas_cost − safety_margin
fire if net_profit ≥ minProfit_config
```

`gas_units` is benchmarked from forge tests per route shape (2-leg V2/V2, V2/V3, 3-leg, etc.) and stored as a constant per shape.

---

## Risk Controls

- **`minProfit` enforced on-chain** — reverts beat losses, always.
- **Per-tx loan cap** (config): start small (e.g. $5k notional) until inclusion + profitability metrics stabilize.
- **Daily loss kill-switch** in the bot: halt submissions if cumulative gas burn > X/day with no successes.
- **No mempool leaks**: don't broadcast to public mempool — direct sequencer RPC only (MegaETH model already implies this).
- **Token allowlist** — only loan + swap allowlisted tokens (skip fee-on-transfer, rebasing, tax tokens).
- **Reentrancy guard** on `ArbExecutor.executeOperation` even though Aave guards it; defense in depth.
- **Owner = multisig** in production. Hot key only signs txs, cannot upgrade or sweep.

---

## Phased Roadmap

### Phase 0 — Setup (1-2 days)
- Init Foundry + Cargo workspaces in this repo.
- `config/mainnet.toml`: confirmed addresses for Aave V3 Pool, top-3 DEX factories, base tokens (USDC, WETH, USDe).
- CI: `forge test`, `cargo test`, `cargo clippy --deny warnings`, `forge fmt --check`.

### Phase 1 — Executor contract + tests (3-5 days)
- `ArbExecutor.sol` with 2-leg and 3-leg routes for V2-style pools.
- Forge fork-test against MegaETH mainnet RPC: real Aave flash loan + real DEX swaps end-to-end.
- Invariant tests: any reachable path that doesn't meet `minProfit` reverts.
- Gas snapshot per route shape → feeds `gas_units` table in searcher.

### Phase 2 — Searcher MVP, read-only (3-5 days)
- WS connection, state cache, pool registry for **2 DEXs, ~50 pools, USDC/WETH/USDe pairs only**.
- Detector logs would-be opportunities to disk; no submissions.
- Validate: Rust-simulated `amountOut` matches `eth_call` quote within 1 wei across 1k random inputs.

### Phase 3 — Live submission, conservative (3-5 days)
- Wire executor calls; submit with `minProfit ≥ $1` floor, loan cap $1k.
- Metrics: opportunities/sec, sim→fire latency, inclusion rate, revert reasons.
- Run 48 h on testnet (or low-cap mainnet) before scaling.

### Phase 4 — Scale & optimize
- Add V3 pool support (tick math).
- Add 3rd DEX, extend pool registry to ~200.
- Triangular cycles within a DEX.
- Tune co-location, pre-signed tx templates, multi-RPC racing.
- Move owner key to multisig; introduce dedicated hot signer.

### Phase 5 — Beyond v1 (only if economics warrant)
- Cross-chain arb (bridges add latency — likely unprofitable unless very wide).
- Just-in-time liquidity / sandwich-style strategies (if MegaETH MEV rules permit).
- Multiple concurrent signers for parallel inclusion.

---

## Critical Files to Be Created

- [contracts/src/ArbExecutor.sol](contracts/src/ArbExecutor.sol) — core executor
- [contracts/src/libs/UniV2Math.sol](contracts/src/libs/UniV2Math.sol) — constant-product math used by both contract and Rust sim
- [contracts/test/ArbExecutor.t.sol](contracts/test/ArbExecutor.t.sol) — fork-tests against MegaETH
- [searcher/crates/searcher-core/src/cycle.rs](searcher/crates/searcher-core/src/cycle.rs) — cycle enumeration & profit math
- [searcher/crates/searcher-pools/src/state.rs](searcher/crates/searcher-pools/src/state.rs) — pool state cache + diff applier
- [searcher/crates/searcher-net/src/realtime.rs](searcher/crates/searcher-net/src/realtime.rs) — Realtime WS client
- [searcher/crates/searcher-exec/src/submit.rs](searcher/crates/searcher-exec/src/submit.rs) — tx build + multi-RPC race
- [config/mainnet.toml](config/mainnet.toml) — addresses, RPC URLs, caps

---

## Verification Plan

**Contracts**
- `forge test -vvv` — unit + invariant.
- `forge test --fork-url $MEGAETH_RPC --fork-block-number <recent>` — end-to-end with real Aave + real DEX state.
- `forge snapshot` — gas regressions.

**Searcher**
- `cargo test` — math parity tests (Rust sim ↔ on-chain `eth_call` quote, 1k random inputs).
- Replay test: feed a captured stream of state diffs → assert detector emits known-good opportunities.
- Latency benchmarks via `criterion`: each pipeline stage stays inside its budget.

**End-to-end**
- Dry-run mode (Phase 2): compare logged opportunities against any independently observed arb that landed on-chain.
- Canary mainnet run with $1k cap, `minProfit ≥ $1`, 48-hour soak.
- Metrics dashboards: opportunities, sim→submit latency p50/p99, inclusion rate, gross & net P&L, revert breakdown.
- Kill-switch drill: confirm bot halts on hitting daily loss cap.

---

## Development Model

Use **Claude Opus 4.7 at high effort (not max)** for implementation work in this project.

## Open Items to Confirm Before Phase 1

1. Exact **Aave V3 Pool address on MegaETH** (verify from latest Aave governance post / Aave UI).
2. **Top 2-3 DEXs to target** — DefiLlama MegaETH chain page has live TVL/volume rankings; pick by 24h volume, not TVL.
3. **Realtime API authentication** model and rate/throughput limits — affects co-location and connection-pooling design.
4. **Sequencer RPC endpoints** to race against (region & count) — affects deployment topology.
