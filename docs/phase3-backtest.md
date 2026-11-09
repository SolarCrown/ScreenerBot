# Phase 3 — Historical backtest

The engine reads `research.db` and writes `backtest_runs` and `backtest_trades` only. There is no forward-test table. Each run starts from its own `starting_cash_sol`. Opening the store latches research mode, so the process cannot sign, send, or parse a key.

A signal uses bars closed at that timestamp. A fill is the next bar's open, and only when that bar's timestamp is exactly one timeframe later. Fees are the app's 0.5% (`PLATFORM_FEE_BPS` = 50) plus 2,000,000 lamports network reserve and 10,000 lamports priority (50,000 micro-lamports × 200,000 CU). Slippage is constant-product impact against a quote reserve observed at or before the fill. Missing reserves, gaps, rugs, and candleless mints are rejections or unresolved opens. Ending cash is cash on hand. Open positions are not marked to market. The run id is the SHA-256 of config hash, data hash, and strategy id. The seed is stored and is not used to draw a fill.

## Sample

One keyless GeckoTerminal request on 9 November 2026:

`GET /networks/solana/pools/58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2/ohlcv/day?aggregate=1&limit=1000`

Raydium SOL/USDC. 184 daily candles, 2026-05-10 through 2026-11-09. No liquidity rows. The page was stored without a pool-creation time, so the mint's born time is the ingest time, which is after the last bar.

Config: `CloseRise` lookback 1, `min_pct` 0, starting cash 1 SOL, the fees above, seed 1.

| Metric | Value |
| --- | --- |
| Fills | 0 |
| Rejected | 85, all `not_yet_listed` |
| Unresolved opens | 0 |
| Ending cash | 1 SOL (unchanged) |
| Data hash | `eb726e6fb8aaa476f59f5581e7d9dd36fb46645b896f09fd6a2e61e2932d1e47` |

That is a coverage result. It is not a statement that a strategy made or lost money. No bar was eligible, so the run never reached a reserve check. A later page that includes a point-in-time listing time and a reserve observed at or before the fill can be filled by the same engine. Fixture tests cover that path. Free OHLCV still does not include historical reserves, so a reserve stamped at fetch time cannot fill earlier bars.

## Tests

`cargo nextest run --all-targets --no-tests=fail`: 3886 passed, 115 skipped, exit 0. The new default tests are the 12 in `research::simulate::tests` (next-open fill, survivorship, gaps, stop before take-profit, trailing stop, fees and slippage, reserve cap, DCA and max hold on an independent bankroll, higher-timeframe close, repeatable hashes, research-mode refusal). The ignored sample above is the extra skip and was run once.
