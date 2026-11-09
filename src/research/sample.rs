// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! A capped memecoin sample from the free sources.
//!
//! Pool lists come from GeckoTerminal. `pool_created_at` is stored as the
//! mint's chain creation time, which the backtester uses as born time.
//! OHLCV uses `currency=token` so prices are in the quote token. A quote
//! reserve is stored only when that quote is the caller's native mint, and
//! the observation time is the fetch time. It is not a historical reserve.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::collect::{CollectError, Collector};
use super::parse::{gecko_ohlcv_page, rugcheck_report, GeckoOhlcvPage};
use super::store::{ingest_snapshot_pairs, ResearchStore, SnapshotPair};
use super::venue::venue_from_dex_id;

const GECKO_ACCEPT: &str = "application/json;version=20230203";
const LIST_PATHS: &[&str] = &[
    "new_pools?page=1",
    "new_pools?page=2",
    "trending_pools?page=1",
    "pools?page=1",
    "dexes/pump-fun/pools?page=1",
    "dexes/pump-fun/pools?page=4",
    "dexes/pumpswap/pools?page=1",
    "dexes/pumpswap/pools?page=4",
    "dexes/raydium/pools?page=1",
    "dexes/meteora/pools?page=1",
    "dexes/orca/pools?page=1",
    "dexes/meteora-damm-v2/pools?page=1",
];

/// How many pools, and how many of each OHLCV timeframe, to keep.
#[derive(Debug, Clone)]
pub struct SampleQuotas {
    pub max_pools: usize,
    pub one_minute: usize,
    pub five_minute: usize,
    pub fifteen_minute: usize,
    pub one_hour: usize,
    /// Drained pools (USD reserve under $1) reserved inside the 1-minute quota
    /// so a dead token is not crowded out by new launches.
    pub drained_one_minute: usize,
}

/// What the collector is allowed to ask for. Network and mint addresses are
/// arguments so this module does not name a chain.
#[derive(Debug, Clone)]
pub struct SampleRequest {
    pub network: String,
    pub native_quote_mint: String,
    pub skip_base_mints: Vec<String>,
    pub quotas: SampleQuotas,
}

/// One pool row from a GeckoTerminal list payload.
#[derive(Debug, Clone)]
struct ListedPool {
    pub pool: String,
    pub mint: String,
    pub quote_mint: String,
    pub dex_id: String,
    pub name: String,
    pub pool_created_at: Option<String>,
    pub reserve_in_usd: Option<f64>,
    pub quote_price_usd: Option<f64>,
    page: usize,
}

#[derive(Debug, Clone)]
struct PickedPool {
    pool: ListedPool,
    timeframe: &'static str,
    seconds: i64,
    path: &'static str,
    aggregate: u32,
}

struct ListPage {
    fetched_at: DateTime<Utc>,
    params: String,
    raw: String,
}

/// Half of `reserve_in_usd`, converted at the quote token's USD price.
/// GeckoTerminal publishes the whole-pool USD reserve, not each side, and
/// not a history. The result is an estimate at the time that payload was fetched.
fn quote_reserve_sol(reserve_usd: Option<f64>, quote_price_usd: Option<f64>) -> Option<f64> {
    let usd = reserve_usd.filter(|value| value.is_finite() && *value > 0.0)?;
    let price = quote_price_usd.filter(|value| value.is_finite() && *value > 0.0)?;
    let quote = usd / 2.0 / price;
    (quote.is_finite() && quote > 0.0).then_some(quote)
}

fn gecko_pool_list(raw: &str, network: &str) -> Result<Vec<ListedPool>, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let rows = value
        .pointer("/data")
        .and_then(|item| item.as_array())
        .ok_or_else(|| "geckoterminal pool list has no data array".to_owned())?;
    let mut pools = Vec::new();
    for row in rows {
        let Some(pool) = text_at(row, "/attributes/address") else {
            continue;
        };
        let Some(mint) = relation_address(row, "base_token", network) else {
            continue;
        };
        let Some(quote_mint) = relation_address(row, "quote_token", network) else {
            continue;
        };
        let dex_id = relation_address(row, "dex", network).unwrap_or_default();
        pools.push(ListedPool {
            pool,
            mint,
            quote_mint,
            dex_id,
            name: text_at(row, "/attributes/name").unwrap_or_default(),
            pool_created_at: text_at(row, "/attributes/pool_created_at"),
            reserve_in_usd: number_at(row, "/attributes/reserve_in_usd"),
            quote_price_usd: number_at(row, "/attributes/quote_token_price_usd"),
            page: 0,
        });
    }
    Ok(pools)
}

fn pick_sample(
    mut pools: Vec<ListedPool>,
    native_quote: &str,
    skip_bases: &[String],
    quotas: &SampleQuotas,
) -> Vec<PickedPool> {
    let mut seen = std::collections::BTreeSet::new();
    pools.retain(|pool| {
        dex_family(&pool.dex_id).is_some()
            && pool.quote_mint == native_quote
            && pool.mint != native_quote
            && !skip_bases.iter().any(|skip| skip == &pool.mint)
            && !pool.pool.is_empty()
            && !pool.mint.is_empty()
            && seen.insert(pool.pool.clone())
    });
    let (mut drained, mut live): (Vec<_>, Vec<_>) = pools.into_iter().partition(is_drained);
    sort_newest(&mut drained);
    sort_newest(&mut live);
    let mut picked = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    let mut cursor = 0;
    take_slots(
        &mut picked,
        &mut used,
        &mut cursor,
        &mut drained,
        &mut live,
        "1m",
        60,
        "minute",
        1,
        quotas.one_minute,
        quotas.drained_one_minute,
    );
    let rest = [
        ("5m", 300, "minute", 5, quotas.five_minute),
        ("15m", 900, "minute", 15, quotas.fifteen_minute),
        ("1h", 3600, "hour", 1, quotas.one_hour),
    ];
    for (label, seconds, path, aggregate, slots) in rest {
        let drained_slots = slots / 5;
        take_slots(
            &mut picked,
            &mut used,
            &mut cursor,
            &mut drained,
            &mut live,
            label,
            seconds,
            path,
            aggregate,
            slots,
            drained_slots,
        );
    }
    picked.truncate(quotas.max_pools);
    picked
}

/// Fetch the sample into `store`. One GeckoTerminal call is paced at the
/// collector's gap. A failed candle or Rugcheck call is recorded and skipped.
pub async fn collect_memecoin_sample(
    store: &mut ResearchStore,
    client: &reqwest::Client,
    request: &SampleRequest,
) -> Result<usize, String> {
    let gecko = Collector::geckoterminal();
    let mut pages = Vec::new();
    let mut listed = Vec::new();
    for path in LIST_PATHS {
        let url = format!(
            "https://api.geckoterminal.com/api/v2/networks/{}/{path}",
            request.network
        );
        let params = format!(r#"{{"list":"{path}"}}"#);
        let fetched_at = Utc::now();
        let body = match gecko
            .pull(store, || get_text(client, &url, GECKO_ACCEPT))
            .await
        {
            Ok(body) => body,
            Err(error) => {
                eprintln!("list {path} skipped: {}", error.message);
                continue;
            }
        };
        let mut pools = match gecko_pool_list(&body, &request.network) {
            Ok(pools) => pools,
            Err(error) => {
                eprintln!("list {path} parse skipped: {error}");
                continue;
            }
        };
        let page_index = pages.len();
        for pool in &mut pools {
            pool.page = page_index;
        }
        listed.extend(pools);
        pages.push(ListPage {
            fetched_at,
            params,
            raw: body,
        });
    }
    if pages.is_empty() {
        return Err("every pool list failed".to_owned());
    }
    let picked = pick_sample(
        listed,
        &request.native_quote_mint,
        &request.skip_base_mints,
        &request.quotas,
    );
    if picked.is_empty() {
        return Err("the pool lists produced an empty sample".to_owned());
    }
    for (index, page) in pages.iter().enumerate() {
        let pairs: Vec<SnapshotPair> = picked
            .iter()
            .filter(|pick| pick.pool.page == index)
            .map(|pick| snapshot_from_list(&pick.pool))
            .collect();
        if pairs.is_empty() {
            continue;
        }
        ingest_snapshot_pairs(
            store,
            "geckoterminal",
            &page.params,
            &page.raw,
            page.fetched_at,
            &pairs,
            None,
        )?;
    }
    let dex = Collector::dexscreener();
    let mut addresses: Vec<&str> = picked.iter().map(|pick| pick.pool.pool.as_str()).collect();
    addresses.sort_unstable();
    addresses.dedup();
    for chunk in addresses.chunks(30) {
        let joined = chunk.join(",");
        let url = format!(
            "https://api.dexscreener.com/latest/dex/pairs/{}/{joined}",
            request.network
        );
        let params = format!(r#"{{"pairs":{}}}"#, chunk.len());
        match dex
            .pull(store, || get_text(client, &url, "application/json"))
            .await
        {
            Ok(body) => {
                let fetched_at = Utc::now();
                match super::parse::dexscreener_snapshot(&body, &request.network, fetched_at) {
                    Ok((_, mut pairs)) => {
                        let wanted: std::collections::BTreeSet<&str> =
                            chunk.iter().copied().collect();
                        pairs.retain(|pair| wanted.contains(pair.pool.as_str()));
                        if !pairs.is_empty() {
                            ingest_snapshot_pairs(
                                store,
                                "dexscreener",
                                &params,
                                &body,
                                fetched_at,
                                &pairs,
                                None,
                            )?;
                        }
                    }
                    Err(error) => eprintln!("dexscreener parse skipped: {error}"),
                }
            }
            Err(error) => eprintln!("dexscreener batch skipped: {}", error.message),
        }
    }
    let mut rug_left: Vec<String> = picked.iter().map(|pick| pick.pool.mint.clone()).collect();
    rug_left.sort();
    rug_left.dedup();
    for pick in &picked {
        let url = format!(
            "https://api.geckoterminal.com/api/v2/networks/{}/pools/{}/ohlcv/{}?aggregate={}&limit=1000&currency=token&token=base",
            request.network, pick.pool.pool, pick.path, pick.aggregate
        );
        let params = format!(
            r#"{{"pool":"{}","timeframe":"{}"}}"#,
            pick.pool.pool, pick.timeframe
        );
        let fetched_at = Utc::now();
        if store.has_candles(&pick.pool.mint, pick.timeframe)? {
            if let Some(mint) = rug_left.pop() {
                check_rug(store, client, &mint).await?;
            }
            continue;
        }
        match gecko
            .pull(store, || get_text(client, &url, GECKO_ACCEPT))
            .await
        {
            Ok(body) => {
                match gecko_ohlcv_page(GeckoOhlcvPage {
                    mint: pick.pool.mint.clone(),
                    pool: pick.pool.pool.clone(),
                    timeframe: pick.timeframe.to_owned(),
                    timeframe_seconds: pick.seconds,
                    dex_id: pick.pool.dex_id.clone(),
                    pool_created_at: pick.pool.pool_created_at.clone(),
                    fetched_at,
                    raw_payload: body,
                    params_json: params,
                }) {
                    Ok(page) => {
                        if let Err(error) = store.ingest_candle_page(&page) {
                            eprintln!("ohlcv store {} {}: {error}", pick.pool.pool, pick.timeframe);
                            store.note_gap(
                                &pick.pool.mint,
                                Some(pick.timeframe),
                                None,
                                None,
                                "fetch_failed",
                                "geckoterminal",
                                fetched_at,
                            )?;
                        }
                    }
                    Err(error) => {
                        eprintln!("ohlcv parse {} {}: {error}", pick.pool.pool, pick.timeframe);
                        store.note_gap(
                            &pick.pool.mint,
                            Some(pick.timeframe),
                            None,
                            None,
                            "fetch_failed",
                            "geckoterminal",
                            fetched_at,
                        )?;
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "ohlcv {} {}: {}",
                    pick.pool.pool, pick.timeframe, error.message
                );
                store.note_gap(
                    &pick.pool.mint,
                    Some(pick.timeframe),
                    None,
                    None,
                    "fetch_failed",
                    "geckoterminal",
                    fetched_at,
                )?;
            }
        }
        if let Some(mint) = rug_left.pop() {
            check_rug(store, client, &mint).await?;
        }
    }
    while let Some(mint) = rug_left.pop() {
        check_rug(store, client, &mint).await?;
    }
    Ok(picked.len())
}

async fn check_rug(
    store: &mut ResearchStore,
    client: &reqwest::Client,
    mint: &str,
) -> Result<(), String> {
    let url = format!("https://api.rugcheck.xyz/v1/tokens/{mint}/report");
    let fetched_at = Utc::now();
    match get_text(client, &url, "application/json").await {
        Ok(body) => {
            store.ingest_raw_dataset(
                "rugcheck",
                &format!(r#"{{"mint":"{mint}"}}"#),
                &body,
                fetched_at,
            )?;
            match rugcheck_report(&body) {
                Ok(view) if view.rugged => {
                    store.mark_lifecycle(&view.mint, "rugged", "rugcheck", fetched_at)?;
                }
                Ok(_) => {}
                Err(error) => eprintln!("rugcheck parse {mint}: {error}"),
            }
        }
        Err(error) => eprintln!("rugcheck {mint}: {}", error.message),
    }
    Ok(())
}

fn snapshot_from_list(pool: &ListedPool) -> SnapshotPair {
    let symbol = pool
        .name
        .split(" / ")
        .next()
        .filter(|symbol| !symbol.is_empty())
        .map(str::to_owned);
    SnapshotPair {
        mint: pool.mint.clone(),
        pool: pool.pool.clone(),
        venue: venue_from_dex_id(&pool.dex_id),
        dex_id: pool.dex_id.clone(),
        symbol,
        name: Some(pool.name.clone()).filter(|name| !name.is_empty()),
        created_at_chain: pool.pool_created_at.clone(),
        liquidity_usd: pool.reserve_in_usd.filter(|value| value.is_finite()),
        reserve_quote: quote_reserve_sol(pool.reserve_in_usd, pool.quote_price_usd),
        source_event_time: None,
    }
}

async fn get_text(
    client: &reqwest::Client,
    url: &str,
    accept: &str,
) -> Result<String, CollectError> {
    let response = match client.get(url).header("accept", accept).send().await {
        Ok(response) => response,
        Err(error) => {
            return Err(CollectError {
                message: error.to_string(),
                retryable: true,
            });
        }
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            return Err(CollectError {
                message: error.to_string(),
                retryable: true,
            });
        }
    };
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(CollectError {
            message: format!("http {status}"),
            retryable: true,
        });
    }
    if !status.is_success() {
        return Err(CollectError {
            message: format!(
                "http {status}: {}",
                body.chars().take(180).collect::<String>()
            ),
            retryable: false,
        });
    }
    Ok(body)
}

fn take_slots(
    picked: &mut Vec<PickedPool>,
    used: &mut std::collections::BTreeSet<String>,
    cursor: &mut usize,
    drained: &mut Vec<ListedPool>,
    live: &mut Vec<ListedPool>,
    timeframe: &'static str,
    seconds: i64,
    path: &'static str,
    aggregate: u32,
    slots: usize,
    drained_slots: usize,
) {
    if slots == 0 {
        return;
    }
    let mut taken = 0;
    while taken < slots && taken < drained_slots {
        let Some(pool) = pop_unused(drained, used) else {
            break;
        };
        push_pick(picked, used, pool, timeframe, seconds, path, aggregate);
        taken += 1;
    }
    while taken < slots {
        let Some(pool) = pop_diverse(live, used, cursor) else {
            break;
        };
        push_pick(picked, used, pool, timeframe, seconds, path, aggregate);
        taken += 1;
    }
    while taken < slots {
        let Some(pool) = pop_unused(drained, used) else {
            break;
        };
        push_pick(picked, used, pool, timeframe, seconds, path, aggregate);
        taken += 1;
    }
}

fn push_pick(
    picked: &mut Vec<PickedPool>,
    used: &mut std::collections::BTreeSet<String>,
    pool: ListedPool,
    timeframe: &'static str,
    seconds: i64,
    path: &'static str,
    aggregate: u32,
) {
    used.insert(pool.pool.clone());
    picked.push(PickedPool {
        pool,
        timeframe,
        seconds,
        path,
        aggregate,
    });
}

fn pop_unused(
    pools: &mut Vec<ListedPool>,
    used: &std::collections::BTreeSet<String>,
) -> Option<ListedPool> {
    while let Some(pool) = pools.pop() {
        if !used.contains(&pool.pool) {
            return Some(pool);
        }
    }
    None
}

/// Newest-first within each DEX family, rotating families so one venue
/// cannot fill a timeframe by itself.
fn pop_diverse(
    pools: &mut Vec<ListedPool>,
    used: &std::collections::BTreeSet<String>,
    cursor: &mut usize,
) -> Option<ListedPool> {
    const FAMILIES: [&str; 5] = ["pump-fun", "pumpswap", "raydium", "meteora", "orca"];
    for offset in 0..FAMILIES.len() {
        let family = FAMILIES[(*cursor + offset) % FAMILIES.len()];
        if let Some(index) = pools.iter().rposition(|pool| {
            !used.contains(&pool.pool) && dex_family(&pool.dex_id) == Some(family)
        }) {
            *cursor = (*cursor + offset + 1) % FAMILIES.len();
            return Some(pools.remove(index));
        }
    }
    pop_unused(pools, used)
}

fn sort_newest(pools: &mut [ListedPool]) {
    // Oldest first, so `pop` yields the newest pool.
    pools.sort_by(|left, right| {
        left.pool_created_at
            .cmp(&right.pool_created_at)
            .then_with(|| left.pool.cmp(&right.pool))
    });
}

fn is_drained(pool: &ListedPool) -> bool {
    pool.reserve_in_usd
        .is_some_and(|value| value.is_finite() && value < 1.0)
}

fn dex_family(dex_id: &str) -> Option<&'static str> {
    let id = dex_id.trim().to_ascii_lowercase();
    if matches!(
        id.as_str(),
        "pump-fun" | "pumpfun" | "pump_fun" | "pump.fun"
    ) {
        return Some("pump-fun");
    }
    if id.contains("pumpswap") || id.contains("pump-swap") || id.contains("pump_amm") {
        return Some("pumpswap");
    }
    if id.contains("pump") && (id.contains("swap") || id.contains("amm")) {
        return Some("pumpswap");
    }
    if id.contains("raydium") {
        return Some("raydium");
    }
    if id.contains("meteora") {
        return Some("meteora");
    }
    if id.contains("orca") || id.contains("whirlpool") {
        return Some("orca");
    }
    None
}

fn relation_address(row: &Value, name: &str, network: &str) -> Option<String> {
    let id = row
        .pointer(&format!("/relationships/{name}/data/id"))
        .and_then(|item| item.as_str())
        .filter(|id| !id.is_empty())?;
    let prefix = format!("{network}_");
    Some(id.strip_prefix(&prefix).unwrap_or(id).to_owned())
}

fn text_at(row: &Value, pointer: &str) -> Option<String> {
    row.pointer(pointer)
        .and_then(|item| item.as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn number_at(row: &Value, pointer: &str) -> Option<f64> {
    let item = row.pointer(pointer)?;
    item.as_f64()
        .or_else(|| item.as_i64().map(|value| value as f64))
        .or_else(|| item.as_str().and_then(|text| text.parse().ok()))
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(
        dex: &str,
        address: &str,
        mint: &str,
        quote: &str,
        usd: f64,
        created: &str,
    ) -> ListedPool {
        ListedPool {
            pool: address.to_owned(),
            mint: mint.to_owned(),
            quote_mint: quote.to_owned(),
            dex_id: dex.to_owned(),
            name: format!("Coin / {quote}"),
            pool_created_at: Some(created.to_owned()),
            reserve_in_usd: Some(usd),
            quote_price_usd: Some(100.0),
            page: 0,
        }
    }

    #[test]
    fn a_pool_list_keeps_creation_time_and_the_base_mint() {
        let raw = r#"{
            "data": [{
                "attributes": {
                    "address": "PoolA",
                    "name": "CAT / SOL",
                    "pool_created_at": "2026-11-01T00:00:00Z",
                    "reserve_in_usd": "2000.5",
                    "quote_token_price_usd": "100"
                },
                "relationships": {
                    "base_token": {"data": {"id": "solana_MintA"}},
                    "quote_token": {"data": {"id": "solana_SoNative"}},
                    "dex": {"data": {"id": "pump-fun"}}
                }
            }]
        }"#;
        let pools = gecko_pool_list(raw, "solana").unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].mint, "MintA");
        assert_eq!(pools[0].quote_mint, "SoNative");
        assert_eq!(pools[0].dex_id, "pump-fun");
        assert_eq!(
            pools[0].pool_created_at.as_deref(),
            Some("2026-11-01T00:00:00Z")
        );
        let reserve = quote_reserve_sol(pools[0].reserve_in_usd, pools[0].quote_price_usd).unwrap();
        assert!((reserve - 10.0025).abs() < 1e-9);
    }

    #[test]
    fn the_sample_keeps_a_drained_pool_and_drops_a_major_pair() {
        let native = "SoNative";
        let quotas = SampleQuotas {
            max_pools: 4,
            one_minute: 2,
            five_minute: 1,
            fifteen_minute: 1,
            one_hour: 1,
            drained_one_minute: 1,
        };
        let picked = pick_sample(
            vec![
                pool(
                    "pump-fun",
                    "p1",
                    "MintNew",
                    native,
                    500.0,
                    "2026-11-09T00:00:00Z",
                ),
                pool(
                    "pumpswap",
                    "p2",
                    "MintDead",
                    native,
                    0.0,
                    "2026-11-08T00:00:00Z",
                ),
                pool(
                    "raydium",
                    "p3",
                    "MintRay",
                    native,
                    8000.0,
                    "2026-11-07T00:00:00Z",
                ),
                pool(
                    "orca",
                    "p4",
                    native,
                    "UsdcMint",
                    1_000_000.0,
                    "2024-01-01T00:00:00Z",
                ),
                pool(
                    "meteora",
                    "p5",
                    "MintMet",
                    native,
                    3000.0,
                    "2026-11-06T00:00:00Z",
                ),
                pool(
                    "unknown-dex",
                    "p6",
                    "MintNo",
                    native,
                    10.0,
                    "2026-11-05T00:00:00Z",
                ),
            ],
            native,
            &["UsdcMint".to_owned()],
            &quotas,
        );
        let ids: Vec<_> = picked.iter().map(|pick| pick.pool.pool.as_str()).collect();
        assert!(
            ids.contains(&"p2"),
            "a drained pool stays in the sample: {ids:?}"
        );
        assert!(
            !ids.contains(&"p4"),
            "the native-base pair is not a memecoin"
        );
        assert!(!ids.contains(&"p6"));
        assert!(picked
            .iter()
            .any(|pick| pick.pool.pool == "p2" && pick.timeframe == "1m"));
        assert_eq!(picked.len(), 4);
    }
}
