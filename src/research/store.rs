// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! `research.db`: the non-evicting historical store.
//!
//! Rows are keyed by the source payload's hash and by the timestamps the
//! source sent. A missing series is a gap row. It is not a flat candle and it
//! is not evidence the mint was safe or profitable. Mints are never deleted.

use std::path::Path;

use blake3::Hash;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::database::configure::{self, configure_connection};
use crate::database::schema::{ddl, SchemaContext, StoreSchema};
use crate::database::WriteTransaction;
use crate::errors::DatabaseError;

use super::venue::Venue;

/// Source name reserved for a future Birdeye candle client. The table accepts
/// it today; this build does not call Birdeye.
pub const SOURCE_BIRDEYE: &str = "birdeye";

pub(crate) const SCHEMA: StoreSchema = StoreSchema {
    version: 1,
    min_reader_version: 1,
    baseline: 1,
    renames: None,
    step: create_schema,
    legacy_stamp: None,
};

fn create_schema(tx: &Transaction<'_>, _: &SchemaContext) -> Result<(), DatabaseError> {
    ddl::run_batch(
        tx,
        "create research schema",
        "
        CREATE TABLE IF NOT EXISTS datasets (
            content_hash TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            retrieved_at TEXT NOT NULL,
            params_json TEXT NOT NULL,
            payload TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS ingest_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source TEXT NOT NULL,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            ok INTEGER NOT NULL,
            error TEXT,
            requests INTEGER NOT NULL DEFAULT 0,
            retries INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS source_health (
            source TEXT PRIMARY KEY,
            last_ok_at TEXT,
            last_error_at TEXT,
            last_error TEXT,
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            requests INTEGER NOT NULL DEFAULT 0,
            retries INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS mints (
            chain_id TEXT NOT NULL,
            mint TEXT NOT NULL,
            first_seen_at TEXT NOT NULL,
            first_source TEXT NOT NULL,
            created_at_chain TEXT,
            creator TEXT,
            symbol TEXT,
            name TEXT,
            venue TEXT NOT NULL,
            dex_id TEXT,
            status TEXT NOT NULL,
            status_at TEXT NOT NULL,
            status_source TEXT NOT NULL,
            PRIMARY KEY (chain_id, mint)
        );
        CREATE TABLE IF NOT EXISTS pools (
            chain_id TEXT NOT NULL,
            pool TEXT NOT NULL,
            mint TEXT NOT NULL,
            dex_id TEXT NOT NULL,
            venue TEXT NOT NULL,
            created_at TEXT,
            first_seen_at TEXT NOT NULL,
            source TEXT NOT NULL,
            dataset_hash TEXT NOT NULL,
            PRIMARY KEY (chain_id, pool)
        );
        CREATE TABLE IF NOT EXISTS candles (
            dataset_hash TEXT NOT NULL,
            chain_id TEXT NOT NULL,
            mint TEXT NOT NULL,
            pool TEXT NOT NULL,
            timeframe TEXT NOT NULL,
            ts INTEGER NOT NULL,
            open REAL NOT NULL,
            high REAL NOT NULL,
            low REAL NOT NULL,
            close REAL NOT NULL,
            volume REAL NOT NULL,
            source TEXT NOT NULL,
            fetched_at TEXT NOT NULL,
            PRIMARY KEY (dataset_hash, pool, timeframe, ts)
        );
        CREATE INDEX IF NOT EXISTS idx_research_candles_mint
            ON candles (chain_id, mint, timeframe, ts);
        CREATE TABLE IF NOT EXISTS liquidity_observations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            dataset_hash TEXT NOT NULL,
            chain_id TEXT NOT NULL,
            mint TEXT NOT NULL,
            pool TEXT,
            observed_at TEXT NOT NULL,
            fetched_at TEXT NOT NULL,
            source_had_event_time INTEGER NOT NULL,
            liquidity_usd REAL,
            reserve_quote REAL,
            source TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS gaps (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chain_id TEXT NOT NULL,
            mint TEXT NOT NULL,
            timeframe TEXT,
            start_ts INTEGER,
            end_ts INTEGER,
            reason TEXT NOT NULL,
            source TEXT NOT NULL,
            noted_at TEXT NOT NULL,
            filled INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_research_gaps_mint
            ON gaps (chain_id, mint, filled, reason);
        ",
    )
}

/// What one ingest of a payload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub content_hash: String,
    /// False when this exact payload was already stored.
    pub inserted: bool,
}

/// One candle whose timestamp came from the source payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceCandle {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// A normalized candle page. `fetched_at` is when we stored it. Candle
/// timestamps are `SourceCandle::ts`, never `fetched_at`.
#[derive(Debug, Clone)]
pub struct CandlePage {
    pub source: String,
    pub params_json: String,
    pub raw_payload: String,
    pub mint: String,
    pub pool: String,
    pub timeframe: String,
    pub timeframe_seconds: i64,
    pub fetched_at: DateTime<Utc>,
    pub venue: Venue,
    pub dex_id: String,
    pub pool_created_at: Option<String>,
    pub candles: Vec<SourceCandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintRecord {
    pub mint: String,
    pub status: String,
    pub venue: String,
    pub created_at_chain: Option<String>,
}

/// Open a research store and engage research mode for this process.
pub struct ResearchStore {
    conn: Connection,
    chain: String,
}

impl ResearchStore {
    pub fn open_path(path: &Path, chain: crate::chains::ChainId) -> Result<Self, String> {
        crate::global::engage_research_mode();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
        }
        let mut conn = Connection::open(path).map_err(|error| error.to_string())?;
        configure_connection(&conn, configure::RESEARCH_DB).map_err(|error| error.to_string())?;
        let tx = conn.write_tx().map_err(|error| error.to_string())?;
        create_schema(
            &tx,
            &SchemaContext {
                chains: vec![chain],
            },
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Self {
            conn,
            chain: chain.as_str().to_owned(),
        })
    }

    pub fn ingest_candle_page(&mut self, page: &CandlePage) -> Result<IngestOutcome, String> {
        let chain = self.chain.clone();
        let tx = self.conn.write_tx().map_err(|error| error.to_string())?;
        let outcome = ingest_candle_page_tx(&tx, page, &chain)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(outcome)
    }

    pub fn ingest_raw_dataset(
        &mut self,
        source: &str,
        params_json: &str,
        raw_payload: &str,
        retrieved_at: DateTime<Utc>,
    ) -> Result<IngestOutcome, String> {
        let tx = self.conn.write_tx().map_err(|error| error.to_string())?;
        let outcome = insert_dataset(&tx, source, params_json, raw_payload, retrieved_at)?;
        record_health_ok(&tx, source, retrieved_at)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(outcome)
    }

    pub fn note_gap(
        &mut self,
        mint: &str,
        timeframe: Option<&str>,
        start_ts: Option<i64>,
        end_ts: Option<i64>,
        reason: &str,
        source: &str,
        noted_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let chain = self.chain.clone();
        let tx = self.conn.write_tx().map_err(|error| error.to_string())?;
        insert_gap(
            &tx, &chain, mint, timeframe, start_ts, end_ts, reason, source, noted_at,
        )?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Record a Pump.fun bonding-curve mint even when no candle source has it.
    /// The mint stays. Missing candles are a gap, not a profitable flat line.
    pub fn observe_bonding_curve_mint(
        &mut self,
        mint: &str,
        creator: Option<&str>,
        created_at_chain: Option<&str>,
        source: &str,
        seen_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let chain = self.chain.clone();
        let tx = self.conn.write_tx().map_err(|error| error.to_string())?;
        upsert_mint(
            &tx,
            MintUpsert {
                mint,
                seen_at,
                source,
                created_at_chain,
                creator,
                symbol: None,
                name: None,
                venue: Venue::BondingCurve,
                dex_id: Some("pump-fun"),
                status: None,
            },
            &chain,
        )?;
        insert_gap(
            &tx,
            &chain,
            mint,
            Some("1m"),
            None,
            None,
            "no_candles",
            source,
            seen_at,
        )?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn mark_lifecycle(
        &mut self,
        mint: &str,
        status: &str,
        source: &str,
        at: DateTime<Utc>,
    ) -> Result<(), String> {
        let chain = self.chain.clone();
        let tx = self.conn.write_tx().map_err(|error| error.to_string())?;
        upsert_mint(
            &tx,
            MintUpsert {
                mint,
                seen_at: at,
                source,
                created_at_chain: None,
                creator: None,
                symbol: None,
                name: None,
                venue: Venue::Unknown,
                dex_id: None,
                status: Some(status),
            },
            &chain,
        )?;
        if status == "untradeable" || status == "abandoned" || status == "rugged" {
            insert_gap(&tx, &chain, mint, None, None, None, status, source, at)?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn mint(&self, mint: &str) -> Result<Option<MintRecord>, String> {
        let chain = self.chain.as_str();
        self.conn
            .query_row(
                "SELECT mint, status, venue, created_at_chain FROM mints
                 WHERE chain_id = ?1 AND mint = ?2",
                params![chain, mint],
                |row| {
                    Ok(MintRecord {
                        mint: row.get(0)?,
                        status: row.get(1)?,
                        venue: row.get(2)?,
                        created_at_chain: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn dataset_count(&self) -> Result<i64, String> {
        self.conn
            .query_row("SELECT COUNT(*) FROM datasets", [], |row| row.get(0))
            .map_err(|error| error.to_string())
    }

    pub fn candle_count(&self) -> Result<i64, String> {
        self.conn
            .query_row("SELECT COUNT(*) FROM candles", [], |row| row.get(0))
            .map_err(|error| error.to_string())
    }

    pub fn candle_timestamps(&self, mint: &str, timeframe: &str) -> Result<Vec<i64>, String> {
        let chain = self.chain.as_str();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT ts FROM candles
                 WHERE chain_id = ?1 AND mint = ?2 AND timeframe = ?3
                 ORDER BY ts",
            )
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map(params![chain, mint, timeframe], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Mints that have an active status, a candle in the window, and no
    /// unfilled blocking gap. Missing data does not qualify.
    pub fn tradeable_mints(
        &self,
        timeframe: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<String>, String> {
        let chain = self.chain.as_str();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.mint FROM mints m
                 WHERE m.chain_id = ?1 AND m.status = 'active'
                   AND EXISTS (
                       SELECT 1 FROM candles c
                       WHERE c.chain_id = m.chain_id AND c.mint = m.mint
                         AND c.timeframe = ?2 AND c.ts >= ?3 AND c.ts <= ?4
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM gaps g
                       WHERE g.chain_id = m.chain_id AND g.mint = m.mint
                         AND g.filled = 0
                         AND g.reason IN (
                             'no_candles', 'hole', 'unindexed', 'untradeable',
                             'abandoned', 'rugged', 'source_error', 'no_market_snapshot'
                         )
                         AND (g.timeframe IS NULL OR g.timeframe = ?2)
                         AND (
                             (g.start_ts IS NULL AND g.end_ts IS NULL)
                             OR (g.start_ts < ?4 AND g.end_ts > ?3)
                         )
                   )
                 ORDER BY m.mint",
            )
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map(params![chain, timeframe, from_ts, to_ts], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn unfilled_gap_reasons(&self, mint: &str) -> Result<Vec<String>, String> {
        let chain = self.chain.as_str();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT reason FROM gaps
                 WHERE chain_id = ?1 AND mint = ?2 AND filled = 0
                 ORDER BY reason",
            )
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map(params![chain, mint], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn record_run(
        &mut self,
        source: &str,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        ok: bool,
        error: Option<&str>,
        requests: i64,
        retries: i64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO ingest_runs
                    (source, started_at, finished_at, ok, error, requests, retries)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    source,
                    started_at.to_rfc3339(),
                    finished_at.to_rfc3339(),
                    ok as i64,
                    error,
                    requests,
                    retries,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn record_health_failure(
        &mut self,
        source: &str,
        error: &str,
        at: DateTime<Utc>,
    ) -> Result<(), String> {
        let tx = self.conn.write_tx().map_err(|error| error.to_string())?;
        record_health_failure(&tx, source, error, at)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn health(&self, source: &str) -> Result<Option<HealthRow>, String> {
        self.conn
            .query_row(
                "SELECT consecutive_failures, requests, retries, last_error
                 FROM source_health WHERE source = ?1",
                params![source],
                |row| {
                    Ok(HealthRow {
                        consecutive_failures: row.get(0)?,
                        requests: row.get(1)?,
                        retries: row.get(2)?,
                        last_error: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn mint_count(&self) -> Result<i64, String> {
        self.conn
            .query_row("SELECT COUNT(*) FROM mints", [], |row| row.get(0))
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthRow {
    pub consecutive_failures: i64,
    pub requests: i64,
    pub retries: i64,
    pub last_error: Option<String>,
}

struct MintUpsert<'a> {
    mint: &'a str,
    seen_at: DateTime<Utc>,
    source: &'a str,
    created_at_chain: Option<&'a str>,
    creator: Option<&'a str>,
    symbol: Option<&'a str>,
    name: Option<&'a str>,
    venue: Venue,
    dex_id: Option<&'a str>,
    status: Option<&'a str>,
}

fn payload_hash(raw: &str) -> Hash {
    blake3::hash(raw.as_bytes())
}

fn insert_dataset(
    tx: &Connection,
    source: &str,
    params_json: &str,
    raw_payload: &str,
    retrieved_at: DateTime<Utc>,
) -> Result<IngestOutcome, String> {
    let content_hash = payload_hash(raw_payload).to_hex().to_string();
    let inserted = tx
        .execute(
            "INSERT INTO datasets (content_hash, source, retrieved_at, params_json, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(content_hash) DO NOTHING",
            params![
                content_hash,
                source,
                retrieved_at.to_rfc3339(),
                params_json,
                raw_payload,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(IngestOutcome {
        content_hash,
        inserted: inserted == 1,
    })
}

fn ingest_candle_page_tx(
    tx: &Connection,
    page: &CandlePage,
    chain: &str,
) -> Result<IngestOutcome, String> {
    let outcome = insert_dataset(
        tx,
        &page.source,
        &page.params_json,
        &page.raw_payload,
        page.fetched_at,
    )?;
    upsert_mint(
        tx,
        MintUpsert {
            mint: &page.mint,
            seen_at: page.fetched_at,
            source: &page.source,
            created_at_chain: page.pool_created_at.as_deref(),
            creator: None,
            symbol: None,
            name: None,
            venue: page.venue,
            dex_id: Some(page.dex_id.as_str()),
            status: None,
        },
        chain,
    )?;
    if !page.pool.is_empty() {
        tx.execute(
            "INSERT INTO pools
                (chain_id, pool, mint, dex_id, venue, created_at, first_seen_at, source, dataset_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(chain_id, pool) DO NOTHING",
            params![
                chain,
                page.pool,
                page.mint,
                page.dex_id,
                page.venue.as_str(),
                page.pool_created_at,
                page.fetched_at.to_rfc3339(),
                page.source,
                outcome.content_hash,
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    if outcome.inserted {
        for candle in &page.candles {
            tx.execute(
                "INSERT INTO candles
                    (dataset_hash, chain_id, mint, pool, timeframe, ts, open, high, low, close,
                     volume, source, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    outcome.content_hash,
                    chain,
                    page.mint,
                    page.pool,
                    page.timeframe,
                    candle.ts,
                    candle.open,
                    candle.high,
                    candle.low,
                    candle.close,
                    candle.volume,
                    page.source,
                    page.fetched_at.to_rfc3339(),
                ],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    if page.candles.is_empty() {
        insert_gap(
            tx,
            chain,
            &page.mint,
            Some(&page.timeframe),
            None,
            None,
            "no_candles",
            &page.source,
            page.fetched_at,
        )?;
    } else {
        tx.execute(
            "UPDATE gaps SET filled = 1
             WHERE chain_id = ?1 AND mint = ?2 AND timeframe = ?3
               AND reason = 'no_candles' AND filled = 0",
            params![chain, page.mint, page.timeframe],
        )
        .map_err(|error| error.to_string())?;
        recompute_holes(
            tx,
            chain,
            &page.mint,
            &page.timeframe,
            page.timeframe_seconds,
            &page.source,
            page.fetched_at,
        )?;
        promote_active_if_unknown(tx, chain, &page.mint, &page.source, page.fetched_at)?;
    }
    record_health_ok(tx, &page.source, page.fetched_at)?;
    Ok(outcome)
}

fn recompute_holes(
    tx: &Connection,
    chain: &str,
    mint: &str,
    timeframe: &str,
    step: i64,
    source: &str,
    noted_at: DateTime<Utc>,
) -> Result<(), String> {
    tx.execute(
        "DELETE FROM gaps
         WHERE chain_id = ?1 AND mint = ?2 AND timeframe = ?3 AND reason = 'hole'",
        params![chain, mint, timeframe],
    )
    .map_err(|error| error.to_string())?;
    if step <= 0 {
        return Ok(());
    }
    let mut stmt = tx
        .prepare(
            "SELECT DISTINCT ts FROM candles
             WHERE chain_id = ?1 AND mint = ?2 AND timeframe = ?3
             ORDER BY ts",
        )
        .map_err(|error| error.to_string())?;
    let stamps: Vec<i64> = stmt
        .query_map(params![chain, mint, timeframe], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(stmt);
    for pair in stamps.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > step + step / 2 {
            insert_gap(
                tx,
                chain,
                mint,
                Some(timeframe),
                Some(pair[0]),
                Some(pair[1]),
                "hole",
                source,
                noted_at,
            )?;
        }
    }
    Ok(())
}

fn promote_active_if_unknown(
    tx: &Connection,
    chain: &str,
    mint: &str,
    source: &str,
    at: DateTime<Utc>,
) -> Result<(), String> {
    tx.execute(
        "UPDATE mints SET status = 'active', status_at = ?3, status_source = ?4
         WHERE chain_id = ?1 AND mint = ?2 AND status = 'unknown'",
        params![chain, mint, at.to_rfc3339(), source],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn upsert_mint(tx: &Connection, row: MintUpsert<'_>, chain: &str) -> Result<(), String> {
    let seen = row.seen_at.to_rfc3339();
    let status = row.status.unwrap_or("unknown");
    tx.execute(
        "INSERT INTO mints
            (chain_id, mint, first_seen_at, first_source, created_at_chain, creator,
             symbol, name, venue, dex_id, status, status_at, status_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?3, ?4)
         ON CONFLICT(chain_id, mint) DO UPDATE SET
            created_at_chain = COALESCE(mints.created_at_chain, excluded.created_at_chain),
            creator = COALESCE(mints.creator, excluded.creator),
            symbol = COALESCE(mints.symbol, excluded.symbol),
            name = COALESCE(mints.name, excluded.name),
            venue = CASE
                WHEN mints.venue = 'unknown' THEN excluded.venue
                ELSE mints.venue
            END,
            dex_id = COALESCE(mints.dex_id, excluded.dex_id),
            status = CASE
                WHEN mints.status IN ('rugged', 'abandoned', 'untradeable') THEN mints.status
                WHEN excluded.status = 'unknown' THEN mints.status
                ELSE excluded.status
            END,
            status_at = CASE
                WHEN mints.status IN ('rugged', 'abandoned', 'untradeable') THEN mints.status_at
                WHEN excluded.status = 'unknown' THEN mints.status_at
                ELSE excluded.status_at
            END,
            status_source = CASE
                WHEN mints.status IN ('rugged', 'abandoned', 'untradeable') THEN mints.status_source
                WHEN excluded.status = 'unknown' THEN mints.status_source
                ELSE excluded.status_source
            END",
        params![
            chain,
            row.mint,
            seen,
            row.source,
            row.created_at_chain,
            row.creator,
            row.symbol,
            row.name,
            row.venue.as_str(),
            row.dex_id,
            status,
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn insert_gap(
    tx: &Connection,
    chain_id: &str,
    mint: &str,
    timeframe: Option<&str>,
    start_ts: Option<i64>,
    end_ts: Option<i64>,
    reason: &str,
    source: &str,
    noted_at: DateTime<Utc>,
) -> Result<(), String> {
    let exists: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM gaps
             WHERE chain_id = ?1 AND mint = ?2
               AND timeframe IS ?3 AND start_ts IS ?4 AND end_ts IS ?5
               AND reason = ?6 AND filled = 0",
            params![chain_id, mint, timeframe, start_ts, end_ts, reason],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if exists > 0 {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO gaps
            (chain_id, mint, timeframe, start_ts, end_ts, reason, source, noted_at, filled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
        params![
            chain_id,
            mint,
            timeframe,
            start_ts,
            end_ts,
            reason,
            source,
            noted_at.to_rfc3339(),
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn record_health_ok(tx: &Connection, source: &str, at: DateTime<Utc>) -> Result<(), String> {
    tx.execute(
        "INSERT INTO source_health
            (source, last_ok_at, consecutive_failures, requests, retries)
         VALUES (?1, ?2, 0, 1, 0)
         ON CONFLICT(source) DO UPDATE SET
            last_ok_at = excluded.last_ok_at,
            consecutive_failures = 0,
            requests = source_health.requests + 1",
        params![source, at.to_rfc3339()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn record_health_failure(
    tx: &Connection,
    source: &str,
    error: &str,
    at: DateTime<Utc>,
) -> Result<(), String> {
    tx.execute(
        "INSERT INTO source_health
            (source, last_error_at, last_error, consecutive_failures, requests, retries)
         VALUES (?1, ?2, ?3, 1, 1, 0)
         ON CONFLICT(source) DO UPDATE SET
            last_error_at = excluded.last_error_at,
            last_error = excluded.last_error,
            consecutive_failures = source_health.consecutive_failures + 1,
            requests = source_health.requests + 1",
        params![source, at.to_rfc3339(), error],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Liquidity and identity taken from one snapshot pair.
#[derive(Debug, Clone)]
pub struct SnapshotPair {
    pub mint: String,
    pub pool: String,
    pub dex_id: String,
    pub venue: Venue,
    pub symbol: Option<String>,
    pub name: Option<String>,
    pub created_at_chain: Option<String>,
    pub liquidity_usd: Option<f64>,
    pub reserve_quote: Option<f64>,
    /// Set only when the payload carried an event time. DexScreener snapshots
    /// usually do not, so this stays `None` and `observed_at` is the fetch time.
    pub source_event_time: Option<DateTime<Utc>>,
}

pub fn ingest_snapshot_pairs(
    store: &mut ResearchStore,
    source: &str,
    params_json: &str,
    raw_payload: &str,
    fetched_at: DateTime<Utc>,
    pairs: &[SnapshotPair],
    expected_mint: Option<&str>,
) -> Result<IngestOutcome, String> {
    let chain = store.chain.clone();
    let tx = store.conn.write_tx().map_err(|error| error.to_string())?;
    let outcome = insert_dataset(&tx, source, params_json, raw_payload, fetched_at)?;
    if outcome.inserted {
        for pair in pairs {
            upsert_mint(
                &tx,
                MintUpsert {
                    mint: &pair.mint,
                    seen_at: fetched_at,
                    source,
                    created_at_chain: pair.created_at_chain.as_deref(),
                    creator: None,
                    symbol: pair.symbol.as_deref(),
                    name: pair.name.as_deref(),
                    venue: pair.venue,
                    dex_id: Some(pair.dex_id.as_str()),
                    status: None,
                },
                &chain,
            )?;
            if !pair.pool.is_empty() {
                tx.execute(
                    "INSERT INTO pools
                        (chain_id, pool, mint, dex_id, venue, created_at, first_seen_at, source, dataset_hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(chain_id, pool) DO NOTHING",
                    params![
                        chain,
                        pair.pool,
                        pair.mint,
                        pair.dex_id,
                        pair.venue.as_str(),
                        pair.created_at_chain,
                        fetched_at.to_rfc3339(),
                        source,
                        outcome.content_hash,
                    ],
                )
                .map_err(|error| error.to_string())?;
            }
            let (observed_at, had_event) = match pair.source_event_time {
                Some(event) => (event, 1),
                None => (fetched_at, 0),
            };
            tx.execute(
                "INSERT INTO liquidity_observations
                    (dataset_hash, chain_id, mint, pool, observed_at, fetched_at,
                     source_had_event_time, liquidity_usd, reserve_quote, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    outcome.content_hash,
                    chain,
                    pair.mint,
                    pair.pool,
                    observed_at.to_rfc3339(),
                    fetched_at.to_rfc3339(),
                    had_event,
                    pair.liquidity_usd,
                    pair.reserve_quote,
                    source,
                ],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    if pairs.is_empty() {
        if let Some(mint) = expected_mint {
            upsert_mint(
                &tx,
                MintUpsert {
                    mint,
                    seen_at: fetched_at,
                    source,
                    created_at_chain: None,
                    creator: None,
                    symbol: None,
                    name: None,
                    venue: Venue::Unknown,
                    dex_id: None,
                    status: None,
                },
                &chain,
            )?;
            insert_gap(
                &tx,
                &chain,
                mint,
                None,
                None,
                None,
                "no_market_snapshot",
                source,
                fetched_at,
            )?;
        }
    }
    record_health_ok(&tx, source, fetched_at)?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(outcome)
}
