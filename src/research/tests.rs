// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

use chrono::{TimeZone, Utc};

use super::*;

fn at() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0).single().unwrap()
}

fn store() -> (tempfile::TempDir, ResearchStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = ResearchStore::open_path(
        &dir.path().join("research.db"),
        crate::chains::ChainId::Solana,
    )
    .expect("open store");
    assert!(
        crate::global::in_research_mode(),
        "opening the research store engages research mode"
    );
    (dir, store)
}

#[test]
fn gecko_candles_keep_the_upstream_timestamp() {
    let (_dir, mut store) = store();
    let raw = r#"{"data":{"attributes":{"ohlcv_list":[[1699920000,1.5,2.0,1.25,1.75,10]]}}}"#;
    let page = gecko_ohlcv_page(GeckoOhlcvPage {
        mint: "MintPump".into(),
        pool: "PoolPump".into(),
        timeframe: "1m".into(),
        timeframe_seconds: 60,
        dex_id: "pump-fun".into(),
        pool_created_at: Some("2023-11-14T00:00:00Z".into()),
        fetched_at: at(),
        raw_payload: raw.into(),
        params_json: r#"{"pool":"PoolPump","timeframe":"1m"}"#.into(),
    })
    .expect("parse");
    assert_eq!(page.candles[0].ts, 1_699_920_000);
    assert_ne!(page.candles[0].ts, at().timestamp());
    let outcome = store.ingest_candle_page(&page).expect("ingest");
    assert!(outcome.inserted);
    assert_eq!(
        store.candle_timestamps("MintPump", "1m").unwrap(),
        vec![1_699_920_000]
    );
    let mint = store.mint("MintPump").unwrap().unwrap();
    assert_eq!(mint.venue, "bonding_curve");
    assert_eq!(
        mint.created_at_chain.as_deref(),
        Some("2023-11-14T00:00:00Z")
    );
    assert_eq!(mint.status, "active");
    assert_eq!(
        store
            .tradeable_mints("1m", 1_699_920_000, 1_699_920_000)
            .unwrap(),
        vec!["MintPump".to_owned()]
    );
}

#[test]
fn the_same_payload_is_not_stored_twice_and_its_hash_stays() {
    let (_dir, mut store) = store();
    let raw = r#"{"data":{"attributes":{"ohlcv_list":[[100,1,1,1,1,1]]}}}"#;
    let page = gecko_ohlcv_page(GeckoOhlcvPage {
        mint: "Mint".into(),
        pool: "Pool".into(),
        timeframe: "5m".into(),
        timeframe_seconds: 300,
        dex_id: "raydium".into(),
        pool_created_at: None,
        fetched_at: at(),
        raw_payload: raw.into(),
        params_json: "{}".into(),
    })
    .unwrap();
    let first = store.ingest_candle_page(&page).unwrap();
    let second = store.ingest_candle_page(&page).unwrap();
    assert_eq!(first.content_hash, second.content_hash);
    assert!(!second.inserted);
    assert_eq!(store.dataset_count().unwrap(), 1);
    assert_eq!(store.candle_count().unwrap(), 1);
}

#[test]
fn a_launch_with_no_candles_is_missing_and_not_tradeable() {
    let (_dir, mut store) = store();
    store
        .observe_bonding_curve_mint(
            "DeadCurve",
            Some("Creator"),
            Some("2024-01-01T00:00:00Z"),
            "solana-rpc",
            at(),
        )
        .unwrap();
    let mint = store.mint("DeadCurve").unwrap().unwrap();
    assert_eq!(mint.venue, "bonding_curve");
    assert_eq!(mint.status, "unknown");
    assert_eq!(
        store.unfilled_gap_reasons("DeadCurve").unwrap(),
        vec!["no_candles".to_owned()]
    );
    assert!(store
        .tradeable_mints("1m", 0, 2_000_000_000)
        .unwrap()
        .is_empty());
}

#[test]
fn an_empty_candle_page_is_a_gap_and_not_a_flat_series() {
    let (_dir, mut store) = store();
    let page = gecko_ohlcv_page(GeckoOhlcvPage {
        mint: "NoBars".into(),
        pool: "Pool".into(),
        timeframe: "1m".into(),
        timeframe_seconds: 60,
        dex_id: "pump-fun".into(),
        pool_created_at: None,
        fetched_at: at(),
        raw_payload: r#"{"data":{"attributes":{"ohlcv_list":[]}}}"#.into(),
        params_json: "{}".into(),
    })
    .unwrap();
    store.ingest_candle_page(&page).unwrap();
    assert_eq!(store.candle_count().unwrap(), 0);
    assert_eq!(
        store.unfilled_gap_reasons("NoBars").unwrap(),
        vec!["no_candles".to_owned()]
    );
    assert!(store.tradeable_mints("1m", 0, 9).unwrap().is_empty());
}

#[test]
fn a_rugged_mint_stays_rugged_when_later_data_arrives() {
    let (_dir, mut store) = store();
    let view = rugcheck_report(r#"{"mint":"RugMint","rugged":true,"creator":"Dev"}"#).unwrap();
    assert!(view.rugged);
    store
        .ingest_raw_dataset(
            "rugcheck",
            &view.mint,
            r#"{"mint":"RugMint","rugged":true}"#,
            at(),
        )
        .unwrap();
    store
        .mark_lifecycle(&view.mint, "rugged", "rugcheck", at())
        .unwrap();
    let page = gecko_ohlcv_page(GeckoOhlcvPage {
        mint: "RugMint".into(),
        pool: "Pool".into(),
        timeframe: "1h".into(),
        timeframe_seconds: 3600,
        dex_id: "raydium".into(),
        pool_created_at: None,
        fetched_at: at(),
        raw_payload: r#"{"data":{"attributes":{"ohlcv_list":[[100,1,1,1,1,1]]}}}"#.into(),
        params_json: "{}".into(),
    })
    .unwrap();
    store.ingest_candle_page(&page).unwrap();
    let mint = store.mint("RugMint").unwrap().unwrap();
    assert_eq!(mint.status, "rugged");
    assert_eq!(store.mint_count().unwrap(), 1);
    assert!(store.tradeable_mints("1h", 0, 1_000).unwrap().is_empty());
}

#[test]
fn a_hole_between_candles_is_marked_and_blocks_the_window() {
    let (_dir, mut store) = store();
    let page = gecko_ohlcv_page(GeckoOhlcvPage {
        mint: "Gappy".into(),
        pool: "Pool".into(),
        timeframe: "1m".into(),
        timeframe_seconds: 60,
        dex_id: "orca".into(),
        pool_created_at: None,
        fetched_at: at(),
        raw_payload: r#"{"data":{"attributes":{"ohlcv_list":[[60,1,1,1,1,1],[300,1,1,1,1,1]]}}}"#
            .into(),
        params_json: "{}".into(),
    })
    .unwrap();
    store.ingest_candle_page(&page).unwrap();
    assert_eq!(
        store.unfilled_gap_reasons("Gappy").unwrap(),
        vec!["hole".to_owned()]
    );
    assert!(store.tradeable_mints("1m", 60, 300).unwrap().is_empty());
}

#[test]
fn dexscreener_snapshot_keeps_liquidity_without_inventing_candles() {
    let (_dir, mut store) = store();
    let raw = r#"{"pairs":[{"chainId":"solana","dexId":"pump-fun","pairAddress":"Curve","baseToken":{"address":"MintP","name":"Pump","symbol":"P"},"liquidity":{"usd":12.5,"quote":1.0},"pairCreatedAt":1700000000000}]}"#;
    let (_, pairs) = dexscreener_snapshot(raw, "solana", at()).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].venue, Venue::BondingCurve);
    assert!(pairs[0].source_event_time.is_none());
    assert_eq!(
        pairs[0].created_at_chain.as_deref(),
        Some("2023-11-14T22:13:20+00:00")
    );
    ingest_snapshot_pairs(
        &mut store,
        "dexscreener",
        "{}",
        raw,
        at(),
        &pairs,
        Some("MintP"),
    )
    .unwrap();
    assert_eq!(store.candle_count().unwrap(), 0);
    let mint = store.mint("MintP").unwrap().unwrap();
    assert_eq!(mint.venue, "bonding_curve");
    assert!(store
        .tradeable_mints("1m", 0, 9_999_999_999)
        .unwrap()
        .is_empty());
}

#[test]
fn birdeye_is_only_a_source_name_the_store_already_accepts() {
    let (_dir, mut store) = store();
    let page = CandlePage {
        source: SOURCE_BIRDEYE.into(),
        params_json: "{}".into(),
        raw_payload: "birdeye-fixture".into(),
        mint: "Later".into(),
        pool: "Pool".into(),
        timeframe: "1m".into(),
        timeframe_seconds: 60,
        fetched_at: at(),
        venue: Venue::Other,
        dex_id: "birdeye".into(),
        pool_created_at: None,
        candles: vec![SourceCandle {
            ts: 50,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }],
    };
    store.ingest_candle_page(&page).unwrap();
    assert_eq!(store.candle_timestamps("Later", "1m").unwrap(), vec![50]);
}

#[test]
fn the_schema_does_not_delete_mints() {
    let source = include_str!("store.rs");
    assert!(
        !source.to_ascii_lowercase().contains("delete from mints"),
        "mints are historical identity and are never deleted"
    );
}

#[tokio::test(start_paused = true)]
async fn the_geckoterminal_pace_is_one_call_each_six_seconds() {
    let pace = Pace::geckoterminal();
    pace.acquire().await;
    let second = tokio::spawn(async move {
        pace.acquire().await;
    });
    tokio::task::yield_now().await;
    assert!(!second.is_finished());
    tokio::time::advance(GECKOTERMINAL_GAP).await;
    second.await.unwrap();
}

#[tokio::test]
async fn retries_are_recorded_and_a_later_success_clears_the_failure_streak() {
    let (_dir, mut store) = store();
    let collector = Collector {
        pace: Pace::every(std::time::Duration::ZERO),
        max_attempts: 3,
        source: "geckoterminal".into(),
    };
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_fetch = attempts.clone();
    let body = collector
        .pull(&mut store, move || {
            let attempts_fetch = attempts_fetch.clone();
            async move {
                let n = attempts_fetch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err(CollectError {
                        message: "429".into(),
                        retryable: true,
                    })
                } else {
                    Ok(r#"{"data":{"attributes":{"ohlcv_list":[[10,1,1,1,1,1]]}}}"#.to_owned())
                }
            }
        })
        .await
        .expect("third try succeeds");
    let page = gecko_ohlcv_page(GeckoOhlcvPage {
        mint: "Retried".into(),
        pool: "Pool".into(),
        timeframe: "1m".into(),
        timeframe_seconds: 60,
        dex_id: "pump-fun".into(),
        pool_created_at: None,
        fetched_at: at(),
        raw_payload: body,
        params_json: "{}".into(),
    })
    .unwrap();
    store.ingest_candle_page(&page).unwrap();
    let health = store.health("geckoterminal").unwrap().unwrap();
    assert_eq!(health.consecutive_failures, 0);
    assert!(health.requests >= 3);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}
