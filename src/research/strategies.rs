// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Rule sets for a research backtest. Each one that can run has its own
//! starting cash. Smart-money and wallet-copy need wallet fills this store
//! does not have, so they are not run and they do not get a made-up result.

use super::simulate::{run, BacktestConfig, BacktestReport, EntryRule};

/// Closed trades, traded tokens, and bar span below these are not enough to
/// treat a return as evidence.
const MIN_CLOSED_TRADES: usize = 30;
const MIN_TOKENS_BOUGHT: usize = 20;
const MIN_SPAN_SECONDS: i64 = 7 * 24 * 60 * 60;

/// One research rule set.
pub struct StrategySpec {
    pub id: &'static str,
    pub name: &'static str,
    /// `None` when the rule set cannot be run on this store.
    pub blocked: Option<&'static str>,
    pub rules: &'static str,
    pub weaknesses: &'static str,
    pub config: Option<BacktestConfig>,
}

/// A spec plus the run, when there was one.
pub struct StrategyOutcome {
    pub spec: StrategySpec,
    pub report: Option<BacktestReport>,
    pub bar_mints: usize,
    pub span_seconds: i64,
    pub sufficient: bool,
}

pub fn research_strategies() -> Vec<StrategySpec> {
    vec![
        runnable(
            "momentum",
            "Momentum",
            "1m close is at least 1% above the close 5 bars earlier. Stop 8%, take-profit 12%, max hold 20 minutes. Order 0.05 SOL.",
            "A 1% rise on a one-minute bar is mostly noise. The stop and the fee can both be larger than the signal. It keeps buying a mint that already ran.",
            momentum(),
        ),
        runnable(
            "volume-breakout",
            "Volume breakout",
            "Closed-bar volume is at least 2x the prior 10-bar average and the close is up. Stop 10%, take-profit 15%, max hold 15 minutes. Order 0.05 SOL.",
            "GeckoTerminal volume is quote-token volume for that pool, not a unique buyer count. One print can clear the multiple. It has no cap on chase distance.",
            volume_breakout(),
        ),
        runnable(
            "mean-reversion",
            "Mean reversion",
            "Close is at least 3% below the mean of the last 10 closes. Stop 6%, take-profit 4%, max hold 15 minutes. Order 0.05 SOL.",
            "Memecoin drops are often the start of a rug, not a return to a mean. A 4% target is inside the fee-and-slippage band on a thin pool.",
            mean_reversion(),
        ),
        runnable(
            "early-entry",
            "Early entry",
            "Pool age at the signal is at most 60 minutes, and the close is at least 0.5% above the previous close or this bar's open. Stop 12%, take-profit 20%, max hold 30 minutes. Order 0.05 SOL.",
            "Born time is pool creation, not the first trade. Most of this file's bars are long after creation, so the rule rarely arms. A new pool's reserve snapshot is still only the fetch time.",
            early_entry(),
        ),
        runnable(
            "liquidity-analysis",
            "Liquidity analysis",
            "Quote reserve observed at or before the signal is at least 5 SOL, and the close is at least 0.5% above the close 3 bars earlier. Stop 8%, take-profit 12%, max hold 20 minutes. Order 0.05 SOL.",
            "The reserve is one fetch-time estimate, not a history. The 5 SOL gate just selects the tail of the file. It does not see a pull of liquidity that happened earlier.",
            liquidity(),
        ),
        runnable(
            "market-regime",
            "Market regime",
            "Overlay on a 3-bar 0.5% rise: at least 55% of mints with a bar at that minute closed up. Stop 8%, take-profit 12%, max hold 20 minutes. Order 0.05 SOL.",
            "Breadth is the other memecoins in this file, not SOL itself. One quiet minute with two prints can flip the filter. It does not predict the next minute.",
            regime(),
        ),
        runnable(
            "hold-cash",
            "Hold SOL/cash",
            "No entries. The bankroll stays in cash.",
            "This is the zero-trade benchmark. It ignores every token path, including ones that went up.",
            hold_cash(),
        ),
        runnable(
            "buy-and-hold",
            "Buy and hold universe",
            "Buy 0.01 SOL of each mint on the first bar that can fill, then hold to the terminal mark. No stop and no take-profit.",
            "Chain fees mean 1 SOL cannot hold a large universe at a bigger size. Fills still require a reserve at the fill, so this buys the tail, not the first bar of history.",
            buy_and_hold(),
        ),
        blocked(
            "smart-money",
            "Smart money",
            "Would follow wallets that accumulated before a run.",
            "There is no wallet-level fill history in this store. A result here would be invented.",
        ),
        blocked(
            "wallet-copy",
            "Wallet copy",
            "Would copy a watched wallet's buys and sells.",
            "There is no wallet-level fill history in this store. A result here would be invented.",
        ),
    ]
}

/// Run every rule set that has a config. Blocked ones are listed and not run.
pub fn run_research_set(
    store: &mut super::store::ResearchStore,
) -> Result<Vec<StrategyOutcome>, String> {
    let candles = store.load_candles("1m")?;
    let mut mints = std::collections::BTreeSet::new();
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    for candle in &candles {
        mints.insert(candle.mint.clone());
        first = first.min(candle.ts);
        last = last.max(candle.ts);
    }
    let span = if candles.is_empty() { 0 } else { last - first };
    let bar_mints = mints.len();
    let mut outcomes = Vec::new();
    for spec in research_strategies() {
        let report = match &spec.config {
            Some(config) => Some(run(store, config)?),
            None => None,
        };
        let sufficient = report.as_ref().is_some_and(|report| {
            report.performance.closed_trades >= MIN_CLOSED_TRADES
                && report.performance.tokens_bought >= MIN_TOKENS_BOUGHT
                && span >= MIN_SPAN_SECONDS
        });
        outcomes.push(StrategyOutcome {
            spec,
            report,
            bar_mints,
            span_seconds: span,
            sufficient,
        });
    }
    Ok(outcomes)
}

fn runnable(
    id: &'static str,
    name: &'static str,
    rules: &'static str,
    weaknesses: &'static str,
    config: BacktestConfig,
) -> StrategySpec {
    StrategySpec {
        id,
        name,
        blocked: None,
        rules,
        weaknesses,
        config: Some(config),
    }
}

fn blocked(
    id: &'static str,
    name: &'static str,
    rules: &'static str,
    weaknesses: &'static str,
) -> StrategySpec {
    StrategySpec {
        id,
        name,
        blocked: Some("blocked: no wallet data"),
        rules,
        weaknesses,
        config: None,
    }
}

fn bankroll(id: &str, entry: EntryRule) -> BacktestConfig {
    let mut config = BacktestConfig::app_fees(id, "1m", 60);
    config.starting_cash_sol = 1.0;
    config.order_sol = 0.05;
    config.entry = entry;
    config.stop_loss_pct = Some(8.0);
    config.take_profit_pct = Some(12.0);
    config.trailing_activation_pct = None;
    config.trailing_distance_pct = None;
    config.max_hold_seconds = Some(20 * 60);
    config.dca_drop_pct = None;
    config.dca_size_multiplier = None;
    config.seed = 1;
    config
}

fn momentum() -> BacktestConfig {
    bankroll(
        "momentum",
        EntryRule::CloseRise {
            lookback: 5,
            min_pct: 1.0,
        },
    )
}

fn volume_breakout() -> BacktestConfig {
    let mut config = bankroll(
        "volume-breakout",
        EntryRule::VolumeBreakout {
            lookback: 10,
            multiple: 2.0,
        },
    );
    config.stop_loss_pct = Some(10.0);
    config.take_profit_pct = Some(15.0);
    config.max_hold_seconds = Some(15 * 60);
    config
}

fn mean_reversion() -> BacktestConfig {
    let mut config = bankroll(
        "mean-reversion",
        EntryRule::MeanRevert {
            lookback: 10,
            drop_pct: 3.0,
        },
    );
    config.stop_loss_pct = Some(6.0);
    config.take_profit_pct = Some(4.0);
    config.max_hold_seconds = Some(15 * 60);
    config
}

fn early_entry() -> BacktestConfig {
    let mut config = bankroll(
        "early-entry",
        EntryRule::EarlyEntry {
            max_age_seconds: 60 * 60,
            min_rise_pct: 0.5,
        },
    );
    config.stop_loss_pct = Some(12.0);
    config.take_profit_pct = Some(20.0);
    config.max_hold_seconds = Some(30 * 60);
    config
}

fn liquidity() -> BacktestConfig {
    bankroll(
        "liquidity-analysis",
        EntryRule::MinReserve {
            min_sol: 5.0,
            inner: Box::new(EntryRule::CloseRise {
                lookback: 3,
                min_pct: 0.5,
            }),
        },
    )
}

fn regime() -> BacktestConfig {
    bankroll(
        "market-regime",
        EntryRule::BreadthFilter {
            min_fraction: 0.55,
            inner: Box::new(EntryRule::CloseRise {
                lookback: 3,
                min_pct: 0.5,
            }),
        },
    )
}

fn hold_cash() -> BacktestConfig {
    let mut config = bankroll("hold-cash", EntryRule::Never);
    config.order_sol = 0.0;
    config.stop_loss_pct = None;
    config.take_profit_pct = None;
    config.max_hold_seconds = None;
    config
}

fn buy_and_hold() -> BacktestConfig {
    let mut config = bankroll("buy-and-hold", EntryRule::BuyAndHold);
    config.order_sol = 0.01;
    config.stop_loss_pct = None;
    config.take_profit_pct = None;
    config.max_hold_seconds = None;
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_file_is_not_treated_as_sufficient_evidence() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data/research-memecoin-sample.db");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("research.db");
        std::fs::copy(&source, &dest).unwrap();
        let mut store =
            crate::research::store::ResearchStore::open_path(&dest, crate::chains::ChainId::Solana)
                .unwrap();
        assert!(crate::global::in_research_mode());
        let outcomes = run_research_set(&mut store).unwrap();
        assert!(outcomes.iter().all(|outcome| !outcome.sufficient));
        for outcome in &outcomes {
            match outcome.spec.blocked {
                Some(reason) => {
                    assert_eq!(reason, "blocked: no wallet data");
                    assert!(outcome.report.is_none(), "{}", outcome.spec.id);
                }
                None => {
                    let report = outcome.report.as_ref().unwrap();
                    let performance = &report.performance;
                    eprintln!(
                        "RESULT {} equity={:.6} realized={:.6} unrealized={:.6} ret={:.4} trades={} wins={} win_rate={:.3} pf={} dd={:.3} tokens={} rejects={:?} fills={} rejected={}",
                        outcome.spec.id,
                        performance.ending_equity_sol,
                        performance.realized_pnl_sol,
                        performance.unrealized_pnl_sol,
                        performance.return_pct,
                        performance.closed_trades,
                        performance.winning_trades,
                        performance.win_rate,
                        performance
                            .profit_factor
                            .map(|value| format!("{value:.3}"))
                            .unwrap_or_else(|| "n/a".into()),
                        performance.max_drawdown_pct,
                        performance.tokens_bought,
                        performance.reject_reasons,
                        report.fills,
                        report.rejected
                    );
                }
            }
        }
        let cash = outcomes
            .iter()
            .find(|outcome| outcome.spec.id == "hold-cash")
            .unwrap();
        let performance = &cash.report.as_ref().unwrap().performance;
        assert!((performance.ending_equity_sol - 1.0).abs() < 1e-9);
        assert_eq!(performance.closed_trades, 0);
        assert!(performance.return_pct.abs() < 1e-9);
        let held = outcomes
            .iter()
            .find(|outcome| outcome.spec.id == "buy-and-hold")
            .unwrap();
        assert_eq!(held.report.as_ref().unwrap().starting_cash_sol, 1.0);
        assert_ne!(
            cash.report.as_ref().unwrap().run_id,
            held.report.as_ref().unwrap().run_id
        );
    }
}
