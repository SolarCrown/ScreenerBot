# Phase 1 — Repository audit and architecture report

Date of audit: 9 November 2026.

Repository inspected: this checkout of `SolarCrown/ScreenerBot`, commit `f26d8524f73fed020bc646d64fc7ec22e7872477` (`chore(release): bump version to 0.2.25`). Package version in `Cargo.toml` is `0.2.25`. The upstream project described by `README.md` is `farfary/ScreenerBot`.

Scope: Phase 1 only. No functional code was changed. Phases 2–8 below are a plan, not an implementation.

Method: the report is based on reading the Rust, dashboard, and test sources, on the official vendor pages fetched during this audit, and on tests that were actually run in this environment. Where a vendor page could not be fetched, the item is marked unverified. Nothing here is a profitability claim.

## 1. What the repository is today

ScreenerBot is a self-hosted Solana trading application, not a research platform. It discovers tokens, filters them, prices pools from on-chain reserves, evaluates user-authored rule trees, and can paper-trade or submit real swaps. A local dashboard, a Telegram bot, an LLM assistant, and a stdio MCP server sit on top of that engine.

The spec’s target — historical backtests, survivorship-aware token history, independent forward-test books, a research leaderboard, and a coordinator that rejects overfit strategies — is not present. Several nearby pieces exist and should be reused rather than rewritten: the condition evaluators, the paper-fill types, the pool decoders, the provider clients, the SQLite catalog, and the dashboard shell.

## 2. Languages, frameworks, and how it runs

| Layer | What it is |
| --- | --- |
| Engine | Rust 2021 library and binary. `src/lib.rs` is crate `screenerbot`. `src/main.rs` calls `screenerbot::run::boot()`. |
| Toolchain in this tree | `rust-toolchain.toml` pins `1.98.1`. CI (`.github/workflows/ci.yml`) installs `1.98.1`. The README badge still says Rust 1.89. The file and CI are the ones that match this checkout. |
| Async / HTTP / WS | Tokio, reqwest (rustls), tokio-tungstenite. |
| HTTP API and dashboard | Axum 0.8. HTML, CSS, and JavaScript are embedded from `src/webserver/templates/` and served by the binary. There is no separate SPA framework. |
| Desktop shell | Electron under `electron/`. Optional. Headless mode does not need it. |
| Persistence | SQLite through rusqlite, bundled SQLite, r2d2 pools. |
| Chain SDK | `solana-sdk` 2.3.1, `solana-client`, SPL token crates. `alloy` is present behind `src/chains/evm`, but `ChainId` in `src/chains/types.rs` has only `Solana`. No EVM chain is wired into trading. |
| Dashboard tooling | Node. `package.json` declares `engines.node >= 24.8.0`. CI uses Node 24. This audit machine had Node `v22.23.4`. Lint and the Node tests still passed on 22; that is an environment mismatch, not a code change. |
| License | Business Source License 1.1 (`LICENSE`, `Cargo.toml`). |

How a run starts, from `README.md` and `src/main.rs`:

- `cargo build --release` produces `target/release/screenerbot`.
- Headless, the dashboard binds to `127.0.0.1:8080` unless `--host` / `--port` change it.
- Desktop builds wrap that binary in Electron. Node is required for the shell, not for the engine.
- First boot is a setup wizard (wallet plus RPC) or Explore Mode. Explore Mode runs discovery and filtering without a wallet. Full mode starts trading services. This is described in `README.md` and implemented around the webserver startup page and initialization routes.
- Settings live in `data/config.toml` under the platform data directory. The schema is generated from `src/config/schemas/`.

This audit compiled the crate as part of the test build. It did not start the server, did not open the dashboard in a browser, and did not call mainnet.

## 3. Architecture

`src/lib.rs` exposes the subsystems. The runtime shape, confirmed in code rather than only in the README diagram:

1. **Discovery.** `src/tokens/discovery.rs` polls DexScreener, GeckoTerminal, Rugcheck, Jupiter, CoinGecko, and DefiLlama. New mints are written to `tokens.db` and announced on an internal event bus.
2. **Filtering.** `src/filtering/stage.rs` runs, in order: meta, symbol, the chain’s on-chain stage, market, Rugcheck, optional LLM. The first rejection wins. Rejections are stored.
3. **Pools and live price.** `src/pools/service.rs` sets `FETCH_INTERVAL_MS` to `500`. Eleven native decoders under `src/chains/solana/pools/decoders/` read reserves over the configured Solana RPC: Raydium CLMM, CPMM, and legacy AMM; Orca Whirlpool; Meteora DAMM, DLMM, and DBC; Pump.fun AMM and bonding curve; FluxBeam; Moonit.
4. **Candles.** `src/ohlcvs/` fetches and caches OHLCV for monitored tokens. This is a chart and indicator cache, not a research warehouse. See section 5.
5. **Strategies.** `src/strategies/` evaluates boolean rule trees against a live `EvaluationContext` (price, market snapshot, candle bundle, optional position). It does not simulate a past path.
6. **Trader.** `src/trader/` admits entries, evaluates exits (stop, trail, take profit, time, strategy signal, safety), and dispatches buys and sells. Paper and live are separate books.
7. **Swaps.** `src/swaps/` quotes direct-pool, Jupiter, and Raptor routes and can sign and submit the winner. Paper code is kept off that path by a task-local scope and a source scan.
8. **Services.** `src/services/` starts named services in dependency order (tokens, pools, pool pricing, OHLCV, filtering, trader, copy trading, wallet, wallet watch, transactions, webserver, telegram, assistant, connectivity, and others).
9. **Surfaces.** Axum routes, embedded pages, Telegram (`teloxide`), and MCP (`rmcp`) over the same tool registry.

`ChainId::ALL` is `[Solana]`. Multi-chain columns exist in several stores so a later chain can be added. They do not mean another chain trades today.

## 4. Market data sources in this code

All of the following are implemented clients, not just README claims.

### HTTP market and discovery APIs

| Source | Where | Role in this repo | Limits encoded in the repo |
| --- | --- | --- | --- |
| DexScreener | `src/apis/dexscreener/` | Profiles, boosts, pair and token market snapshots. Base `https://api.dexscreener.com`. | Client comments: 300/min on pair lookups, 60/min on profiles and boosts, batches of 30. No candle client. |
| GeckoTerminal | `src/apis/geckoterminal/`, `src/ohlcvs/fetcher.rs` | New and trending pools, token pools, pool OHLCV. Base `https://api.geckoterminal.com/api/v2`. | `RATE_LIMIT_PER_MINUTE = 30`. OHLCV `limit` clamped to 1000 (`MAX_CANDLES_PER_REQUEST`). `before_timestamp` is supported by the fetcher. |
| Rugcheck | `src/apis/rugcheck/` | Security reports and discovery lists (new, recent, trending, verified). | 60/min in the client comments. Report includes a rugged flag, authorities, holders, risks. |
| Jupiter token API | `src/apis/jupiter/` | Recent, organic, traded, and trending token lists. Quote path for swaps is separate, under `src/chains/solana/swaps/routers/jupiter.rs`. | Discovery client treats the public token API as unlimited. Swap quotes are a different host and are part of execution. |
| CoinGecko | `src/apis/coingecko/` | Coin list, used to extract Solana addresses. Optional `COINGECKO_API_KEY`. | One list endpoint in the module README. |
| DefiLlama | `src/apis/defillama/` | Protocol list for Solana addresses, plus a current SOL price helper. | No historical series in this client. |
| Solana Tracker | `src/chains/solana/apis/solana_tracker/` | Optional OHLCV fallback. Base `https://data.solanatracker.io`. | Off unless `[ohlcv.sources.solana_tracker].enabled` and an API key are set. Default in `src/config/schemas/ohlcv.rs` is `enabled: false`. Credit-based; current prices were not re-checked. |
| ScreenerBot data service | `src/data_server/mod.rs` | Optional first hop for candles, pool registry, Rugcheck reports, decimals, and, in service-access mode, chain reads. Default base `https://screenerbot.io/data`. | Requires an account session. Callers of market data fall back to the direct provider on refusal. Signed-out installs make no request. History depth of that service is not specified in this repo. |

Public GeckoTerminal rate limits published by CoinGecko are lower than the 30/min this client assumes. On 9 November 2026, [GeckoTerminal’s own API docs](https://api.geckoterminal.com/docs/index.html) say the public API is about 10 calls per minute, and [CoinGecko’s keyless-API page](https://docs.coingecko.com/docs/keyless-public-api) says about 5 calls per minute and that the keyless API is not for production polling. The app’s 30/min default is therefore optimistic against the current public host. A 429 backoff exists in the GeckoTerminal client.

### RPC and WebSockets

- Trading, balances, pool accounts, and transaction reads go through `src/rpc/` and `src/chains/solana/rpc/`. The user supplies endpoints. The RPC layer recognizes Helius, QuickNode, Triton, and Alchemy URLs and applies the matching configured rate limit (`src/rpc/provider/detection.rs`, `src/config/schemas/rpc.rs`). Helius is not a built-in historical warehouse here. It is one possible RPC vendor, plus a high-activity wallet-history path (`get_helius_successful_transactions_after` in `src/chains/solana/wallets/runtime.rs`).
- Wallet and transaction watching uses Solana `logsSubscribe` at `confirmed` commitment (`src/chains/solana/rpc/subscriptions/`). Cursor polling fills gaps after reconnect. That socket is chain RPC, not a browser socket.
- The dashboard does not expose an Axum WebSocket. Pages poll. `src/webserver/templates/scripts/core/poller.js` defaults to a 1000 ms interval, persisted as `pollingInterval`.

### What is not integrated

Birdeye, Bitquery, Dune, Flipside, and a raw-block indexer are not clients in this tree. Birdeye appears only as an outbound analytics link on a position (`src/webserver/routes/positions/types.rs`).

## 5. Historical data that is actually stored

There is no durable, bias-aware history suitable for the spec’s backtester.

### Candles

`src/ohlcvs/database/mod.rs` stores `ohlcv_pools`, `ohlcv_candles`, `ohlcv_gaps`, `ohlcv_monitor_config`, and `ohlcv_data_versions`. A candle row is chain, mint, pool, timeframe, timestamp, open, high, low, close, volume, source, and `fetched_at`.

`src/config/schemas/ohlcv.rs` sets `retention_days` default `7`, minimum `1`, maximum `30`, and `max_monitored_tokens` default `300`. `retention_days` bounds how far backfill is requested (`src/ohlcvs/monitor.rs`, `ensure_retention_window`). It is not a guaranteed archive. `src/database/catalog.rs` classifies the OHLCV store as `Cache` and marks `ohlcv_candles` evictable when storage housekeeping is over budget (`src/storage/engine.rs`).

`src/ohlcvs/README.md` is stale relative to the code. It still describes `ohlcv_1m` plus a separate aggregate table and a cold archive. The schema that ships is `ohlcv_candles` keyed by timeframe. Treat the Rust schema and config, not that README, as current.

Fetch order in `src/ohlcvs/fetcher.rs`: ScreenerBot data service, then chain candle feeds (Solana Tracker when enabled), then GeckoTerminal by pool address. GeckoTerminal OHLCV is per pool. Intervals with no swaps are a property of the upstream response. The local gap table records holes the monitor noticed. A missing series is not evidence that a token was safe or flat.

Timeframes the strategy engine and candle module share: 1m, 5m, 15m, 1h, 4h, 12h, 1d.

### Token universe

`tokens.db` is also a cache (`StoreClass::Cache`). Discovery inserts whatever the live feeds return. `src/tokens/database/retention.rs` deletes tokens that have not been used, together with their market, security, pool, and rejection rows. Favorites and manual blacklist rows are kept. Rejected and rugged tokens are therefore not a permanent universe. A token that died before this process was running, or that aged out of the cache, is simply absent.

Rugcheck’s `is_rugged` flag is stored on `security_rugcheck` while the token row exists. That is a point-in-time report, not a launch-to-death panel.

There is no table of pool-creation time, initial liquidity, liquidity time series, or “this mint existed and then became untradeable.” Live pool snapshots in `pools.db` and `token_pools` are current state.

### Trades and positions

`positions.db` is the live book. `paper_positions.db` is the simulated book. `transactions.db` stores decoded wallet and watched-wallet transactions (`StoreClass::Derived`). `copy_trading.db` stores copy tasks and their ledgers. These are operational ledgers for one running bot. They are not a market-data history, and they are not reproducible research runs tied to a frozen dataset version.

## 6. Strategy engine

`src/strategies/` is a live rule engine.

- A strategy row (`strategies` in `strategies.db`) is an entry or exit rule tree: id, name, type, enabled, priority, timeframe, `rules_json`, parameters, version, the entry strategies an exit strategy manages, mode `paper` or `live`, and a rules fingerprint plus `rules_changed_at`.
- Schema default for `mode` is `'live'` so rows written before mode existed stay live (`src/strategies/database.rs`). New strategies created by `POST /api/strategies` are forced to `TradingMode::Paper` (`src/webserver/routes/strategies/crud.rs`). Moving a strategy to live requires the exact confirmation string `ARM LIVE STRATEGY` (`src/trader/paper/mode.rs`).
- Evaluation is `evaluate_entry_strategies` / the exit counterpart in `src/strategies/mod.rs`. The engine walks the tree against the current context. Missing candle data returns an “unknown” result, not a pass (`is_data_unavailable`).
- Conditions implemented, by `condition_type()`:

  `PriceBreakout`, `PriceChangePercent`, `PriceToMA`, `VolumeSpike`, `CandleSize`, `ConsecutiveCandles`, `LiquidityLevel`, `Rsi`, `BollingerBands`, `MaCross`, `Vwap`, `BuyPressure`, `TransactionVelocity`, `VolatilityRange`, `PullbackFromHigh`, `TokenAge`, `TimeOfDay`, `PositionHoldingTime`, `PositionProfit`, `ProfitGiveback`, `DrawdownFromPeak`, `LiquidityChangeSinceEntry`, `LiquidityToSize`, `PositionAdds`.

- `POST /api/strategies/{id}/test` (`src/webserver/routes/strategies/testing.rs`) explains one evaluation against the live token or against a caller-supplied price. It waits up to 10 seconds for candles. It does not walk history.
- `strategy_performance` and `strategy_evaluation_totals` count live evaluations (pass/fail, latency, last signal). They are not backtest trades.
- `strategy_backtests` is created in `src/strategies/database.rs` with id, strategy id, start, end, trade counts, `total_profit_sol`, and `results_json`. A repository search finds no `INSERT` and no query that reads it. The table is unused. `src/database/catalog.rs` still classifies it as a ledger, so housekeeping would keep rows if any were ever written.
- `src/trader/templates.rs` is a set of exit-config presets (trailing stop, take profit, time override). It does not define the eight research families in the spec.

Exit policy that does exist, and that a later simulator can call if it is kept pure, lives in `src/trader/evaluators/`: stop loss, trailing stop, take profit, time override, time stagnation, DCA, strategy signals. Admission gates (force stop, period loss limit, connectivity, position caps, cooldown, blacklist, optional LLM) are in `src/trader/`. Those gates assume a running bot, not a historical clock.

The auto-trader master switch defaults to off (`trader.enabled = false` in `src/config/schemas/trader.rs`). Default trade size is 0.005 SOL and `max_open_positions` is 2. Stop loss and take profit default on. Trailing stop and DCA default off.

## 7. Paper trading, and real signing

This is the risk section. The repository can sign and submit real Solana transactions. It also has a real paper path that refuses to sign while it is in force. Both are true.

### Paper path

`src/trader/paper/mod.rs` states the invariant: paper code does not reach swap execution, key material, or a transaction send. `tests/paper_isolation.rs` scans the paper modules and fails if they name those symbols. `src/global/state.rs` sets a task-local `paper_scope`. Inside it:

- `src/rpc/manager.rs` refuses `sendTransaction`.
- `src/swaps/operations.rs` refuses swap execution.
- `src/chains/solana/accounts/keypair.rs` refuses `parse_private_key`.

The scope does not cross `tokio::spawn`. The paper modules are forbidden from spawning for that reason.

A current paper fill (`src/trader/paper/fill.rs`) is quote-based, not a candle replay:

- It requests a live router quote, waits (`DEFAULT_LANDING_DELAY_MS` is 1000, cap 30000), quotes again, and fills only if the landing output is at least the first quote’s minimum.
- Network fee must be present on the quote. Platform fee is treated as included in the output amount.
- A quote that does not match the request, or a quote that never arrives, does not fill (`QuoteUnfillable`, `QuoteUnavailable`, `MinimumOutputNotMet`).
- Older rows can be `PaperModel::Legacy` (pool-price estimate) or `Mark` (closed at the pool mark when a paper run is reset). New fills are `Quote`.

Paper positions live in `paper_positions.db`, separate from `positions.db`. Cash is one derived balance per wallet per paper epoch (`src/positions/paper/account.rs`): starting balance minus buys and fees plus sells, shared by every strategy on that book. Rounds remember `strategy_id`, so per-strategy stats can be filtered, but the bankroll is not independent. Copy-trading tasks are different: each new task is forced to paper (`CopyTaskInput::into_task` returns `ModeTransitionRequired` otherwise), and each task has its own paper book and P&L. Going live requires the exact phrase `ARM LIVE COPY TRADING`.

Paper fills still call live quote routers (Jupiter, Raptor, direct-pool quotes). They do not submit. They are not a historical backtest, and they are not reproducible after the quote window has passed.

### Live path — keys and signing

- Wallet secrets are parsed as base58 or a JSON byte array (`parse_private_key`) and stored encrypted with AES-256-GCM. The key is derived from a machine id (`src/secure_storage/encryption.rs`, salt `screenerbot-wallet-encryption-v1`). Setup routes accept `wallet_private_key` and can generate a key (`src/webserver/routes/initialization/`).
- `signing_keypair` in `src/chains/solana/swaps/routers/mod.rs` loads `configured_keypair()` before a router builds or signs. Direct pool, Jupiter, and Raptor each implement `build_and_submit` with a `Keypair`.
- The same admission, quote, swap, and position path serves the auto-trader (when a strategy is live and `trader.enabled` is on), manual buys and sells from the dashboard, and copy tasks that have been armed live.
- MCP tools `BuyTokenTool`, `AddToPositionTool`, `SellTokenTool`, and `ClosePositionTool` (`src/agent_control/tools/trading.rs`) call that manual API when the connection’s trading permission allows it. The agent does not receive the private key. The process signs locally. Audit redaction strips fields whose names look like secrets (`src/agent_control/audit.rs`). That is not the same as “this build cannot trade.”
- Integration tests include ignored live swap simulations and an ignored test whose name is `mainnet_manual_buy_then_sell_lifecycle`, documented as spending real SOL when `SB_TEST_MAINNET_SWAP=1` and `SB_TEST_WALLET` are set (`tests/trading.rs`). Those tests were not run. See section 10.

A research build that “never signs” does not exist yet. The paper scope is a runtime guard around paper tasks. Live mode, manual trade, and permitted agent tools can sign. Any later phase that imports `swaps`, `configured_keypair`, or the trader executors into a research loop can move real funds. The plan in section 13 keeps research code on the paper side of that boundary and does not delete the existing live trader in this phase.

## 8. Dashboard and APIs

The UI is server-rendered HTML plus page scripts. Routes are registered in `src/webserver/routes/mod.rs`.

Pages: Home, Tokens, Positions, Filtering, Auto Trader, Strategies (linked from the trader and strategies templates), Copy Trading, Wallets, Transactions, Assistant, Services, Events, Config, plus login, lock screen, setup, and onboarding.

API groups under `/api` include status, tokens, events, filtering, positions (including `/positions/paper` and `/positions/paper/stats`), dashboard, wallet, blacklist, config, services, ohlcv, copy-trading, trading, trader, strategies, transactions, assistant, llm, llm-analysis, agent-control, telegram, and account.

What the current UI is good at:

- Portfolio and open positions, with realized stats that include profit factor, win rate, expectancy, max drawdown in SOL, fees, hold times, and a daily or weekly P&L series (`src/trader/stats/types.rs`). There is no Sharpe ratio anywhere under `src/trader/stats/`.
- A visual strategy editor over the condition catalog.
- Filtering analytics: passed and rejected tokens with reasons.
- Copy-trading insights: win rate, profit factor, P&L curve, skip reasons, latency, slippage, wallet comparison. That is per copy task, not a multi-strategy research leaderboard.
- Service health, endpoint connectivity, and an event log.
- Theme: `:root` in `src/webserver/templates/styles/foundation.css` is light (`--bg-primary: #f8f9fb`). `[data-theme="dark"]` is a full graphite palette. Dark mode exists. It is not the default.
- Updates without a full browser reload: the shell polls JSON. It is not a push stream, and it is not a research tape of signals, simulated fills, and rejection reasons across many strategies.

What the spec’s dashboard asks for and this UI does not have: a leaderboard of backtest return versus forward return, equity and drawdown curves per research strategy, monthly tables, regime splits, and a live feed whose rows are research events rather than this bot’s operational events. Building that on the existing Axum shell is reasonable. Pointing the existing Home page at backtest numbers would mix two kinds of P&L.

## 9. Persistence

Seventeen stores are listed in `src/database/catalog.rs` as `STORES`. Files, from `src/paths/database.rs` and the catalog:

| File | Class in catalog | Holds |
| --- | --- | --- |
| `wallets.db` | Vault | Encrypted key material. Not rebuilt automatically. |
| `positions.db` | Ledger | Live positions. |
| `paper_positions.db` | Ledger | Paper rounds, legs, epochs. |
| `copy_trading.db` | Ledger | Copy tasks and their books. |
| `strategies.db` | Ledger | Strategies, templates, evaluation totals, unused `strategy_backtests`. |
| `agent_control.db` | Ledger | MCP pairings, approvals, audit. |
| `ai.db` | Ledger | LLM instruction records. |
| `ai_chat.db` | Ledger | Assistant chats and scheduled tasks. |
| `transactions.db` | Derived | Decoded transactions. |
| `wallet.db` | Derived | Balance snapshots. |
| `tokens.db` | Cache | Token rows, market snapshots, Rugcheck, pools, rejections. Evictable. |
| `ohlcvs.db` | Cache | Candles and monitor state. Evictable. |
| `pools.db` | Cache | Pool tracking and blacklists. |
| `rpc_stats.db` | Cache | Provider health. |
| `events.db` | Cache | Event log. |
| `actions.db` | Cache | Action log. |
| `tools.db` | Cache | ATA cleanup tool state. |

Chain-scoped files (`tokens.db`, `ohlcvs.db`, `pools.db`, `rpc_stats.db`) use the unsuffixed Solana names. A future non-Solana chain would get `stem-chain.db`.

Research data should be a new store, not more rows in `ohlcvs.db` or `tokens.db`. Those two are allowed to delete themselves.

## 10. Tests that were run

Commands were the project’s own CI commands, on this machine, against commit `f26d8524f`. Live, mainnet, and real-database tests are `#[ignore]` and are excluded by the default nextest profile (`.config/nextest.toml`, `.github/workflows/ci.yml`). They were not run. No mainnet transaction was sent. No wallet was loaded.

### Rust

```text
cargo nextest 0.9.146
rustc 1.98.1 (48a229cea 2026-09-01)
CARGO_BUILD_JOBS=2
cargo nextest run --all-targets --no-tests=fail --profile default
```

Result, from the nextest summary:

```text
Nextest run ID 244b23b5-124c-4bff-8486-43c28c020eb7
Starting 3858 tests across 3 binaries (114 tests skipped)
Summary [87.038s] 3858 tests run: 3858 passed, 114 skipped
exit code 0
```

The 87 seconds are the test run after compilation. Compilation itself was a clean dependency build and finished before that summary. The 114 skipped tests are the ones nextest did not start because they are ignored: live RPC reads, live swap simulations, the opt-in mainnet spend test, and real-database tests that need an owner’s local `data/` directory. This environment has no such data directory and no funded wallet, so those tiers were left skipped on purpose.

The run includes the paper-isolation source scan, strategy engine tests, trader exit tests, storage recovery tests, and dashboard boot-performance tests that render HTML in-process. It does not prove that a long-running process stays healthy, and it does not prove quote or RPC behaviour against mainnet.

### Dashboard lint and Node tests

`npm ci` on Node `v22.23.4` warned that `package.json` requires Node `>=24.8.0`, then installed. Electron dependencies were installed with `ELECTRON_SKIP_BINARY_DOWNLOAD=1`. Playwright’s Chromium headless shell was installed because many dashboard tests launch it.

```text
npm run lint
exit code 0
```

The lint script is eslint, stylelint, html-validate, `tools/dashboard-ui-audit.mjs` (“Dashboard UI audit passed (124 stylesheets checked)”), `tools/i18n/audit.mjs` (0 hardcoded user-visible strings; locale catalogs at 100% for en, ar, de, es, fa, fr, hi, id, ja, ko, pt-BR, ru, tr, uk, vi, zh-Hans), and `tools/header_check.mjs` (“OK, 1963 files carry the standard header”).

```text
node --test --test-concurrency=4 tools/tests/*.test.mjs
tests 1340
pass 1340
fail 0
skipped 0
duration_ms 383345.314741
exit code 0
```

`npm run check` is lint plus this Node test script. Both halves passed, separately, on Node 22 rather than the Node 24 CI image. The Electron app was not launched.

## 11. Gap analysis against the spec

### Historical backtesting

| Spec requirement | In this repo |
| --- | --- |
| Historical OHLCV, several timeframes | Partial, and not durable. Seven timeframes exist. Default horizon is 7 days, hard cap in config is 30 days, only for tokens the monitor is tracking (default cap 300), and rows can be evicted. Upstream public history is pool-based and rate-limited. |
| Launch and liquidity history | Not stored as a series. Discovery sees “new” and “trending” now. Pool rows are current reserves. |
| Entry and exit simulation, fees, slippage, liquidity constraints, SL/TP, trailing stop, position size, max hold, DCA | Live and paper executors implement these against current quotes and pool prices. There is no bar-by-bar simulator. The unused `strategy_backtests` table does not implement one. |
| No look-ahead | The live engine uses “now,” so look-ahead is not a question it answers. A future replay must not use a candle’s close for a decision stamped at that candle’s open, and must not use a Rugcheck or liquidity reading from after the decision time. Nothing enforces that today. |
| No survivorship bias, including failed, rugged, and untradeable tokens | Not met. Feeds and cache eviction both drop tokens that stop being interesting. A missing candle or a missing token is indistinguishable from “never existed.” The spec forbids treating that absence as safety or profit. |
| Reproducible runs | Paper quote fills depend on live router responses. Candle rows do not record a dataset version that a backtest can pin. |

### Forward testing

Paper trading of the current market is implemented and guarded. It is one shared paper account for strategies, plus separate paper books for copy tasks. It does not:

- keep a frozen record of the market data the fill was decided on, beyond the quote metadata on the leg;
- separate forward-test results from backtest results, because backtests do not run;
- give each strategy its own bankroll, loss limit, and position cap;
- simulate a failed landing as a first-class research event independent of the live quote API being up.

The live 500 ms pool loop, rejection log, and paper executor are the right base for Phase 5, provided research cannot call `signing_keypair`.

### Dashboard

The operational console is real and tested at the HTML/JS contract level. The research surfaces in section 5 of the spec are absent: cross-strategy leaderboard, backtest versus forward, regime splits, Sharpe, and a research event tape. Profit factor, win rate, drawdown, hold time, and a P&L series exist for the live book, the shared paper book, and copy tasks. Sharpe is not computed. Dark theme exists and is not the default. Refresh is polling, not a websocket.

### Research coordinator

There is no module that watches a population of strategies, detects decay, runs walk-forward or parameter sensitivity, versions hypotheses, or rejects a strategy for any reason other than the live safety gates. `rules_fingerprint` and `strategy_evaluation_totals` are useful inputs. LLM calls exist for chat, filtering scores, and entry/exit scores. They are off by default and they do not produce backtest numbers. The spec’s rule against LLM-fabricated performance has no engine to violate yet, and the new engine must not let a model write `total_profit_sol`.

### Risk and data integrity

Paper and live books are separate files. Paper scope blocks signing on that task. The process as a whole still has keys, a signer, and trade APIs. Research logs are not a separate immutable dataset. Data-source health exists for RPC and API stats, not for “this backtest used a gap.” Missing data is not yet a typed research outcome.

## 12. Candidate historical sources

“Verified” means a vendor document was fetched or its current text was returned in this audit on 9 November 2026. Costs change. Re-check before buying a plan.

### Already integrated

**GeckoTerminal / CoinGecko on-chain OHLCV.** The repo already calls the keyless host. [CoinGecko’s pool OHLCV reference](https://docs.coingecko.com/reference/pool-ohlcv-contract-address) (paid on-chain API) says: timeframes day, hour, minute, and second; minute aggregates 1, 5, 15; hour aggregates 1, 4, 12; at most 1000 candles per call; `before_timestamp` for older pages; each call covers at most a 6-month range; empty intervals are omitted unless `include_empty_intervals=true`. Historical access on that reference: Basic is the past 6 months; Analyst and above is September 2021 to present, and only from when the pool was tracked. That page is the Pro API, not the keyless host this app uses. Keyless rate limits are about 5–10 requests per minute (sources above), not the 30 per minute in config. Pools with more than two tokens are unsupported on the public OHLCV route (stated in CoinGecko’s GeckoTerminal client docs). A memecoin that never received a tracked pool, or whose pool died first, has no series. Do not fill that hole with a flat line.

**DexScreener.** [The public reference](https://docs.dexscreener.com/api/reference) documents current pair and token snapshots (price, liquidity, volume, rolling m5/h1/h6/h24 changes) at 300 requests per minute, and profile/boost/order endpoints at 60 per minute. It does not document an OHLCV or historical endpoint. Useful for discovery and for a point-in-time liquidity snapshot if this process stores the snapshot itself. Not a backtest source.

**Solana Tracker.** Already an optional candle fallback. Billing is credit-based in code comments. Current price list: unverified this audit.

**ScreenerBot data service.** Already the preferred candle hop when an account is signed in. Retention, coverage of dead tokens, and whether responses are reproducible: not documented in this repo. Unverified externally. Treat it as an accelerator with a fallback, which is what `src/data_server/mod.rs` says it is.

**Helius, as this app uses it.** RPC, `logsSubscribe`, and enhanced wallet history for copy trading. Not a candle store.

**Rugcheck, Jupiter lists, CoinGecko lists, DefiLlama.** Discovery and security. Not OHLCV history.

### Not integrated — verified enough to plan against

**Birdeye.** Not in the repo. [OHLCV V3](https://data.birdeye.so/docs/data-api/price-ohlcv/get-defi-v3-ohlcv) is token-level (not only pool-level), Solana is a supported `x-chain`, intervals include 1s through 1M, and a call returns at most 5000 candles. [Compute-unit docs](https://data.birdeye.so/docs/guides/what-is-compute-unit-cost) price `GET /defi/ohlcv` at 25 CU up to 100 candles, 30 CU up to 300, and 40 CU up to 1000. V3-specific CU for the 5000-candle cap was not on the page text retrieved here. [Birdeye’s pricing page](https://birdeye.so/data-api/pricing), as returned in this audit: Lite $39/month, 2.5M CU, 15 rps; Starter $99/month, 8M CU, 15 rps; Premium $199/month, 20M CU, 50 rps, WebSocket; Business $499/month, 60M CU, 100 rps. Overage prices differ by tier. WebSocket is not on the cheaper tiers. Birdeye also documents token creation info, new listings, and trade history, which matter more for a launch panel than candles alone. Coverage still depends on Birdeye having indexed the mint. A rugged token they never indexed is still a hole. Paid, API key, not in this codebase.

**Helius, as a history source.** [Credits](https://www.helius.dev/docs/billing/credits) and [getTransactionsForAddress](https://www.helius.dev/docs/rpc/gettransactionsforaddress): mainnet retention for that method is unlimited; devnet is two weeks. Up to 1000 full transactions per call. Full responses cost 10 credits per 100 transactions returned, minimum 10. `getBlock` is 1 credit. Rate limits on the [rate-limit page](https://www.helius.dev/docs/billing/rate-limits): Free 10 RPC rps and 2 DAS rps; Developer 50 and 10; Business 200 and 50; Professional 500 and 100. The credits page lists Free $0 / 1M credits, Developer $49 / 10M, Business $499 / 100M, Professional $999 / 200M. A pricing-page snippet in the same audit mentioned 200M credits next to Business; the credits table and the marketing page do not agree in the text retrieved. Re-check before purchase. Helius does not return OHLCV. Reconstructing candles means reading every relevant swap. That is the right way to include failed tokens if the creation transaction is known, and it is expensive if the creation set is “all of Pump.fun.” Standard public RPC can do the same reads with worse rate limits and, on many free providers, shorter history. Unverified: any specific public RPC provider’s archival window.

**Bitquery.** [Historical Solana data](https://docs.bitquery.io/docs/blockchain/Solana/historical-aggregate-data/) says aggregate `DEXTradeByTokens` on `dataset: archive` starts 1 June 2024; per-trade `DEXTrades` archive is being backfilled toward 2024 and must be filtered by time or slot; the Trading API holds about the last 30 days and can build candles down to one second; the free trial and self-serve plans are realtime only; archive queries need a paid “Solana Historical OHLCV & Token Price Data” add-on. The add-on’s dollar price was not on the page fetched here. Unverified: that price, and whether every Pump.fun bonding-curve mint is in the archive. This is the strongest off-the-shelf candidate for a SQL-like candle history that starts before a local collector is turned on, with a hard floor around June 2024 for aggregates and an incomplete per-trade backfill.

**Dune.** [Solana DEX trades](https://docs.dune.com/data-catalog/curated/dex-trades/solana/overview) documents `dex_solana.trades` and Jupiter tables. [How credits work](https://docs.dune.com/resources/credits-billing/how-credits-work): Analyst $75/month or $65/month billed annually, 4000 credits; Plus $399/month or $349/month annually, 25000 credits. Free access after the trial is view-only, with no API export. [API billing](https://docs.dune.com/api-reference/overview/billing): Analyst exports cost 10 credits per MB. Queries are warehouse jobs, not a live feed. Completeness for short-lived memecoins was not verified. Good for cross-checks and for building a universe of mints that traded. Poor as the only source of 1-minute bars for a walk-forward engine.

### Unverified or a poor fit

**Flipside.** `https://docs.flipsidecrypto.xyz/flipside-products/data-studio/data-studio-pricing` returned HTTP 500 in this audit. Secondary write-ups say the crypto-data business was sold to SonarX and that Flipspace shut down around June 2026. Current tables, SLAs, and prices: unverified. Do not plan Phase 2 on Flipside until a primary source confirms the dataset still exists.

**Raw `getBlock` over a public RPC.** Complete in principle, from chain genesis, for any transaction the node still serves. In practice a public endpoint will rate-limit or truncate history long before a memecoin universe is rebuilt. No OHLCV, no USD, no token list. Cost is engineering plus whatever RPC plan can serve archival `getBlock`. Use it to fill specific mints and to audit a vendor, not as the first collector.

**DexScreener website charts.** The website draws candles. The documented API does not serve them. Scraping that site is out of scope and would be brittle.

## 13. Implementation plan for Phases 2–8

Principles that apply to every phase:

- Reuse the condition evaluators, pool decoders, provider clients, paper/live book split, Axum shell, and SQLite catalog. Do not fork a second trader.
- Research code must not call `signing_keypair`, `parse_private_key`, `sendTransaction`, or `execute_swap`. A source-scan test, the same shape as `tests/paper_isolation.rs`, should cover the new modules.
- Do not write backtest rows into `strategy_backtests` until that table is either replaced or migrated on purpose. It has no writer and too few columns.
- Do not store research history in `ohlcvs.db` or `tokens.db`. Those stores are caches and may delete rows.
- A missing bar, a missing pool, or a token that could not be priced is an explicit outcome (`missing`, `untradeable`, `unknown`). It is never a zero-loss trade and never a pass.
- Backtest results and forward-test results are different tables. The dashboard must not add them together.
- No component may accept a performance number from an LLM. Models may propose rules. Only the simulator may write returns.
- The existing live trader stays as it is until the user decides otherwise. See open questions. Research runs even if `trader.enabled` is false.

Proposed layout, added beside the current tree:

```text
src/research/
  mod.rs
  store/            # research.db schema, migrations, catalog entry
  universe/         # mints, launches, rugged/untradeable flags, source provenance
  candles/          # immutable OHLCV pages, gaps, dataset versions
  liquidity/        # pool reserve or liquidity observations
  simulate/         # Phase 3 backtest engine
  forward/          # Phase 5 paper portfolios
  agents/           # Phase 4 hypothesis runners
  coordinator/      # Phase 7
  report/           # metrics shared by backtest and forward
src/webserver/routes/research/
src/webserver/templates/pages/research.html
```

`research.db` is a new `StoreSpec` with class `Ledger` for universe membership, dataset versions, and result rows, and with explicit retention for raw vendor payloads. It is not evicted by the trading cache.

### Phase 2 — Historical collection and storage

Build a collector that can be re-run.

Schema, minimal:

- `datasets(id, source, retrieved_at, params_json, content_hash, notes)`
- `mints(chain_id, mint, first_seen_at, source, created_at_chain, creator, status)` where `status` is `unknown`, `active`, `rugged`, `untradeable`, or `missing_data`
- `pools(chain_id, pool, mint, dex, created_at, source, dataset_id)`
- `candles(dataset_id, chain_id, mint, pool, timeframe, ts, o, h, l, c, v, source, fetched_at)` with a uniqueness key on dataset, pool, timeframe, ts
- `observations(dataset_id, mint, ts, liquidity_usd, reserve_quote, source)` for liquidity, only when a source actually provided it
- `gaps(dataset_id, mint, timeframe, start_ts, end_ts, reason)`
- `ingest_runs(id, started_at, finished_at, source, error)`

Collector behaviour:

- Define a universe as “mints observed,” not “mints that still have a chart.” Start with two ingests that this repo can already call: GeckoTerminal new pools and Rugcheck new/trending, persisted instead of evicted. That universe is still biased toward what those APIs list. Record the bias on the dataset row.
- Add one paid or archival source for history before the collector existed. Recommended first paid source: Birdeye token OHLCV plus token-creation, if a key is available, because it is token-level and already documents creation info. Recommended cross-check: Bitquery archive aggregates from 1 June 2024, or Dune `dex_solana.trades` for mint membership. Helius `getTransactionsForAddress` / `getBlock` is the audit path for a sample of mints, not the bulk path, until a creation-event firehose is chosen.
- Store vendor payloads’ hashes. A backtest names a `dataset_id` and refuses to run if a required gap is unmarked.
- Map GeckoTerminal’s 1m/5m/15m/1h/4h/12h/1d aggregates onto the existing `Timeframe` enum. Do not invent 1s bars unless the chosen source returns them.
- Cap public GeckoTerminal at the published keyless limit, or move that traffic to a keyed CoinGecko on-chain plan. The current 30/min default will not survive a universe backfill.

Acceptance tests:

- A fixture page of GeckoTerminal OHLCV parses into candles whose timestamps are the upstream timestamps, not `Utc::now`.
- Re-ingesting the same payload does not duplicate rows and does not change `content_hash`.
- A mint with a launch record and zero candles is stored as `missing_data`, and a query for “tradeable over this window” excludes it.
- A token marked rugged by a fixture Rugcheck payload stays in `mints` after a trading-cache eviction (the research store is a different file).
- The research crate’s source scan fails if it names `signing_keypair` or `sendTransaction`.

### Phase 3 — Backtesting engine

`src/research/simulate/` replays candles and liquidity observations through the existing condition evaluators by building an `EvaluationContext` whose clock is the bar time, not `Utc::now`.

Rules:

- Decision at bar `t` may read only bars with timestamp strictly before the decision, or a documented open-of-bar convention that is the same for every strategy. Tests must fail if a condition sees the signal bar’s close.
- Fills use a model with configurable fee bps, slippage bps, and a maximum fraction of bar volume or of observed liquidity. If liquidity is absent, the fill is `untradeable`, not a full fill at the close.
- Reuse pure exit math from `src/trader/evaluators/` where the function does not read global config or the clock. Wrap the ones that do. Do not call the live paper quote function. That function needs a network.
- Position sizing, max hold, DCA adds, stop, trail, and take profit are parameters on the run, defaulting from `TraderConfig` only as a labelled preset.
- One run writes `backtest_runs` and `backtest_trades` (strategy id, rules fingerprint, dataset id, every fill, fee, slippage assumption, reject reason). It does not write `positions.db` or `paper_positions.db`.

Acceptance tests:

- A one-bar fixture cannot trigger a condition that needs that bar’s close.
- A strategy that would buy a mint flagged `missing_data` records a rejection and a zero trade count, and the reported return is “no trades,” not a profit.
- Two runs on the same dataset id, rules fingerprint, and parameters produce the same trade list.
- Fee and slippage change the fill price in the expected direction on a hand-computed fixture.
- A stop and a take-profit on a three-bar fixture exit on the first matching bar, in the same priority the live evaluator documents, and the test names that priority.

### Phase 4 — Strategy research agents

Eight families, each a module under `src/research/agents/` that emits a `Strategy` JSON document the existing validator already accepts: Momentum, Volume Breakout, Mean Reversion, Early Entry, Smart Money Tracking, Wallet Copy Trading, Liquidity Analysis, Market Regime.

- Hypotheses are parameter grids and rule trees, stored as `hypotheses(id, family, rules_json, params_json, parent_id, created_at)`.
- The only performance numbers stored are those `simulate` just wrote, joined by hypothesis id and dataset id.
- An agent may call an LLM to draft a rule tree. The draft is invalid until `validate_strategy` accepts it and a backtest run finishes. The prompt and the model id are stored. The model’s prose is not stored as a metric.
- Wallet-copy and smart-money hypotheses need wallet-trade history. That data is not in the candle store. Phase 4 should ship those two families as specifications and fixtures unless Phase 2 added a wallet-trade dataset. Do not pretend copy-trading paper tasks are a backtest of historical wallets.
- Each family writes a short weakness report from the trade list (regimes where it lost, sensitivity to fees). The report is computed, not generated as a performance claim.

Acceptance tests:

- A fixture hypothesis with a known trade list produces a stored report whose return equals the simulator’s return.
- An LLM client double that returns a fake “profit factor 3” does not create a `backtest_trades` row.
- A rule tree the current validator rejects is stored as a failed hypothesis and is not backtested.
- Momentum and mean-reversion fixtures lose when the fee is set above their edge, and the test asserts that.

### Phase 5 — Forward testing

`src/research/forward/` consumes the live pool price and the candle cache the bot already maintains, and books fills into `forward_portfolios` inside `research.db`.

- One portfolio per strategy: cash, position cap, daily loss limit. Not the shared `PaperAccount`.
- Fills record the decision timestamp, the pool price timestamp, the quote or the explicit model used, fee, slippage, and `filled` or `rejected` with a reason.
- If the implementation reuses `trader/paper/fill.rs`, it must run inside `paper_scope` and it must not enable a live strategy or a live copy task. Prefer a price-based fill with an explicit slippage model so a forward test still records something when Jupiter is down, and label that model on the row. Do not mix quote fills and model fills in one equity curve without a column that says which.
- Forward rows are never written by the backtester.

Acceptance tests:

- Two strategies signaling the same mint debit two portfolios.
- A forced quote failure records a rejected entry and does not change cash.
- A test that enables `TradingMode::Live` from the forward runner fails to compile or fails the source scan.
- Restarting the process reloads open forward positions from `research.db` and does not create a second fill for the same decision id.

### Phase 6 — Live dashboard

Add a Research page to the existing shell, dark by using the existing `[data-theme="dark"]` palette (make dark the default for this page even if the rest of the app stays on the user theme). Poll the research API on the existing poller, or add a dedicated timer. Do not add a browser WebSocket unless polling cannot meet the update the page needs. The spec asks for auto-update without a manual refresh. The current poller already does that.

Panels, each backed by a query over `backtest_*` and `forward_*` only:

- Overview counts and P&L for forward portfolios: daily, weekly, monthly, win rate, profit factor, max drawdown, average hold. Sharpe only when a portfolio has enough daily returns to define it, otherwise an explicit empty state. Do not show 0 as “no risk.”
- Leaderboard: name, backtest return, forward return, win rate, profit factor, max drawdown, trade count, status, last signal. A strategy with no forward trades shows an empty forward cell.
- Tape: new mints ingested, signals, simulated entries and exits, open positions, unrealized P&L from the live pool price, rejections with reasons.
- Charts: equity, drawdown, monthly bars, backtest versus forward. Regime chart only after Phase 4 has a regime label on trades. Until then the panel says the label is absent.

Acceptance tests:

- Fixture portfolios render the leaderboard in the existing Node test style (`tools/tests`), including the empty and “missing data” states.
- A backtest trade cannot appear in the forward P&L fixture.
- The page template uses the dark tokens and still updates from a second poll without a navigation.

The visual check belongs in the browser once the page exists. This phase does not add that page, so it was not browser-tested.

### Phase 7 — Coordinator

`src/research/coordinator/` reads finished runs. It does not place trades.

- Walk-forward: consecutive dataset slices, train parameters on one slice, report the next slice. The stored score is the out-of-sample slice.
- Sensitivity: perturb each numeric parameter and store the return range. A strategy whose only good point is the unperturbed point is flagged.
- Decay: compare a rolling forward window with the backtest. A drawdown or profit-factor break beyond a configured line marks the strategy `deteriorated`.
- Rejection rules are data, not code buried in a prompt: minimum out-of-sample trades, maximum drawdown, maximum in-sample to out-of-sample gap. A strategy that fails is `rejected` and cannot be ranked as active.
- Versioning uses the existing rules fingerprint. A fingerprint change starts a new hypothesis generation and clears forward evidence for ranking, without deleting the old trades.
- The coordinator never sorts by in-sample return alone.

Acceptance tests:

- A fixture that is profitable in-sample and flat out-of-sample is rejected.
- Changing the rules fingerprint starts a new generation and the leaderboard does not keep the old forward return on the new fingerprint.
- A run with no out-of-sample slice has status `insufficient`, not a rank.

### Phase 8 — Testing, validation, and docs

- Extend CI so `cargo nextest run --all-targets` covers `src/research/` without network.
- Add a small recorded-fixture ingest test and a golden backtest. Do not add a mainnet test that spends SOL.
- Document the dataset version, the fill model, and the known holes (unindexed launches, public rate limits, shared versus per-strategy books).
- Keep this audit’s limitation list in the Phase 8 doc until each item is closed by a test, not by a sentence.

Acceptance test for the phase itself: the new suite passes offline, and a documented command prints dataset id, trade count, and reject count for the golden fixture. No command in that suite loads `wallets.db`.

## 14. Open questions

1. Should this fork keep the live signer available to the existing dashboard, or should a research mode refuse to load a key at all? The code can do either. The spec says no signing. The product today signs. This phase did not remove signing.
2. Which history is in budget? Birdeye (token OHLCV, paid CU plans listed above), Bitquery archive (from 1 June 2024 for aggregates, add-on price not confirmed), Dune (warehouse, Analyst at $75/month), or Helius archival reads. Public GeckoTerminal alone will not build a survivorship-aware corpus.
3. How far back, and which venues, count as the universe? Pump.fun bonding curves, graduated pools, and Raydium/Meteora/Orca pools do not share one creation feed in this repo.
4. Is a shared paper bankroll acceptable for forward tests, or is the spec’s independent portfolio per strategy required from the first forward-test release? The plan assumes independent portfolios because the spec says so. The current paper account is shared.
5. For tokens no vendor has candles for, is “store the mint and refuse to trade it” the desired behaviour? The plan assumes yes.
6. Will research runs be signed into the ScreenerBot data service, and what history does that service actually keep? The repo does not say.
7. Node on the CI image is 24 and `package.json` requires `>=24.8.0`. This audit passed lint and Node tests on Node 22.23.4. That should be aligned before Phase 6 adds dashboard tests, so a failure is not blamed on the engine mismatch.

## 15. Conclusion

The repository is a working Solana trading bot with a tested offline suite: 3858 nextest tests passed, 114 ignored tests not run, and 1340 Node dashboard tests passed. It can paper-trade without signing, and it can also sign real swaps when a strategy, a copy task, a manual action, or an authorized agent tool asks it to. It does not have historical backtests, a dead-token universe, per-strategy forward books, a research dashboard, or a coordinator. The unused `strategy_backtests` table and the 7-to-30-day candle cache should not be treated as those features. Phases 2–8 can sit beside the current engine in `src/research/` and reuse its evaluators, providers, and dashboard shell, if research storage is separate and research code cannot reach the signer.
