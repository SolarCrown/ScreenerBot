# Phase 4 — Strategy research on the free memecoin sample

Each runnable rule set has its own 1 SOL bankroll. The file is `data/research-memecoin-sample.db` (PR #5). Opening that store latches research mode, so this process cannot sign or send. No wallet-level fills exist, so smart money and wallet copy are not run.

These numbers are one pass over a thin file. They are **not statistically reliable** and they are **not a claim that any rule makes money**.

## Data the rules actually saw

All runnable rules use the 1-minute bars. That is the only timeframe with bars after the fetch-time reserves (first positive reserve 2026-11-09 07:41 UTC). A 5m, 15m, or 1h entry would be rejected as `no_liquidity` for lack of an earlier reserve. No extra history was pulled.

| | |
| --- | --- |
| Mints with 1m bars | 59 (71 other mints in the file have no 1m bars and are counted as `no_candles`) |
| 1m span | 2026-11-08 04:16 to 2026-11-09 08:02 UTC (27.8 hours) |
| Sufficiency rule | At least 30 closed trades, 20 tokens bought, and 7 days of bars |
| Result | Every rule fails that rule. None are sufficient. |

A signal uses only the closed bar. The fill is the next bar's open, with the 0.5% fee, the 2,000,000 lamport network reserve, the 10,000 lamport priority fee, and constant-product slippage against a reserve observed at or before the fill. A position still open on the last bar is force-closed at that bar's close with the same costs, or written off if the sell cannot be filled. Unrealized is then zero. Born time is `pool_created_at`. A rugged, drained, or candleless mint stays in the universe. Missing data is not a profit.

The reserve is still one snapshot at fetch time. Almost every earlier bar is `no_liquidity`. The trades below are that short tail, and the fees are large next to a 0.05 SOL or 0.01 SOL order.

## Results

Start is 1 SOL for each row. Unrealized is 0 after the terminal mark. Return is ending equity over that 1 SOL.

| Strategy | End equity | Realized | Return | Trades | Wins | Win rate | Profit factor | Max drawdown | Tokens | Sufficient |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Hold SOL/cash | 1.000000 | 0 | 0% | 0 | 0 | n/a | n/a | 0% | 0 | no |
| Momentum | 0.962950 | -0.037050 | -3.71% | 7 | 0 | 0% | 0 | 3.71% | 7 | no |
| Volume breakout | 0.995441 | -0.004559 | -0.46% | 1 | 0 | 0% | 0 | 0.46% | 1 | no |
| Mean reversion | 0.995540 | -0.004460 | -0.45% | 1 | 0 | 0% | 0 | 0.45% | 1 | no |
| Early entry | 0.865641 | -0.134359 | -13.44% | 8 | 1 | 12.5% | 0.24 | 13.44% | 8 | no |
| Liquidity analysis | 0.970492 | -0.029508 | -2.95% | 8 | 0 | 0% | 0 | 3.12% | 8 | no |
| Market regime | 0.969251 | -0.030749 | -3.07% | 6 | 0 | 0% | 0 | 3.07% | 6 | no |
| Buy and hold universe | 0.848988 | -0.151012 | -15.10% | 25 | 1 | 4.0% | 0.04 | 15.10% | 25 | no |
| Smart money | | | | | | | | | | blocked: no wallet data |
| Wallet copy | | | | | | | | | | blocked: no wallet data |

Holding cash is the benchmark that does nothing. Buy-and-hold spends 0.01 SOL per name on the first bar that can fill, then marks the last close. On this file every traded rule finished below the cash benchmark. That is the fee and the tail, not a ranking of the rules in the market.

### Rejects

`no_candles` (71) is the mints stored without a 1-minute series. `no_liquidity` is a bar before the reserve snapshot. `not_yet_listed` is a bar before `pool_created_at`. A rule that never signals does not write a reject.

| Strategy | missing_next_bar | no_liquidity | no_candles | not_yet_listed |
| --- | --- | --- | --- | --- |
| Hold SOL/cash | 0 | 0 | 71 | 0 |
| Momentum | 233 | 2348 | 71 | 0 |
| Volume breakout | 74 | 714 | 71 | 0 |
| Mean reversion | 161 | 1318 | 71 | 0 |
| Early entry | 2 | 703 | 71 | 0 |
| Liquidity analysis | 2 | 0 | 71 | 0 |
| Market regime | 126 | 1129 | 71 | 0 |
| Buy and hold universe | 1261 | 9649 | 71 | 52 |

Liquidity analysis requires a reserve of at least 5 SOL already observed at the signal bar, so it does not send orders into the earlier history. That is why its `no_liquidity` count is zero. It is not evidence those bars were tradable.

## Rules, and where they fail

**Momentum.** 1m close at least 1% above the close five bars earlier. Stop 8%, take-profit 12%, max hold 20 minutes, order 0.05 SOL. A 1% one-minute rise is mostly noise, and the stop plus the fee can both be larger than the signal.

**Volume breakout.** Closed-bar volume at least 2× the prior 10-bar average, and the close up. Stop 10%, take-profit 15%, max hold 15 minutes, order 0.05 SOL. The volume is pool quote volume, not a buyer count. One print can clear the multiple.

**Mean reversion.** Close at least 3% below the mean of the last 10 closes. Stop 6%, take-profit 4%, max hold 15 minutes, order 0.05 SOL. A memecoin drop is often the start of a rug, not a mean. The 4% target sits inside fees and slippage on a thin pool.

**Early entry.** Pool age at the signal at most 60 minutes, close at least 0.5% above the previous close (or this bar's open). Stop 12%, take-profit 20%, max hold 30 minutes, order 0.05 SOL. Most bars in this file are long after creation, so the rule rarely arms. The eight trades are still the reserve tail.

**Liquidity analysis.** Quote reserve observed at or before the signal is at least 5 SOL, and the close is at least 0.5% above the close three bars earlier. Stop 8%, take-profit 12%, max hold 20 minutes, order 0.05 SOL. The reserve is one fetch-time estimate. The gate only selects the tail. It does not see liquidity that left earlier.

**Market regime.** Same three-bar rise, and only when at least 55% of mints with a bar at that minute closed up. Stop 8%, take-profit 12%, max hold 20 minutes, order 0.05 SOL. Breadth is the other names in this file, not SOL. A quiet minute can flip it.

**Hold SOL/cash.** No entries.

**Buy and hold.** 0.01 SOL of each mint on the first fillable bar, held to the terminal mark. Chain fees mean a 1 SOL bankroll cannot buy the whole universe at the 0.05 size the other rules use. Fills still wait for a reserve, so this is not a hold from the first historical bar. Fifty-two signals were `not_yet_listed` because the bar was before pool creation.

**Smart money. Wallet copy.** Interface only. Status `blocked: no wallet data`. No trades, no P&L.

## Tests

`cargo nextest run --all-targets --no-tests=fail`: 3893 passed, 116 skipped, exit 0. That includes the terminal-mark, unsellable-mark, and breadth checks, and the check that this sample is marked insufficient.
