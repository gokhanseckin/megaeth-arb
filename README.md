# megaeth-arb

Flash-loan arbitrage bot for [MegaETH](https://www.megaeth.com/). Borrows from **Aave V3**, swaps across AMM pools (Uniswap V2/V3-style), and reverts unless `minProfit` is met — atomic-or-nothing.

## Why MegaETH

- **10ms mini-blocks**, single sequencer, [Realtime API](https://docs.megaeth.com/realtime-api) streams state diffs.
- The "speed game" is reaction-time to sequencer state diffs, not public-mempool gas auctions.
- Aave V3 deployed day-one (flash loans available).

## Layout

| Path | What |
|------|------|
| [contracts/](contracts/) | Foundry workspace — `ArbExecutor.sol` (Aave receiver, multi-DEX router, on-chain `minProfit` enforcement) |
| [searcher/](searcher/) | Cargo workspace (Rust) — Realtime WS client, pool state cache, opportunity detector, tx submitter |
| [config/](config/) | Per-network TOML — RPC, addresses, caps |
| [ops/](ops/) | Docker, monitoring |

## Quick start

Prereqs: [Foundry](https://book.getfoundry.sh/getting-started/installation), Rust (`rustup`).

```bash
# Build
cd contracts && forge build && cd ..
cd searcher && cargo build --release && cd ..

# Test
cd contracts && forge test -vvv && cd ..
cd searcher && cargo test --workspace && cd ..

# Run searcher (dry-run)
cd searcher
MEGAETH_RPC=... MEGAETH_WS=... MEGAETH_PK=... \
  cargo run --release -p searcher-bin -- --config ../config/mainnet.toml --dry-run
```

## Status

**Phase 0 — setup.** See [CLAUDE.md](CLAUDE.md) for architectural conventions and the full plan at `~/.claude/plans/purring-plotting-lollipop.md`.

## Safety

This is experimental software that signs transactions and moves funds. Don't run it on a wallet you can't afford to lose. The on-chain `minProfit` revert is the safety floor — never bypass it.
