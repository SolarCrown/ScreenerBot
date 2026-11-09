// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Paced collection. The GeckoTerminal ceiling is the figure that host
//! publishes for its keyless API (about 10 calls per minute). A 429 is
//! retryable. This module does not raise the live client's 30/min default;
//! research calls wait on their own pace first.

use std::future::Future;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::Mutex;

use super::store::ResearchStore;

/// GeckoTerminal's own API docs describe the public API as about 10 calls
/// per minute (`https://api.geckoterminal.com/docs/index.html`). CoinGecko's
/// keyless page says the shared pool can sit nearer 5 and moves with load.
/// Research uses one call every 6 seconds so a burst cannot exceed 10/min.
pub const GECKOTERMINAL_GAP: Duration = Duration::from_secs(6);

/// DexScreener documents 300/min on pair and token routes and 60/min on
/// profile, boost and order routes. Research snapshots wait 1 second, which
/// stays inside the stricter published bucket.
pub const DEXSCREENER_GAP: Duration = Duration::from_secs(1);

/// Minimum time between acquisitions.
pub struct Pace {
    gap: Duration,
    next_ok: Mutex<Option<tokio::time::Instant>>,
}

impl Pace {
    pub fn every(gap: Duration) -> Self {
        Self {
            gap,
            next_ok: Mutex::new(None),
        }
    }

    pub fn geckoterminal() -> Self {
        Self::every(GECKOTERMINAL_GAP)
    }

    pub fn dexscreener() -> Self {
        Self::every(DEXSCREENER_GAP)
    }

    pub async fn acquire(&self) {
        let wait = {
            let mut next_ok = self.next_ok.lock().await;
            let now = tokio::time::Instant::now();
            let start = (*next_ok).unwrap_or(now).max(now);
            *next_ok = Some(start + self.gap);
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// A fetch the collector may try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectError {
    pub message: String,
    pub retryable: bool,
}

/// How a research collector paces, retries and records source health.
pub struct Collector {
    pub pace: Pace,
    pub max_attempts: u32,
    pub source: String,
}

impl Collector {
    pub fn geckoterminal() -> Self {
        Self {
            pace: Pace::geckoterminal(),
            max_attempts: 3,
            source: "geckoterminal".to_owned(),
        }
    }

    /// Fetch one payload, waiting on the pace, retrying retryable failures,
    /// and writing health plus an ingest-run row. The store engages research
    /// mode when it was opened.
    pub async fn pull<F, Fut>(
        &self,
        store: &mut ResearchStore,
        mut fetch: F,
    ) -> Result<String, CollectError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<String, CollectError>>,
    {
        let started = Utc::now();
        let mut attempts = 0_i64;
        let mut retries = 0_i64;
        let mut last_error = String::new();
        while attempts < i64::from(self.max_attempts) {
            self.pace.acquire().await;
            attempts += 1;
            match fetch().await {
                Ok(body) => {
                    let finished = Utc::now();
                    store
                        .record_run(
                            &self.source,
                            started,
                            finished,
                            true,
                            None,
                            attempts,
                            retries,
                        )
                        .map_err(|error| CollectError {
                            message: error,
                            retryable: false,
                        })?;
                    return Ok(body);
                }
                Err(error) => {
                    last_error = error.message.clone();
                    store
                        .record_health_failure(&self.source, &error.message, Utc::now())
                        .map_err(|message| CollectError {
                            message,
                            retryable: false,
                        })?;
                    if error.retryable && attempts < i64::from(self.max_attempts) {
                        retries += 1;
                        continue;
                    }
                    store
                        .record_run(
                            &self.source,
                            started,
                            Utc::now(),
                            false,
                            Some(&last_error),
                            attempts,
                            retries,
                        )
                        .ok();
                    return Err(error);
                }
            }
        }
        Err(CollectError {
            message: last_error,
            retryable: false,
        })
    }
}

/// Run `collector.pull` and hand a successful body to `accept`.
pub async fn collect_pages<F, Fut, A>(
    store: &mut ResearchStore,
    collector: &Collector,
    fetch: F,
    accept: A,
) -> Result<(), CollectError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String, CollectError>>,
    A: FnOnce(&mut ResearchStore, &str) -> Result<(), String>,
{
    let body = collector.pull(store, fetch).await?;
    accept(store, &body).map_err(|message| CollectError {
        message,
        retryable: false,
    })
}
