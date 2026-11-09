// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Parsers for the free sources. Candle timestamps are taken from the payload.
//! A missing list becomes an empty page, which the store records as a gap.

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

use super::store::{CandlePage, SnapshotPair, SourceCandle};
use super::venue::venue_from_dex_id;

/// One GeckoTerminal OHLCV response, already fetched. `fetched_at` is when this
/// process stored it. Each candle's timestamp is the first element of the
/// upstream list.
pub struct GeckoOhlcvPage {
    pub mint: String,
    pub pool: String,
    pub timeframe: String,
    pub timeframe_seconds: i64,
    pub dex_id: String,
    pub pool_created_at: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub raw_payload: String,
    pub params_json: String,
}

pub fn gecko_ohlcv_page(input: GeckoOhlcvPage) -> Result<CandlePage, String> {
    let value: serde_json::Value =
        serde_json::from_str(&input.raw_payload).map_err(|error| error.to_string())?;
    let list = value
        .pointer("/data/attributes/ohlcv_list")
        .and_then(|item| item.as_array())
        .ok_or_else(|| "geckoterminal payload has no data.attributes.ohlcv_list".to_owned())?;
    let mut candles = Vec::with_capacity(list.len());
    for row in list {
        let row = row
            .as_array()
            .ok_or_else(|| "ohlcv row is not an array".to_owned())?;
        if row.len() < 6 {
            return Err(format!("ohlcv row has {} fields", row.len()));
        }
        let ts = json_i64(&row[0]).ok_or_else(|| "ohlcv timestamp is not a number".to_owned())?;
        candles.push(SourceCandle {
            ts,
            open: json_f64(&row[1])?,
            high: json_f64(&row[2])?,
            low: json_f64(&row[3])?,
            close: json_f64(&row[4])?,
            volume: json_f64(&row[5])?,
        });
    }
    Ok(CandlePage {
        source: "geckoterminal".to_owned(),
        params_json: input.params_json,
        raw_payload: input.raw_payload,
        mint: input.mint,
        pool: input.pool,
        timeframe: input.timeframe,
        timeframe_seconds: input.timeframe_seconds,
        fetched_at: input.fetched_at,
        venue: venue_from_dex_id(&input.dex_id),
        dex_id: input.dex_id,
        pool_created_at: input.pool_created_at,
        candles,
    })
}

#[derive(Debug, Clone)]
pub struct SnapshotMint {
    pub expected_mint: Option<String>,
}

#[derive(Deserialize)]
struct DexPairs {
    #[serde(default)]
    pairs: Vec<DexPair>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DexPair {
    chain_id: Option<String>,
    dex_id: Option<String>,
    pair_address: Option<String>,
    base_token: Option<DexToken>,
    liquidity: Option<DexLiquidity>,
    pair_created_at: Option<i64>,
}

#[derive(Deserialize)]
struct DexToken {
    address: Option<String>,
    name: Option<String>,
    symbol: Option<String>,
}

#[derive(Deserialize)]
struct DexLiquidity {
    usd: Option<f64>,
    quote: Option<f64>,
}

/// DexScreener's public API is a snapshot. `pairCreatedAt`, when present, is
/// pool-creation metadata. It is not a candle. Liquidity is stamped with
/// `fetched_at` unless that creation field is the only time we were given,
/// and even then the observation records that the payload had no trade time.
pub fn dexscreener_snapshot(
    raw_payload: &str,
    network: &str,
    _fetched_at: DateTime<Utc>,
) -> Result<(String, Vec<SnapshotPair>), String> {
    let parsed: DexPairs = serde_json::from_str(raw_payload).map_err(|error| error.to_string())?;
    let mut pairs = Vec::new();
    for pair in parsed.pairs {
        if pair
            .chain_id
            .as_deref()
            .is_some_and(|chain| chain != network)
        {
            continue;
        }
        let Some(mint) = pair
            .base_token
            .as_ref()
            .and_then(|token| token.address.clone())
        else {
            continue;
        };
        let dex_id = pair.dex_id.unwrap_or_default();
        let created_at_chain = pair.pair_created_at.and_then(epoch_to_rfc3339);
        pairs.push(SnapshotPair {
            mint,
            pool: pair.pair_address.unwrap_or_default(),
            venue: venue_from_dex_id(&dex_id),
            dex_id,
            symbol: pair
                .base_token
                .as_ref()
                .and_then(|token| token.symbol.clone()),
            name: pair
                .base_token
                .as_ref()
                .and_then(|token| token.name.clone()),
            created_at_chain,
            liquidity_usd: pair.liquidity.as_ref().and_then(|liquidity| liquidity.usd),
            reserve_quote: pair
                .liquidity
                .as_ref()
                .and_then(|liquidity| liquidity.quote),
            source_event_time: None,
        });
    }
    Ok((raw_payload.to_owned(), pairs))
}

#[derive(Deserialize)]
struct RugcheckReport {
    mint: String,
    #[serde(default)]
    rugged: Option<bool>,
    creator: Option<String>,
    token: Option<RugcheckToken>,
}

#[derive(Deserialize)]
struct RugcheckToken {
    name: Option<String>,
    symbol: Option<String>,
}

/// Rugcheck's `rugged` flag is a point-in-time report. A true value sets the
/// lifecycle to `rugged` and that status is not cleared by a later candle.
pub fn rugcheck_report(raw_payload: &str) -> Result<RugcheckView, String> {
    let report: RugcheckReport =
        serde_json::from_str(raw_payload).map_err(|error| error.to_string())?;
    Ok(RugcheckView {
        mint: report.mint,
        rugged: report.rugged.unwrap_or(false),
        creator: report.creator,
        name: report.token.as_ref().and_then(|token| token.name.clone()),
        symbol: report.token.as_ref().and_then(|token| token.symbol.clone()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RugcheckView {
    pub mint: String,
    pub rugged: bool,
    pub creator: Option<String>,
    pub name: Option<String>,
    pub symbol: Option<String>,
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
}

fn json_f64(value: &serde_json::Value) -> Result<f64, String> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|number| number as f64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .ok_or_else(|| format!("ohlcv field is not a number: {value}"))
}

fn epoch_to_rfc3339(raw: i64) -> Option<String> {
    let seconds = if raw > 10_000_000_000 {
        raw / 1000
    } else {
        raw
    };
    Utc.timestamp_opt(seconds, 0)
        .single()
        .map(|time| time.to_rfc3339())
}
