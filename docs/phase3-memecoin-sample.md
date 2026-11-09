# Memecoin sample before Phase 4

Point-in-time free-source sample stored in `data/research-memecoin-sample.db`. It is a capped set, not the market. The sanity backtest is one path through that file. It is not a claim that a strategy made or lost money.

Collected 9 November 2026. GeckoTerminal lists (`new_pools`, `trending_pools`, `pools`, and dex pages for pump-fun, pumpswap, raydium, meteora, orca, meteora-damm-v2) plus DexScreener pair batches and a Rugcheck report per mint. `pool_created_at` is the mint born time. OHLCV is `currency=token` (quote-token price) and one page of at most 1000 bars. Quote reserves are the fetch-time snapshot only: GeckoTerminal's pool USD reserve divided by two and by the quote USD price, and DexScreener `liquidity.quote` when that batch returned one. A reserve is not copied backward onto earlier bars.

The Pump.fun frontend API returned 403. Bonding-curve rows are GeckoTerminal `pump-fun` pools. One list call, `dexes/pump-fun/pools?page=4`, returned 429 and was skipped. A first pass aborted on a duplicate candle timestamp; the retry kept stored pages and filled the rest. The file below is both passes.

## What was stored

| | |
| --- | --- |
| Pools | 137 |
| Mints | 130, all with `pool_created_at` |
| Venues | 52 bonding curve, 28 PumpSwap, 50 other (Raydium, Meteora, Orca) |
| DEX ids | pump-fun 52, pumpswap 28, meteora-damm-v2 26, meteora 12, raydium 11, orca 5, meteora-dbc 2, raydium-launchlab 1 |
| Pool creation | 2023-04-22 through 2026-11-09 |
| Rugcheck `rugged` | 0 of 130 |
| Latest USD reserve under $1 | 12 of 130 (3 of those are $0) |
| Open gap rows | 2009 `hole` (successive candles farther apart than one bar). No `fetch_failed` left open |

| Timeframe | Mints | Candles | First bar (UTC) | Last bar (UTC) |
| --- | --- | --- | --- | --- |
| 1m | 59 | 11093 | 2026-11-08 04:16 | 2026-11-09 08:02 |
| 5m | 30 | 7130 | 2026-11-03 11:40 | 2026-11-09 08:00 |
| 15m | 20 | 5106 | 2026-10-29 17:00 | 2026-11-09 08:00 |
| 1h | 20 | 3095 | 2026-09-28 10:00 | 2026-11-09 08:00 |

Each pool was given one timeframe, up to 100 pools in a pass. Status is `active` for 117 mints and `unknown` for 13. Rugcheck did not flag a rug in this slice, so "died" here means a reserve under $1, not a Rugcheck rug. GeckoTerminal's list pages are mostly pools that still have a reserve.

## Sanity backtest (not a performance result)

Engine from Phase 3, timeframe 1m, `CloseRise` lookback 1 with `min_pct` 0, starting cash 2 SOL, order 0.02 SOL, 0.5% fee, 2,000,000 lamport network reserve, 10,000 lamport priority fee. Seed 1. Data hash `034f26375f78840b36c94425a405e5ab8a4753d34fccf938b791415bbcb28f88`.

| | |
| --- | --- |
| Fills | 21 (16 buys, 5 sells) |
| Sells | 4 stop-loss, 1 take-profit |
| Open at the last bar | 11 |
| Rejected | 5597 |
| Ending cash | 1.7124940261578012 SOL |

Reject reasons: `no_liquidity` 4969, `missing_next_bar` 545, `no_candles` 71, `not_yet_listed` 1. The single `not_yet_listed` is the born-time check. The 4969 `no_liquidity` rows are bars before the fetch-time reserve. The 21 fills are on the 1-minute tail after that snapshot. Ending cash is lower on this path because of fees and the four stops. That number is this run only.

`cargo nextest run --all-targets --no-tests=fail`: 3889 passed, 116 skipped, exit 0. The live sample test is one of the skips and was run to produce this file.
