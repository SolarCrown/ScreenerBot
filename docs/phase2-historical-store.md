# Phase 2 — Historical store and research mode

This note records what Phase 2 added, what was measured, and what the free sources do not provide. It is not a backtest and it does not claim any strategy made money.

## Research mode

`engage_research_mode` latches a process-wide flag. It is not task-local. A `tokio::spawn` after the latch still sees it. There is no function that turns it off.

While the latch is on:

- `parse_private_key`, decrypt, export, and the trading-key accessors return `ResearchModeRefused` before they read the wallet database.
- A key already sitting in the in-memory cache is dropped when the latch engages. Accessors check the latch again before they return a copy.
- `sendTransaction` is refused in the RPC manager and on the service relay. Reads such as `getBalance` are still allowed, so a collector can use RPC.
- Swap execution returns `NotSubmittedReason::ResearchMode` before any router is asked.

Opening `ResearchStore` engages the latch. The normal trading process only runs the schema step inside `open_all`. That step does not engage the latch, so a trading run that has not opened the research store can still sign.

Running a collector or opening the research store inside the trading process disables signing until that process exits. That is the hard block. The signer code is still in the binary for a process that never engages research mode.

## Store

`research.db` is a catalog ledger store, separate from `ohlcvs.db` and `tokens.db`. Its tables are `datasets`, `ingest_runs`, `source_health`, `mints`, `pools`, `candles`, `liquidity_observations`, and `gaps`. Retention rules keep every one of them. Nothing in the schema deletes from `mints`.

A payload is stored under the blake3 hash of its bytes. Inserting the same bytes again does not add a second dataset or a second copy of its candles, and the hash does not change.

Candle rows use the timestamp from the payload. `fetched_at` is when this process stored the page. DexScreener liquidity rows set `source_had_event_time = 0` because that API is a snapshot. `pairCreatedAt`, when present, is stored as pool-creation metadata, not as a candle.

An empty candle page, a bonding-curve mint with no candles, a hole larger than one bar, and a rugged or abandoned or untradeable status are gap rows. `tradeable_mints` requires status `active`, at least one candle in the window, and no unfilled blocking gap. A missing series is not a flat profitable path.

Lifecycle status `rugged`, `abandoned`, and `untradeable` is not replaced by a later candle page. Pump.fun bonding curves are a venue (`bonding_curve`), including mints that have not graduated. `pumpswap` is recorded as `graduated_amm`.

`source = "birdeye"` is accepted by the same candle ingest. This build does not call Birdeye.

## What one keyless GeckoTerminal session returned

Calls below were made on 9 November 2026 against `https://api.geckoterminal.com/api/v2`, with the `Accept: application/json;version=20230203` header. They are one sample, not a contract.

| Request | Result |
| --- | --- |
| `GET /networks/solana/new_pools?page=1` | 20 pools. Several had `dex` id `pump-fun` (bonding curve) and one had `pumpswap`, with `pool_created_at` in the same minute as the call. |
| Pump.fun pool `Fb2yNKjJCgtwGWTHBHyzVftbuQDiQyCUXm1cPjLEngXx`, 1m, `limit=1000` | 2 candles, 2026-11-09 06:41 and 06:42 UTC. |
| Raydium SOL/USDC `58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2`, 1m, `limit=1000` | 1000 candles, 2026-11-08 14:04 through 2026-11-09 06:42 UTC. |
| Same pool, 1m, `before_timestamp` of that oldest bar | Another 1000 candles, back to 2026-11-07 21:23 UTC. |
| Same pool, 1d, `limit=1000` | 184 candles, 2026-05-10 through 2026-11-09. |
| Same pool, 1d, `before_timestamp=1778371200` | 181 candles, 2025-11-11 through 2026-05-10. |

The walk stopped there. Both series still had a full or nearly full page, so this session did not find the start of Raydium SOL/USDC history. A response never contained more than 1000 candles. A brand-new bonding curve had only the minutes since it was indexed.

The research collector waits 6 seconds between GeckoTerminal calls (10 per minute, the figure on GeckoTerminal's own API page). CoinGecko's keyless page says the shared pool can be nearer 5 calls per minute and moves with load. A 429 is retryable. The live trading client still has its own 30/min limiter. Research calls pass through the research pace first when the `Collector` is used. Parsing a fixture does not wait.

## What the free sources still cannot do

- DexScreener's documented API is a current snapshot. This store keeps the snapshots we fetch. It does not reconstruct candles DexScreener never sent.
- Rugcheck's `rugged` flag is a point-in-time report. It does not say when the rug happened.
- Public RPC can observe new Pump.fun mints from now on if a caller passes them to `observe_bonding_curve_mint`. This phase does not scan the Pump.fun program back to genesis. A mint nobody has fetched is absent until something observes it. Absence is not "safe".
- GeckoTerminal OHLCV is per pool, and only for pools it has indexed. A bonding curve that died before it was indexed has no bars. The store can still hold the mint and a `no_candles` gap.
- Daily and 1-minute depth beyond the two pages above was not measured. Do not treat 2025-11-11 or 2026-11-07 as the earliest data the host will ever return, and do not treat those pages as complete history.
- Per-strategy paper bankrolls are not in this phase. Forward-test books are Phase 5. The research store is separate from `paper_positions.db` so that work does not have to share one cash balance.

## Tests

Offline suite on this branch, rustc 1.98.1, cargo-nextest 0.9.146:

```text
cargo nextest run --all-targets --no-tests=fail --profile default
Summary [86.929s] 3874 tests run: 3874 passed, 114 skipped
```

The 114 skipped tests are the ignored live, mainnet, and real-database tiers. They were not run. The GeckoTerminal numbers above are the only live HTTP calls made for this phase, and they were not part of the test suite. Phase 1's run on the parent commit was 3858 passed and 114 skipped. The 16 new tests are the research store, gap marking, GeckoTerminal pace, and the research-mode refusal across `tokio::spawn`.
