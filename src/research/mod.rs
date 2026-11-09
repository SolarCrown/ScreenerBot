// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Historical research data. This is not the live trading cache.
//!
//! `research.db` keeps timestamped candles, launch metadata, liquidity
//! observations, lifecycle status and explicit gaps. Housekeeping does not
//! evict those rows, and nothing here deletes a mint. Opening the store
//! engages research mode for the process, so a backtest or a forward test
//! that uses this data cannot load a signing key or submit a swap.
//!
//! Free sources only. A later Birdeye client can call [`ResearchStore::ingest_candle_page`]
//! with [`SOURCE_BIRDEYE`] without a schema change.

mod collect;
mod parse;
mod sample;
mod simulate;
pub(crate) mod store;
mod venue;

pub use collect::{
    collect_pages, CollectError, Collector, Pace, DEXSCREENER_GAP, GECKOTERMINAL_GAP,
};
pub use parse::{
    dexscreener_snapshot, gecko_ohlcv_page, rugcheck_report, GeckoOhlcvPage, RugcheckView,
};
pub use sample::{collect_memecoin_sample, SampleQuotas, SampleRequest};
pub use simulate::{
    quote_buy, quote_sell, run as run_backtest, BacktestConfig, BacktestReport, EntryRule,
    HigherTimeframe, APP_NETWORK_FEE_LAMPORTS, APP_PLATFORM_FEE_BPS, APP_PRIORITY_FEE_LAMPORTS,
};
pub use store::{
    ingest_snapshot_pairs, CandlePage, IngestOutcome, ResearchStore, SnapshotPair, SourceCandle,
    SOURCE_BIRDEYE,
};
pub use venue::{venue_from_dex_id, Venue};

#[cfg(test)]
mod tests;
