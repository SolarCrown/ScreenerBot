// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Historical backtest on the research store.
//!
//! A decision uses only bars that have closed. The fill, if the next bar exists
//! and is the next timestamp of this timeframe, is that bar's open adjusted for
//! pool-reserve slippage and fees. A missing next bar, a missing reserve, a
//! rugged mint, or a gap is a recorded rejection or an unresolved open. It is
//! not a profit. Each run has its own cash balance. Results are written to
//! `backtest_runs` and `backtest_trades`, not to a forward-test book.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{DcaLevel, DcaMeasureFrom, TakeProfitLevel};
use crate::trader::evaluators::exit_rules::{self, ExitInputs, ExitRule};

use super::store::{
    BacktestRunRow, BacktestTradeRow, ResearchStore, StoredCandle, StoredLiquidity, StoredMint,
};

/// The app's platform fee: `PLATFORM_FEE_BPS` is 50, which is 0.5%.
pub const APP_PLATFORM_FEE_BPS: u16 = 50;
/// The app's network-fee reserve, in lamports (`NETWORK_FEE_RESERVE_LAMPORTS`).
pub const APP_NETWORK_FEE_LAMPORTS: u64 = 2_000_000;
/// Default priority fee in lamports: the app's default compute-unit price is
/// 50_000 micro-lamports. Charged on a 200_000 CU budget that is
/// `50_000 * 200_000 / 1_000_000 = 10_000` lamports. This is that product, not
/// a fee measured from a landed transaction.
pub const APP_PRIORITY_FEE_LAMPORTS: u64 = 10_000;

fn lamports_per_sol() -> f64 {
    10f64.powi(9)
}

/// How an entry is decided from closed bars only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryRule {
    /// Close-to-close change over `lookback` bars is at least `min_pct`.
    CloseRise { lookback: usize, min_pct: f64 },
    /// The closed bar's close is strictly above `level`.
    CloseAbove { level: f64 },
}

/// A higher timeframe whose last *closed* bar must be an up bar. A bar is
/// closed only when its start plus its length is at or before the decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HigherTimeframe {
    pub timeframe: String,
    pub seconds: i64,
    pub require_up_bar: bool,
}

/// One strategy's backtest. Cash starts at `starting_cash_sol` and is not
/// shared with any other strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub strategy_id: String,
    pub timeframe: String,
    pub timeframe_seconds: i64,
    pub starting_cash_sol: f64,
    pub order_sol: f64,
    pub entry: EntryRule,
    pub higher_timeframe: Option<HigherTimeframe>,
    pub platform_fee_bps: u16,
    pub network_fee_lamports: u64,
    pub priority_fee_lamports: u64,
    /// Buy or sell size above this fraction of the quote reserve is refused.
    pub max_reserve_fraction: f64,
    pub stop_loss_pct: Option<f64>,
    pub trailing_activation_pct: Option<f64>,
    pub trailing_distance_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    pub max_hold_seconds: Option<i64>,
    pub dca_drop_pct: Option<f64>,
    pub dca_size_multiplier: Option<f64>,
    /// Recorded on the run. This fill model does not draw random numbers.
    pub seed: u64,
}

impl BacktestConfig {
    pub fn app_fees(strategy_id: impl Into<String>, timeframe: &str, seconds: i64) -> Self {
        Self {
            strategy_id: strategy_id.into(),
            timeframe: timeframe.to_owned(),
            timeframe_seconds: seconds,
            starting_cash_sol: 1.0,
            order_sol: 0.05,
            entry: EntryRule::CloseRise {
                lookback: 1,
                min_pct: 0.0,
            },
            higher_timeframe: None,
            platform_fee_bps: APP_PLATFORM_FEE_BPS,
            network_fee_lamports: APP_NETWORK_FEE_LAMPORTS,
            priority_fee_lamports: APP_PRIORITY_FEE_LAMPORTS,
            max_reserve_fraction: 0.05,
            stop_loss_pct: Some(10.0),
            trailing_activation_pct: None,
            trailing_distance_pct: None,
            take_profit_pct: Some(20.0),
            max_hold_seconds: None,
            dca_drop_pct: None,
            dca_size_multiplier: None,
            seed: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BacktestReport {
    pub run_id: String,
    pub strategy_id: String,
    pub starting_cash_sol: f64,
    pub ending_cash_sol: f64,
    pub config_hash: String,
    pub data_hash: String,
    pub seed: u64,
    pub fills: usize,
    pub rejected: usize,
    pub open_unresolved: usize,
    pub notes: String,
    pub(crate) trades: Vec<BacktestTradeRow>,
}

#[derive(Clone)]
struct Bar {
    ts: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    pool: String,
}

struct MintInfo {
    status: String,
    status_at: i64,
    born_at: i64,
}

struct Reserve {
    at: i64,
    sol: f64,
}

struct Position {
    tokens: f64,
    cost_sol: f64,
    entry_price: f64,
    first_buy_sol: f64,
    entry_ts: i64,
    peak_close: f64,
    dca_count: u32,
    take_profit_steps: u32,
    partial_stop: bool,
}

/// Buy `order_sol` of a token whose bar opens at `open_price`, against
/// `reserve_sol` of quote reserves. Returns `(fill_price, tokens, sol_debit, fee_sol, slippage_pct)`.
pub fn quote_buy(
    open_price: f64,
    order_sol: f64,
    reserve_sol: f64,
    platform_fee_bps: u16,
    network_fee_lamports: u64,
    priority_fee_lamports: u64,
    max_reserve_fraction: f64,
) -> Result<(f64, f64, f64, f64, f64), &'static str> {
    if !(open_price.is_finite() && open_price > 0.0 && order_sol.is_finite() && order_sol > 0.0) {
        return Err("bad_price_or_size");
    }
    if !(reserve_sol.is_finite() && reserve_sol > 0.0) {
        return Err("no_liquidity");
    }
    if order_sol / reserve_sol > max_reserve_fraction {
        return Err("exceeds_liquidity");
    }
    let impact = order_sol / (reserve_sol + order_sol);
    let fill_price = open_price * (1.0 + impact);
    let platform = order_sol * f64::from(platform_fee_bps) / 10_000.0;
    let tokens = (order_sol - platform) / fill_price;
    if !(tokens.is_finite() && tokens > 0.0) {
        return Err("unfillable");
    }
    let chain_fee = (network_fee_lamports + priority_fee_lamports) as f64 / lamports_per_sol();
    let fee_sol = platform + chain_fee;
    let debit = order_sol + chain_fee;
    Ok((fill_price, tokens, debit, fee_sol, impact * 100.0))
}

/// Sell `tokens` into `reserve_sol`. Returns `(fill_price, net_sol, fee_sol, slippage_pct)`.
pub fn quote_sell(
    open_price: f64,
    tokens: f64,
    reserve_sol: f64,
    platform_fee_bps: u16,
    network_fee_lamports: u64,
    priority_fee_lamports: u64,
    max_reserve_fraction: f64,
) -> Result<(f64, f64, f64, f64), &'static str> {
    if !(open_price.is_finite() && open_price > 0.0 && tokens.is_finite() && tokens > 0.0) {
        return Err("bad_price_or_size");
    }
    if !(reserve_sol.is_finite() && reserve_sol > 0.0) {
        return Err("no_liquidity");
    }
    let gross_at_open = tokens * open_price;
    if gross_at_open / reserve_sol > max_reserve_fraction {
        return Err("exceeds_liquidity");
    }
    let impact = gross_at_open / (reserve_sol + gross_at_open);
    let fill_price = open_price * (1.0 - impact);
    let gross = tokens * fill_price;
    let platform = gross * f64::from(platform_fee_bps) / 10_000.0;
    let chain_fee = (network_fee_lamports + priority_fee_lamports) as f64 / lamports_per_sol();
    let fee_sol = platform + chain_fee;
    let net = gross - fee_sol;
    Ok((fill_price, net, fee_sol, impact * 100.0))
}

/// Run `config` against whatever the store holds for its chain. The same
/// config, seed and data hash produce the same trades.
pub fn run(store: &mut ResearchStore, config: &BacktestConfig) -> Result<BacktestReport, String> {
    let started = Utc::now();
    let config_json = serde_json::to_string(config).map_err(|error| error.to_string())?;
    let config_hash = sha256_hex(&config_json);
    let candles = store.load_candles(&config.timeframe)?;
    let higher = match &config.higher_timeframe {
        Some(higher) => store.load_candles(&higher.timeframe)?,
        None => Vec::new(),
    };
    let mints = store.load_mints()?;
    let liquidity = store.load_liquidity()?;
    let data_hash = hash_inputs(&candles, &higher, &mints, &liquidity, config);
    let trades = simulate(config, &candles, &higher, &mints, &liquidity);
    let fills = trades.iter().filter(|trade| trade.side != "reject").count();
    let rejected = trades.iter().filter(|trade| trade.side == "reject").count();
    let open_unresolved = trades
        .iter()
        .filter(|trade| trade.reason == "open_unresolved")
        .count();
    let ending_cash_sol = trades
        .iter()
        .rev()
        .find_map(|trade| trade.cash_after)
        .unwrap_or(config.starting_cash_sol);
    let notes = coverage_notes(config, &candles, &mints, &liquidity);
    let run_id = sha256_hex(&format!("{config_hash}|{data_hash}|{}", config.strategy_id));
    let row = BacktestRunRow {
        id: run_id.clone(),
        strategy_id: config.strategy_id.clone(),
        timeframe: config.timeframe.clone(),
        starting_cash_sol: config.starting_cash_sol,
        ending_cash_sol,
        config_json,
        config_hash: config_hash.clone(),
        data_hash: data_hash.clone(),
        seed: i64::try_from(config.seed).unwrap_or(i64::MAX),
        started_at: started.to_rfc3339(),
        finished_at: Utc::now().to_rfc3339(),
        trade_count: i64::try_from(fills).unwrap_or(i64::MAX),
        rejected_count: i64::try_from(rejected).unwrap_or(i64::MAX),
        open_unresolved: i64::try_from(open_unresolved).unwrap_or(i64::MAX),
        notes: notes.clone(),
    };
    store.save_backtest(&row, &trades)?;
    Ok(BacktestReport {
        run_id,
        strategy_id: config.strategy_id.clone(),
        starting_cash_sol: config.starting_cash_sol,
        ending_cash_sol,
        config_hash,
        data_hash,
        seed: config.seed,
        fills,
        rejected,
        open_unresolved,
        notes,
        trades,
    })
}

fn simulate(
    config: &BacktestConfig,
    candles: &[StoredCandle],
    higher: &[StoredCandle],
    mints: &[StoredMint],
    liquidity: &[StoredLiquidity],
) -> Vec<BacktestTradeRow> {
    let policy = exit_policy(config);
    let bars = group_bars(candles);
    let higher_bars = group_bars(higher);
    let info = mint_info(mints);
    let reserves = reserves_by_mint(liquidity);
    let mut trades = Vec::new();
    let mut cash = config.starting_cash_sol;

    for (mint, state) in &info {
        if !bars.contains_key(mint) {
            trades.push(reject(mint, None, "no_candles", cash));
        }
        let _ = state;
    }

    for (mint, series) in &bars {
        let Some(state) = info.get(mint) else {
            trades.push(reject(mint, None, "mint_not_in_universe", cash));
            continue;
        };
        let higher_series = higher_bars.get(mint);
        let reserve_series = reserves.get(mint);
        let mut position: Option<Position> = None;
        for index in 0..series.len() {
            let bar = &series[index];
            let next = series.get(index + 1);
            if let Some(held) = position.as_mut() {
                if bar.close > held.peak_close {
                    held.peak_close = bar.close;
                }
            }
            if position.is_some() && terminal(state, bar.ts) {
                let loss = position.as_ref().map(|held| held.cost_sol).unwrap_or(0.0);
                trades.push(BacktestTradeRow {
                    mint: mint.clone(),
                    side: "sell".into(),
                    reason: "exit_unfillable".into(),
                    signal_ts: Some(bar.ts),
                    fill_ts: None,
                    fill_price: None,
                    size_sol: Some(0.0),
                    fee_sol: Some(0.0),
                    slippage_pct: None,
                    cash_after: Some(cash),
                    pnl_sol: Some(-loss),
                });
                position = None;
                continue;
            }
            if let Some(held) = position.as_ref() {
                let decision = exit_rules::decide(
                    &ExitInputs {
                        average_entry_price: held.entry_price,
                        current_price: bar.close,
                        basis_peak_price: held.peak_close,
                        held_seconds: bar.ts.saturating_sub(held.entry_ts),
                        partial_exits_taken: held.partial_stop,
                        take_profit_levels_taken: held.take_profit_steps,
                    },
                    &policy,
                );
                let max_hold = config
                    .max_hold_seconds
                    .is_some_and(|limit| bar.ts.saturating_sub(held.entry_ts) >= limit);
                if decision.outcome.is_some() || max_hold {
                    let reason = decision
                        .outcome
                        .map(|outcome| rule_name(outcome.rule))
                        .unwrap_or_else(|| "max_hold".to_owned());
                    let fraction = decision
                        .outcome
                        .and_then(|outcome| outcome.exit_percentage)
                        .unwrap_or(100.0);
                    let next = match next_step(next, bar.ts, config.timeframe_seconds) {
                        NextStep::Fill(next) => next,
                        NextStep::Gap => {
                            trades.push(reject(mint, Some(bar.ts), "missing_next_bar", cash));
                            continue;
                        }
                        NextStep::End => continue,
                    };
                    position = apply_sell(
                        config,
                        mint,
                        bar.ts,
                        next,
                        fraction,
                        &reason,
                        reserve_series,
                        &mut cash,
                        &mut trades,
                        position.take().unwrap(),
                    );
                    continue;
                }
                if let Some(multiplier) = dca_multiplier(config, held, bar.close, bar.ts) {
                    let next = match next_step(next, bar.ts, config.timeframe_seconds) {
                        NextStep::Fill(next) => next,
                        NextStep::Gap => {
                            trades.push(reject(mint, Some(bar.ts), "missing_next_bar", cash));
                            continue;
                        }
                        NextStep::End => continue,
                    };
                    let add = held.first_buy_sol * multiplier;
                    if let Some(bought) = buy_fill(
                        config,
                        mint,
                        bar.ts,
                        next,
                        add,
                        "dca",
                        reserve_series,
                        &mut cash,
                        &mut trades,
                    ) {
                        let held = position.as_mut().unwrap();
                        let new_cost = held.cost_sol + bought.debit;
                        let new_tokens = held.tokens + bought.tokens;
                        held.entry_price = new_cost / new_tokens;
                        held.cost_sol = new_cost;
                        held.tokens = new_tokens;
                        held.dca_count = held.dca_count.saturating_add(1);
                    }
                    continue;
                }
            }
            if position.is_none() && signal(config, series, index) {
                if !eligible(state, bar.ts) {
                    let reason = if terminal(state, bar.ts) {
                        "not_in_universe"
                    } else {
                        "not_yet_listed"
                    };
                    trades.push(reject(mint, Some(bar.ts), reason, cash));
                    continue;
                }
                if !higher_ok(config, higher_series, bar.ts) {
                    trades.push(reject(mint, Some(bar.ts), "higher_timeframe_missing", cash));
                    continue;
                }
                let next = match next_step(next, bar.ts, config.timeframe_seconds) {
                    NextStep::Fill(next) => next,
                    NextStep::Gap => {
                        trades.push(reject(mint, Some(bar.ts), "missing_next_bar", cash));
                        continue;
                    }
                    NextStep::End => continue,
                };
                if let Some(bought) = buy_fill(
                    config,
                    mint,
                    bar.ts,
                    next,
                    config.order_sol,
                    "entry",
                    reserve_series,
                    &mut cash,
                    &mut trades,
                ) {
                    position = Some(Position {
                        tokens: bought.tokens,
                        cost_sol: bought.debit,
                        entry_price: bought.price,
                        first_buy_sol: config.order_sol,
                        entry_ts: next.ts,
                        peak_close: bought.price,
                        dca_count: 0,
                        take_profit_steps: 0,
                        partial_stop: false,
                    });
                }
            }
        }
        if position.is_some() {
            trades.push(BacktestTradeRow {
                mint: mint.clone(),
                side: "reject".into(),
                reason: "open_unresolved".into(),
                signal_ts: series.last().map(|bar| bar.ts),
                fill_ts: None,
                fill_price: None,
                size_sol: None,
                fee_sol: None,
                slippage_pct: None,
                cash_after: Some(cash),
                pnl_sol: None,
            });
        }
    }
    trades
}

struct Bought {
    price: f64,
    tokens: f64,
    debit: f64,
}

fn buy_fill(
    config: &BacktestConfig,
    mint: &str,
    signal_ts: i64,
    next: &Bar,
    order_sol: f64,
    reason: &str,
    reserves: Option<&Vec<Reserve>>,
    cash: &mut f64,
    trades: &mut Vec<BacktestTradeRow>,
) -> Option<Bought> {
    let chain_fee = chain_fee_sol(config);
    if *cash + 1e-12 < order_sol + chain_fee {
        trades.push(reject(mint, Some(signal_ts), "insufficient_cash", *cash));
        return None;
    }
    let Some(reserve) = reserve_at(reserves, next.ts) else {
        trades.push(reject(mint, Some(signal_ts), "no_liquidity", *cash));
        return None;
    };
    match quote_buy(
        next.open,
        order_sol,
        reserve,
        config.platform_fee_bps,
        config.network_fee_lamports,
        config.priority_fee_lamports,
        config.max_reserve_fraction,
    ) {
        Ok((price, tokens, debit, fee, slip)) => {
            *cash -= debit;
            trades.push(BacktestTradeRow {
                mint: mint.to_owned(),
                side: "buy".into(),
                reason: reason.to_owned(),
                signal_ts: Some(signal_ts),
                fill_ts: Some(next.ts),
                fill_price: Some(price),
                size_sol: Some(order_sol),
                fee_sol: Some(fee),
                slippage_pct: Some(slip),
                cash_after: Some(*cash),
                pnl_sol: None,
            });
            Some(Bought {
                price,
                tokens,
                debit,
            })
        }
        Err(reason) => {
            trades.push(reject(mint, Some(signal_ts), reason, *cash));
            None
        }
    }
}

fn apply_sell(
    config: &BacktestConfig,
    mint: &str,
    signal_ts: i64,
    next: &Bar,
    fraction: f64,
    reason: &str,
    reserves: Option<&Vec<Reserve>>,
    cash: &mut f64,
    trades: &mut Vec<BacktestTradeRow>,
    mut held: Position,
) -> Option<Position> {
    let share = (fraction / 100.0).clamp(0.0, 1.0);
    let sold = held.tokens * share;
    let Some(reserve) = reserve_at(reserves, next.ts) else {
        let loss = held.cost_sol;
        trades.push(BacktestTradeRow {
            mint: mint.to_owned(),
            side: "sell".into(),
            reason: "exit_unfillable".into(),
            signal_ts: Some(signal_ts),
            fill_ts: Some(next.ts),
            fill_price: None,
            size_sol: Some(0.0),
            fee_sol: Some(0.0),
            slippage_pct: None,
            cash_after: Some(*cash),
            pnl_sol: Some(-loss),
        });
        return None;
    };
    match quote_sell(
        next.open,
        sold,
        reserve,
        config.platform_fee_bps,
        config.network_fee_lamports,
        config.priority_fee_lamports,
        config.max_reserve_fraction,
    ) {
        Ok((price, net, fee, slip)) => {
            let cost_sold = held.cost_sol * share;
            *cash += net;
            trades.push(BacktestTradeRow {
                mint: mint.to_owned(),
                side: "sell".into(),
                reason: reason.to_owned(),
                signal_ts: Some(signal_ts),
                fill_ts: Some(next.ts),
                fill_price: Some(price),
                size_sol: Some(net),
                fee_sol: Some(fee),
                slippage_pct: Some(slip),
                cash_after: Some(*cash),
                pnl_sol: Some(net - cost_sold),
            });
            if share >= 1.0 - 1e-9 {
                None
            } else {
                held.tokens -= sold;
                held.cost_sol -= cost_sold;
                held.partial_stop = true;
                held.take_profit_steps = held.take_profit_steps.saturating_add(1);
                Some(held)
            }
        }
        Err(_) => {
            let loss = held.cost_sol;
            trades.push(BacktestTradeRow {
                mint: mint.to_owned(),
                side: "sell".into(),
                reason: "exit_unfillable".into(),
                signal_ts: Some(signal_ts),
                fill_ts: Some(next.ts),
                fill_price: None,
                size_sol: Some(0.0),
                fee_sol: Some(0.0),
                slippage_pct: None,
                cash_after: Some(*cash),
                pnl_sol: Some(-loss),
            });
            None
        }
    }
}

fn rule_name(rule: ExitRule) -> String {
    match rule {
        ExitRule::StopLoss => "stop_loss",
        ExitRule::Trailing => "trailing_stop",
        ExitRule::BreakEven => "break_even",
        ExitRule::TakeProfit { .. } => "take_profit",
        ExitRule::TimeLoss => "time_loss",
        ExitRule::TimeStagnation => "time_stagnation",
    }
    .to_owned()
}

enum NextStep<'a> {
    Fill(&'a Bar),
    Gap,
    End,
}

fn next_step<'a>(next: Option<&'a Bar>, bar_ts: i64, step: i64) -> NextStep<'a> {
    match next {
        None => NextStep::End,
        Some(next) if next.ts == bar_ts + step => NextStep::Fill(next),
        Some(_) => NextStep::Gap,
    }
}

fn chain_fee_sol(config: &BacktestConfig) -> f64 {
    (config.network_fee_lamports + config.priority_fee_lamports) as f64 / lamports_per_sol()
}

fn dca_multiplier(config: &BacktestConfig, held: &Position, close: f64, ts: i64) -> Option<f64> {
    let drop = config.dca_drop_pct?;
    let multiplier = config.dca_size_multiplier?;
    if held.dca_count > 0 {
        return None;
    }
    let move_pct = (close / held.entry_price - 1.0) * 100.0;
    if move_pct <= drop && ts > held.entry_ts {
        Some(multiplier)
    } else {
        None
    }
}

fn signal(config: &BacktestConfig, series: &[Bar], index: usize) -> bool {
    let close = series[index].close;
    match config.entry {
        EntryRule::CloseAbove { level } => close > level,
        EntryRule::CloseRise { lookback, min_pct } => {
            if lookback == 0 || index < lookback {
                return false;
            }
            let prior = series[index - lookback].close;
            if !(prior.is_finite() && prior > 0.0) {
                return false;
            }
            (close / prior - 1.0) * 100.0 >= min_pct
        }
    }
}

fn higher_ok(config: &BacktestConfig, series: Option<&Vec<Bar>>, decision_ts: i64) -> bool {
    let Some(rule) = &config.higher_timeframe else {
        return true;
    };
    let Some(series) = series else {
        return false;
    };
    let closed = series
        .iter()
        .rev()
        .find(|bar| bar.ts + rule.seconds <= decision_ts);
    match closed {
        None => false,
        Some(bar) if rule.require_up_bar => bar.close >= bar.open,
        Some(_) => true,
    }
}

fn eligible(info: &MintInfo, ts: i64) -> bool {
    ts >= info.born_at && !terminal(info, ts)
}

fn terminal(info: &MintInfo, ts: i64) -> bool {
    matches!(info.status.as_str(), "rugged" | "abandoned" | "untradeable") && info.status_at <= ts
}

fn reserve_at(series: Option<&Vec<Reserve>>, ts: i64) -> Option<f64> {
    series.and_then(|series| {
        series
            .iter()
            .rev()
            .find(|reserve| reserve.at <= ts)
            .map(|reserve| reserve.sol)
    })
}

fn group_bars(candles: &[StoredCandle]) -> BTreeMap<String, Vec<Bar>> {
    let mut best: BTreeMap<(String, i64), &StoredCandle> = BTreeMap::new();
    for candle in candles {
        let key = (candle.mint.clone(), candle.ts);
        match best.get(&key) {
            Some(existing) if existing.dataset_hash.as_str() <= candle.dataset_hash.as_str() => {}
            _ => {
                best.insert(key, candle);
            }
        }
    }
    let mut out: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
    for ((mint, _), candle) in best {
        out.entry(mint).or_default().push(Bar {
            ts: candle.ts,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
            pool: candle.pool.clone(),
        });
    }
    for series in out.values_mut() {
        series.sort_by_key(|bar| bar.ts);
    }
    out
}

fn mint_info(mints: &[StoredMint]) -> BTreeMap<String, MintInfo> {
    let mut out = BTreeMap::new();
    for mint in mints {
        let first = unix(&mint.first_seen_at).unwrap_or(i64::MAX);
        let created = mint.created_at_chain.as_deref().and_then(unix);
        out.insert(
            mint.mint.clone(),
            MintInfo {
                status: mint.status.clone(),
                status_at: unix(&mint.status_at).unwrap_or(first),
                born_at: created.unwrap_or(first),
            },
        );
    }
    out
}

fn reserves_by_mint(rows: &[StoredLiquidity]) -> BTreeMap<String, Vec<Reserve>> {
    let mut out: BTreeMap<String, Vec<Reserve>> = BTreeMap::new();
    for row in rows {
        let Some(at) = unix(&row.observed_at) else {
            continue;
        };
        let Some(sol) = row
            .reserve_quote
            .filter(|value| value.is_finite() && *value > 0.0)
        else {
            continue;
        };
        out.entry(row.mint.clone())
            .or_default()
            .push(Reserve { at, sol });
    }
    for series in out.values_mut() {
        series.sort_by_key(|reserve| reserve.at);
    }
    out
}

fn unix(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp())
}

fn reject(mint: &str, signal_ts: Option<i64>, reason: &str, cash: f64) -> BacktestTradeRow {
    BacktestTradeRow {
        mint: mint.to_owned(),
        side: "reject".into(),
        reason: reason.to_owned(),
        signal_ts,
        fill_ts: None,
        fill_price: None,
        size_sol: None,
        fee_sol: None,
        slippage_pct: None,
        cash_after: Some(cash),
        pnl_sol: None,
    }
}

fn exit_policy(config: &BacktestConfig) -> crate::trader::policy::ExitPolicy {
    use crate::trader::evaluators::dca::DcaConfigSnapshot;
    use crate::trader::policy::{
        BreakEvenPolicy, ExitPolicy, StopLossPolicy, TakeProfitPolicy, TimePolicy,
        TimeStagnationPolicy, TrailingPolicy,
    };
    ExitPolicy {
        stop_loss: StopLossPolicy {
            enabled: config.stop_loss_pct.is_some(),
            threshold_pct: config.stop_loss_pct.unwrap_or(100.0),
            min_hold_seconds: 0,
            allow_partial: false,
            partial_exit_default_pct: 100.0,
        },
        trailing: TrailingPolicy {
            enabled: config.trailing_activation_pct.is_some(),
            activation_pct: config.trailing_activation_pct.unwrap_or(20.0),
            distance_pct: config.trailing_distance_pct.unwrap_or(10.0),
            bands: Vec::new(),
        },
        break_even: BreakEvenPolicy {
            enabled: false,
            trigger_pct: 10.0,
            offset_pct: 0.0,
        },
        take_profit: TakeProfitPolicy {
            enabled: config.take_profit_pct.is_some(),
            levels: config
                .take_profit_pct
                .map(|profit| vec![TakeProfitLevel::sell_all_at(profit)])
                .unwrap_or_default(),
        },
        time: TimePolicy {
            enabled: false,
            loss_threshold_pct: -100.0,
            duration_seconds: f64::MAX,
        },
        time_stagnation: TimeStagnationPolicy {
            enabled: false,
            duration_seconds: f64::MAX,
            band_pct: 100.0,
        },
        dca: DcaConfigSnapshot {
            enabled: false,
            levels: match (config.dca_drop_pct, config.dca_size_multiplier) {
                (Some(drop), Some(multiplier)) => vec![DcaLevel {
                    drop_pct: drop,
                    size_multiplier: multiplier,
                }],
                _ => Vec::new(),
            },
            measure_from: DcaMeasureFrom::Average,
            cooldown_minutes: 0,
            min_hold_minutes: 0,
        },
    }
}

fn hash_inputs(
    candles: &[StoredCandle],
    higher: &[StoredCandle],
    mints: &[StoredMint],
    liquidity: &[StoredLiquidity],
    config: &BacktestConfig,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("tf={}", config.timeframe));
    for candle in candles {
        lines.push(format!(
            "c|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            candle.mint,
            candle.pool,
            candle.ts,
            candle.open,
            candle.high,
            candle.low,
            candle.close,
            candle.volume,
            candle.dataset_hash
        ));
    }
    for candle in higher {
        lines.push(format!(
            "h|{}|{}|{}|{}|{}|{}|{}|{}",
            candle.mint,
            candle.ts,
            candle.open,
            candle.high,
            candle.low,
            candle.close,
            candle.volume,
            candle.dataset_hash
        ));
    }
    for mint in mints {
        lines.push(format!(
            "m|{}|{}|{}|{}|{}|{}",
            mint.mint,
            mint.status,
            mint.status_at,
            mint.first_seen_at,
            mint.created_at_chain.as_deref().unwrap_or(""),
            mint.venue
        ));
    }
    for row in liquidity {
        lines.push(format!(
            "l|{}|{}|{}|{}|{}",
            row.mint,
            row.pool.as_deref().unwrap_or(""),
            row.observed_at,
            row.reserve_quote.unwrap_or(f64::NAN),
            row.liquidity_usd.unwrap_or(f64::NAN)
        ));
    }
    sha256_hex(&lines.join("\n"))
}

fn coverage_notes(
    config: &BacktestConfig,
    candles: &[StoredCandle],
    mints: &[StoredMint],
    liquidity: &[StoredLiquidity],
) -> String {
    let stamps: Vec<i64> = candles.iter().map(|candle| candle.ts).collect();
    let first = stamps.iter().min().copied();
    let last = stamps.iter().max().copied();
    format!(
        "timeframe={} candles={} mints={} liquidity_rows={} first_ts={:?} last_ts={:?} fee_bps={} network_lamports={} priority_lamports={} seed={}",
        config.timeframe,
        candles.len(),
        mints.len(),
        liquidity.len(),
        first,
        last,
        config.platform_fee_bps,
        config.network_fee_lamports,
        config.priority_fee_lamports,
        config.seed
    )
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::{
        ingest_snapshot_pairs, CandlePage, SnapshotPair, SourceCandle, Venue, SOURCE_BIRDEYE,
    };
    use chrono::{TimeZone, Utc};

    fn stamp(ts: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(ts, 0).single().unwrap()
    }

    fn store() -> (tempfile::TempDir, ResearchStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ResearchStore::open_path(
            &dir.path().join("research.db"),
            crate::chains::ChainId::Solana,
        )
        .unwrap();
        (dir, store)
    }

    fn ingest(
        store: &mut ResearchStore,
        mint: &str,
        timeframe: &str,
        seconds: i64,
        bars: &[(i64, f64, f64, f64)],
    ) {
        let payload = format!("{mint}|{timeframe}|{bars:?}");
        store
            .ingest_candle_page(&CandlePage {
                source: SOURCE_BIRDEYE.into(),
                params_json: "{}".into(),
                raw_payload: payload,
                mint: mint.into(),
                pool: format!("{mint}-pool"),
                timeframe: timeframe.into(),
                timeframe_seconds: seconds,
                fetched_at: stamp(bars[0].0),
                venue: Venue::Other,
                dex_id: "fixture".into(),
                pool_created_at: Some(stamp(bars[0].0).to_rfc3339()),
                candles: bars
                    .iter()
                    .map(|(ts, open, _high, close)| SourceCandle {
                        ts: *ts,
                        open: *open,
                        high: *_high,
                        low: open.min(*close),
                        close: *close,
                        volume: 1.0,
                    })
                    .collect(),
            })
            .unwrap();
    }

    fn reserve(store: &mut ResearchStore, mint: &str, at: i64, sol: f64) {
        ingest_snapshot_pairs(
            store,
            "fixture",
            &format!("{mint}-{at}"),
            &format!("reserve-{mint}-{at}-{sol}"),
            stamp(at),
            &[SnapshotPair {
                mint: mint.into(),
                pool: format!("{mint}-pool"),
                dex_id: "fixture".into(),
                venue: Venue::Other,
                symbol: None,
                name: None,
                created_at_chain: None,
                liquidity_usd: Some(sol),
                reserve_quote: Some(sol),
                source_event_time: Some(stamp(at)),
            }],
            None,
        )
        .unwrap();
    }

    fn buys(report: &BacktestReport) -> Vec<&BacktestTradeRow> {
        report
            .trades
            .iter()
            .filter(|trade| trade.side == "buy")
            .collect()
    }

    #[test]
    fn a_signal_fills_on_the_next_bar_open_and_cannot_see_that_bar() {
        let (_dir, mut store) = store();
        // Close 80 is the signal. The next bar opens at 11 and closes at 1.
        // A look-ahead fill would use 80 or 1. The engine must use 11.
        ingest(
            &mut store,
            "Look",
            "1m",
            60,
            &[
                (0, 10.0, 10.0, 10.0),
                (60, 10.0, 10.0, 80.0),
                (120, 11.0, 11.0, 1.0),
            ],
        );
        reserve(&mut store, "Look", 0, 1_000.0);
        let mut config = BacktestConfig::app_fees("look", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 50.0 };
        config.order_sol = 0.05;
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        let report = run(&mut store, &config).unwrap();
        let buy = buys(&report);
        assert_eq!(buy.len(), 1, "{:?}", report.trades);
        assert_eq!(buy[0].signal_ts, Some(60));
        assert_eq!(buy[0].fill_ts, Some(120));
        let price = buy[0].fill_price.unwrap();
        assert!(
            price > 11.0 && price < 11.01,
            "fill {price} used a later price"
        );
        assert!(report
            .trades
            .iter()
            .all(|trade| trade.pnl_sol.unwrap_or(0.0) <= 0.0));
    }

    #[test]
    fn rugged_and_candleless_mints_stay_in_the_result_and_are_not_profits() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Survivor",
            "1m",
            60,
            &[
                (0, 1.0, 1.0, 2.0),
                (60, 1.0, 1.0, 1.0),
                (120, 1.0, 1.0, 1.0),
            ],
        );
        ingest(
            &mut store,
            "Rug",
            "1m",
            60,
            &[
                (0, 1.0, 1.0, 50.0),
                (60, 1.0, 100.0, 100.0),
                (120, 100.0, 100.0, 100.0),
            ],
        );
        reserve(&mut store, "Survivor", 0, 1_000.0);
        reserve(&mut store, "Rug", 0, 1_000.0);
        store
            .mark_lifecycle("Rug", "rugged", "rugcheck", stamp(0))
            .unwrap();
        store
            .observe_bonding_curve_mint("Ghost", None, None, "solana-rpc", stamp(0))
            .unwrap();
        let mut config = BacktestConfig::app_fees("universe", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 0.0 };
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        let report = run(&mut store, &config).unwrap();
        let mints: Vec<_> = report
            .trades
            .iter()
            .map(|trade| trade.mint.as_str())
            .collect();
        assert!(mints.contains(&"Survivor"), "{mints:?}");
        assert!(mints.contains(&"Rug"), "{mints:?}");
        assert!(mints.contains(&"Ghost"), "{mints:?}");
        assert!(report
            .trades
            .iter()
            .any(|trade| trade.mint == "Rug" && trade.reason == "not_in_universe"));
        assert!(report
            .trades
            .iter()
            .any(|trade| trade.mint == "Ghost" && trade.reason == "no_candles"));
        assert!(report
            .trades
            .iter()
            .filter(|trade| trade.mint == "Rug")
            .all(|trade| trade.side != "buy"));
        assert!(report
            .trades
            .iter()
            .filter(|trade| trade.mint == "Ghost")
            .all(|trade| trade.pnl_sol.is_none()));
    }

    #[test]
    fn a_gap_does_not_become_a_fill_or_a_profit() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Gap",
            "1m",
            60,
            &[(0, 1.0, 1.0, 10.0), (180, 1.0, 1.0, 1.0)],
        );
        reserve(&mut store, "Gap", 0, 1_000.0);
        let mut config = BacktestConfig::app_fees("gap", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 5.0 };
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        let report = run(&mut store, &config).unwrap();
        assert!(buys(&report).is_empty(), "{:?}", report.trades);
        assert!(report
            .trades
            .iter()
            .any(|trade| trade.reason == "missing_next_bar"));
        assert_eq!(report.ending_cash_sol, config.starting_cash_sol);
    }

    #[test]
    fn stop_loss_is_checked_before_take_profit_and_fills_next_open() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Stop",
            "1m",
            60,
            &[
                (0, 100.0, 100.0, 60.0),
                (60, 100.0, 100.0, 100.0),
                (120, 100.0, 100.0, 80.0),
                (180, 90.0, 90.0, 90.0),
            ],
        );
        reserve(&mut store, "Stop", 0, 10_000.0);
        let mut config = BacktestConfig::app_fees("stop", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 50.0 };
        config.order_sol = 0.05;
        config.stop_loss_pct = Some(10.0);
        config.take_profit_pct = Some(5.0);
        let report = run(&mut store, &config).unwrap();
        let sell = report
            .trades
            .iter()
            .find(|trade| trade.side == "sell")
            .unwrap();
        assert_eq!(sell.reason, "stop_loss");
        assert_eq!(sell.signal_ts, Some(120));
        assert_eq!(sell.fill_ts, Some(180));
        assert!(sell.fill_price.unwrap() < 90.0);
        assert!(sell.pnl_sol.unwrap() < 0.0);

        let loss = exit_rules::decide(
            &ExitInputs {
                average_entry_price: 100.0,
                current_price: 80.0,
                basis_peak_price: 100.0,
                held_seconds: 60,
                partial_exits_taken: false,
                take_profit_levels_taken: 0,
            },
            &exit_policy(&config),
        );
        assert_eq!(loss.outcome.unwrap().rule, ExitRule::StopLoss);
    }

    #[test]
    fn a_trailing_stop_arms_off_the_peak_and_sells_the_next_open() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Trail",
            "1m",
            60,
            &[
                (0, 100.0, 100.0, 60.0),
                (60, 100.0, 100.0, 120.0),
                (120, 110.0, 110.0, 110.0),
                (180, 112.0, 112.0, 112.0),
            ],
        );
        reserve(&mut store, "Trail", 0, 10_000.0);
        let mut config = BacktestConfig::app_fees("trail", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 50.0 };
        config.stop_loss_pct = Some(50.0);
        config.take_profit_pct = Some(80.0);
        config.trailing_activation_pct = Some(10.0);
        config.trailing_distance_pct = Some(5.0);
        let report = run(&mut store, &config).unwrap();
        let sell = report
            .trades
            .iter()
            .find(|trade| trade.side == "sell")
            .unwrap();
        assert_eq!(sell.reason, "trailing_stop");
        assert_eq!(sell.signal_ts, Some(120));
        assert_eq!(sell.fill_ts, Some(180));
    }

    #[test]
    fn take_profit_fills_the_next_open_not_the_signal_close() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Tp",
            "1m",
            60,
            &[
                (0, 100.0, 100.0, 60.0),
                (60, 100.0, 100.0, 130.0),
                (120, 125.0, 125.0, 125.0),
            ],
        );
        reserve(&mut store, "Tp", 0, 10_000.0);
        let mut config = BacktestConfig::app_fees("tp", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 50.0 };
        config.stop_loss_pct = Some(50.0);
        config.take_profit_pct = Some(20.0);
        let report = run(&mut store, &config).unwrap();
        let sell = report
            .trades
            .iter()
            .find(|trade| trade.side == "sell")
            .unwrap();
        assert_eq!(sell.reason, "take_profit");
        assert_eq!(sell.fill_ts, Some(120));
        assert!(sell.fill_price.unwrap() < 125.0 && sell.fill_price.unwrap() > 124.0);
    }

    #[test]
    fn fees_and_a_larger_order_move_the_buy_price_up() {
        let (small_price, _, small_debit, small_fee, _) =
            quote_buy(10.0, 1.0, 100.0, 50, 2_000_000, 10_000, 1.0).unwrap();
        let (large_price, _, _, _, _) =
            quote_buy(10.0, 5.0, 100.0, 50, 2_000_000, 10_000, 1.0).unwrap();
        let impact = 1.0 / 101.0;
        let expected_price = 10.0 * (1.0 + impact);
        assert!(
            (small_price - expected_price).abs() < 1e-9,
            "{small_price} vs {expected_price}"
        );
        let platform = 0.005;
        let chain = 2_010_000.0 / 1_000_000_000.0;
        assert!((small_fee - (platform + chain)).abs() < 1e-12);
        assert!((small_debit - (1.0 + chain)).abs() < 1e-12);
        assert!(large_price > small_price);
    }

    #[test]
    fn an_order_bigger_than_the_reserve_limit_is_not_filled() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Thin",
            "1m",
            60,
            &[(0, 1.0, 1.0, 2.0), (60, 1.0, 1.0, 1.0)],
        );
        reserve(&mut store, "Thin", 0, 0.2);
        let mut config = BacktestConfig::app_fees("thin", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 0.0 };
        config.order_sol = 0.05;
        config.max_reserve_fraction = 0.05;
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        let report = run(&mut store, &config).unwrap();
        assert!(report
            .trades
            .iter()
            .any(|trade| trade.reason == "exceeds_liquidity"));
        assert!(buys(&report).is_empty());
        assert_eq!(report.ending_cash_sol, config.starting_cash_sol);
    }

    #[test]
    fn dca_and_max_hold_use_only_that_strategy_cash() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Add",
            "1m",
            60,
            &[
                (0, 100.0, 100.0, 60.0),
                (60, 100.0, 100.0, 70.0),
                (120, 70.0, 70.0, 70.0),
                (180, 70.0, 70.0, 70.0),
                (240, 70.0, 70.0, 70.0),
            ],
        );
        reserve(&mut store, "Add", 0, 10_000.0);
        let mut config = BacktestConfig::app_fees("dca", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 50.0 };
        config.order_sol = 0.1;
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        config.dca_drop_pct = Some(-20.0);
        config.dca_size_multiplier = Some(0.5);
        config.max_hold_seconds = Some(120);
        let report = run(&mut store, &config).unwrap();
        assert!(report.trades.iter().any(|trade| trade.reason == "dca"));
        assert!(report.trades.iter().any(|trade| trade.reason == "max_hold"));
        let mut small = config.clone();
        small.strategy_id = "tiny".into();
        small.starting_cash_sol = 0.001;
        small.order_sol = 0.05;
        let tiny = run(&mut store, &small).unwrap();
        assert!(tiny
            .trades
            .iter()
            .any(|trade| trade.reason == "insufficient_cash"));
        assert_ne!(report.ending_cash_sol, tiny.ending_cash_sol);
        assert!(tiny.ending_cash_sol <= tiny.starting_cash_sol + 1e-9);
    }

    #[test]
    fn a_higher_timeframe_bar_is_invisible_until_it_has_closed() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Ht",
            "1m",
            60,
            &[(3600, 1.0, 1.0, 2.0), (3660, 1.5, 1.5, 1.5)],
        );
        ingest(&mut store, "Ht", "1h", 3600, &[(0, 1.0, 2.0, 2.0)]);
        reserve(&mut store, "Ht", 0, 1_000.0);
        let mut config = BacktestConfig::app_fees("ht", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 0.0 };
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        config.higher_timeframe = Some(HigherTimeframe {
            timeframe: "1h".into(),
            seconds: 3600,
            require_up_bar: true,
        });
        let report = run(&mut store, &config).unwrap();
        let buy = buys(&report);
        assert_eq!(buy.len(), 1, "{:?}", report.trades);
        assert_eq!(buy[0].signal_ts, Some(3600));
        assert_eq!(buy[0].fill_ts, Some(3660));
    }

    #[test]
    fn the_same_config_and_data_hash_repeat() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Same",
            "1m",
            60,
            &[(0, 1.0, 1.0, 2.0), (60, 1.0, 1.0, 1.0)],
        );
        reserve(&mut store, "Same", 0, 500.0);
        let mut config = BacktestConfig::app_fees("same", "1m", 60);
        config.entry = EntryRule::CloseAbove { level: 0.0 };
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        let first = run(&mut store, &config).unwrap();
        let second = run(&mut store, &config).unwrap();
        assert_eq!(first.config_hash, second.config_hash);
        assert_eq!(first.data_hash, second.data_hash);
        assert_eq!(first.seed, second.seed);
        assert_eq!(first.run_id, second.run_id);
        let reasons: Vec<_> = first
            .trades
            .iter()
            .map(|trade| trade.reason.clone())
            .collect();
        let again: Vec<_> = second
            .trades
            .iter()
            .map(|trade| trade.reason.clone())
            .collect();
        assert_eq!(reasons, again);
    }

    #[tokio::test]
    async fn a_backtest_run_keeps_the_process_from_signing() {
        let (_dir, mut store) = store();
        ingest(
            &mut store,
            "Mode",
            "1m",
            60,
            &[(0, 1.0, 1.0, 2.0), (60, 1.0, 1.0, 1.0)],
        );
        reserve(&mut store, "Mode", 0, 100.0);
        let mut config = BacktestConfig::app_fees("mode", "1m", 60);
        config.stop_loss_pct = None;
        config.take_profit_pct = None;
        run(&mut store, &config).unwrap();
        let secret = crate::chains::solana::accounts::generate_key().1;
        let parsed = tokio::spawn(async move {
            crate::chains::solana::accounts::parse_private_key(secret.as_str())
        })
        .await
        .unwrap();
        assert!(matches!(
            parsed,
            Err(crate::chains::solana::Error::ResearchModeRefused { .. })
        ));
    }
}
