use std::{
    collections::HashMap,
    fmt::Write as _,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use madcap_fast::merge_track_pages;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, value::RawValue};
use tokio::sync::RwLock;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{info, warn};

const UPSTREAM: &str = "https://api.madcap.cc";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(120);
const EVENTS_LIST_REFRESH: Duration = Duration::from_secs(300);

#[derive(Clone)]
struct Snapshot {
    body: Bytes,
    body_br: Bytes,
    body_gz: Bytes,
    etag: String,
    fetched_at: Instant,
    upstream_ms: u64,
}

struct EventCache {
    snapshot: RwLock<Option<Snapshot>>,
    slug: String,
    /// Pre-rendered race-metric value lines (no HELP/TYPE); refreshed after
    /// each successful fetch and stitched into `/metrics`.
    race_metrics: RwLock<Option<String>>,
}

struct EventsListCache {
    snapshot: RwLock<Option<Snapshot>>,
}

#[derive(Default)]
struct Metrics {
    requests_event: AtomicU64,
    requests_events_list: AtomicU64,
    responses_304: AtomicU64,
    refreshes: AtomicU64,
    upstream_errors: AtomicU64,
    csv_exports: AtomicU64,
}

struct AppState {
    client: Client,
    events: RwLock<HashMap<String, Arc<EventCache>>>,
    events_list: Arc<EventsListCache>,
    cache_dir: Option<PathBuf>,
    metrics: Arc<Metrics>,
}

fn event_cache_path(dir: &FsPath, slug: &str) -> PathBuf {
    dir.join("events").join(format!("{slug}.json"))
}

fn events_list_cache_path(dir: &FsPath) -> PathBuf {
    dir.join("events_list.json")
}

fn persist_bytes(path: &FsPath, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Serialize)]
struct Combined<'a> {
    slug: &'a str,
    fetched_at_unix: u64,
    upstream_ms: u64,
    info: &'a RawValue,
    participants: &'a RawValue,
    geo: &'a RawValue,
    journals: &'a RawValue,
    tracks: &'a RawValue,
}

async fn fetch_raw(
    client: &Client,
    method: reqwest::Method,
    url: &str,
    body: Option<&str>,
) -> Result<Box<RawValue>> {
    let mut req = client.request(method, url);
    if let Some(b) = body {
        req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(b.to_owned());
    }
    let res = req
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = res.status();
    let text = res
        .text()
        .await
        .with_context(|| format!("reading body of {url}"))?;
    if !status.is_success() {
        anyhow::bail!(
            "upstream {url} returned {status}: {}",
            &text[..text.len().min(200)]
        );
    }
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing json from {url}"))?;
    let inner = v.get("data").cloned().unwrap_or(v);
    let raw = RawValue::from_string(serde_json::to_string(&inner)?)?;
    Ok(raw)
}

async fn fetch_tracks_paginated(client: &Client, slug: &str) -> Result<Box<RawValue>> {
    const MAX_PAGES: usize = 30;
    let mut ts = String::from("now");
    let mut prev_ts_used: Option<String> = None;
    let mut pages: Vec<Value> = Vec::new();

    for _ in 0..MAX_PAGES {
        let url = format!("{UPSTREAM}/event/v1/tracks/{slug}?ts={ts}");
        let res = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .with_context(|| format!("reading body of {url}"))?;
        if !status.is_success() {
            anyhow::bail!(
                "upstream {url} returned {status}: {}",
                &text[..text.len().min(200)]
            );
        }
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing json from {url}"))?;
        let data = v.get("data").cloned().unwrap_or(v);

        let prev = data.get("previous_page_ts").and_then(|p| {
            p.as_f64()
                .map(|f| format!("{}", f as i64))
                .or_else(|| p.as_i64().map(|i| i.to_string()))
                .or_else(|| p.as_str().map(String::from))
        });
        pages.push(data);

        match prev {
            Some(p) if p != ts && prev_ts_used.as_deref() != Some(&p) => {
                prev_ts_used = Some(ts);
                ts = p;
            }
            _ => break,
        }
    }

    let payload = merge_track_pages(&pages);
    let raw = RawValue::from_string(serde_json::to_string(&payload)?)?;
    Ok(raw)
}

async fn fetch_combined(client: &Client, slug: &str) -> Result<Snapshot> {
    let t = Instant::now();
    let body = format!(r#"{{"event":"{}"}}"#, slug.replace('"', ""));

    let info_url = format!("{UPSTREAM}/event/v1/{slug}/info");
    let participants_url = format!("{UPSTREAM}/event/v3/participants/{slug}");
    let geo_url = format!("{UPSTREAM}/event/geo/v3");
    let journals_url = format!("{UPSTREAM}/event/journals");

    let (info, participants, tracks, geo, journals) = tokio::try_join!(
        fetch_raw(client, reqwest::Method::GET, &info_url, None),
        fetch_raw(client, reqwest::Method::GET, &participants_url, None),
        fetch_tracks_paginated(client, slug),
        fetch_raw(client, reqwest::Method::POST, &geo_url, Some(&body)),
        fetch_raw(client, reqwest::Method::POST, &journals_url, Some(&body)),
    )?;
    let upstream_ms = t.elapsed().as_millis() as u64;

    let combined = Combined {
        slug,
        fetched_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        upstream_ms,
        info: &info,
        participants: &participants,
        geo: &geo,
        journals: &journals,
        tracks: &tracks,
    };
    let bytes = serde_json::to_vec(&combined)?;

    snapshot_from_bytes(bytes, upstream_ms).await
}

async fn snapshot_from_bytes(bytes: Vec<u8>, upstream_ms: u64) -> Result<Snapshot> {
    let (br, gz) = tokio::task::spawn_blocking({
        let raw = bytes.clone();
        move || -> Result<(Vec<u8>, Vec<u8>)> {
            use std::io::Write;
            let mut br_out = Vec::with_capacity(raw.len() / 5);
            {
                let mut w = brotli::CompressorWriter::new(&mut br_out, 8192, 6, 22);
                w.write_all(&raw)?;
            }
            let mut gz_out = Vec::with_capacity(raw.len() / 4);
            {
                let mut w = flate2::write::GzEncoder::new(&mut gz_out, flate2::Compression::new(6));
                w.write_all(&raw)?;
                w.finish()?;
            }
            Ok((br_out, gz_out))
        }
    })
    .await??;

    let etag = format!("\"{:x}\"", fnv1a(&bytes));

    Ok(Snapshot {
        body: Bytes::from(bytes),
        body_br: Bytes::from(br),
        body_gz: Bytes::from(gz),
        etag,
        fetched_at: Instant::now(),
        upstream_ms,
    })
}

async fn fetch_events_list(client: &Client) -> Result<Snapshot> {
    let t = Instant::now();
    let url = format!("{UPSTREAM}/v1/events/list");
    let inner = fetch_raw(client, reqwest::Method::GET, &url, None).await?;
    let upstream_ms = t.elapsed().as_millis() as u64;

    // Unwrap the `{ "events": [...] }` envelope so the client just sees the array.
    let parsed: Value = serde_json::from_str(inner.get())?;
    let list = parsed.get("events").cloned().unwrap_or(parsed);

    let wrapper = serde_json::json!({
        "fetched_at_unix": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "upstream_ms": upstream_ms,
        "events": list,
    });
    let bytes = serde_json::to_vec(&wrapper)?;
    snapshot_from_bytes(bytes, upstream_ms).await
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

async fn ensure_cache(state: &Arc<AppState>, slug: &str) -> Arc<EventCache> {
    {
        let r = state.events.read().await;
        if let Some(c) = r.get(slug) {
            return c.clone();
        }
    }
    let mut w = state.events.write().await;
    if let Some(c) = w.get(slug) {
        return c.clone();
    }
    let cache = Arc::new(EventCache {
        snapshot: RwLock::new(None),
        slug: slug.to_string(),
        race_metrics: RwLock::new(None),
    });
    w.insert(slug.to_string(), cache.clone());
    spawn_refresher(
        state.client.clone(),
        cache.clone(),
        state.cache_dir.clone(),
        state.metrics.clone(),
    );
    cache
}

async fn restore_from_disk(state: &Arc<AppState>) {
    let Some(dir) = state.cache_dir.clone() else {
        return;
    };
    let events_dir = dir.join("events");
    if events_dir.is_dir() {
        let entries = match std::fs::read_dir(&events_dir) {
            Ok(r) => r,
            Err(e) => {
                warn!(dir = ?events_dir, error = %e, "read_dir failed");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if !slug_ok(&slug) {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = ?path, error = %e, "read failed");
                    continue;
                }
            };
            match snapshot_from_bytes(bytes, 0).await {
                Ok(snap) => {
                    let race_metrics = render_event_race_metrics(&slug, &snap.body);
                    let cache = Arc::new(EventCache {
                        snapshot: RwLock::new(Some(snap)),
                        slug: slug.clone(),
                        race_metrics: RwLock::new(race_metrics),
                    });
                    state.events.write().await.insert(slug.clone(), cache.clone());
                    spawn_refresher(
                        state.client.clone(),
                        cache,
                        state.cache_dir.clone(),
                        state.metrics.clone(),
                    );
                    info!(slug = %slug, "restored event cache from disk");
                }
                Err(e) => warn!(slug = %slug, error = %e, "snapshot reconstruction failed"),
            }
        }
    }
    let list_path = events_list_cache_path(&dir);
    if list_path.is_file() {
        match std::fs::read(&list_path) {
            Ok(bytes) => match snapshot_from_bytes(bytes, 0).await {
                Ok(snap) => {
                    *state.events_list.snapshot.write().await = Some(snap);
                    info!("restored events list cache from disk");
                }
                Err(e) => warn!(error = %e, "events list reconstruction failed"),
            },
            Err(e) => warn!(error = %e, "events list read failed"),
        }
    }
}

fn spawn_refresher(
    client: Client,
    cache: Arc<EventCache>,
    cache_dir: Option<PathBuf>,
    metrics: Arc<Metrics>,
) {
    tokio::spawn(async move {
        loop {
            match fetch_combined(&client, &cache.slug).await {
                Ok(snap) => {
                    info!(
                        slug = %cache.slug,
                        body = snap.body.len(),
                        body_br = snap.body_br.len(),
                        upstream_ms = snap.upstream_ms,
                        "refreshed"
                    );
                    if let Some(dir) = &cache_dir {
                        let path = event_cache_path(dir, &cache.slug);
                        if let Err(e) = persist_bytes(&path, &snap.body) {
                            warn!(slug = %cache.slug, error = %e, "persist failed");
                        }
                    }
                    metrics.refreshes.fetch_add(1, Ordering::Relaxed);
                    let rendered = render_event_race_metrics(&cache.slug, &snap.body);
                    *cache.race_metrics.write().await = rendered;
                    *cache.snapshot.write().await = Some(snap);
                }
                Err(e) => {
                    warn!(slug = %cache.slug, error = %e, "refresh failed");
                    metrics.upstream_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

fn spawn_events_list_refresher(
    client: Client,
    cache: Arc<EventsListCache>,
    cache_dir: Option<PathBuf>,
    metrics: Arc<Metrics>,
) {
    tokio::spawn(async move {
        loop {
            match fetch_events_list(&client).await {
                Ok(snap) => {
                    info!(
                        body = snap.body.len(),
                        body_br = snap.body_br.len(),
                        upstream_ms = snap.upstream_ms,
                        "events list refreshed"
                    );
                    if let Some(dir) = &cache_dir {
                        let path = events_list_cache_path(dir);
                        if let Err(e) = persist_bytes(&path, &snap.body) {
                            warn!(error = %e, "events list persist failed");
                        }
                    }
                    metrics.refreshes.fetch_add(1, Ordering::Relaxed);
                    *cache.snapshot.write().await = Some(snap);
                }
                Err(e) => {
                    warn!(error = %e, "events list refresh failed");
                    metrics.upstream_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            tokio::time::sleep(EVENTS_LIST_REFRESH).await;
        }
    });
}

async fn combined_handler(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.metrics.requests_event.fetch_add(1, Ordering::Relaxed);
    if !slug_ok(&slug) {
        return (StatusCode::BAD_REQUEST, "bad slug").into_response();
    }
    let cache = ensure_cache(&state, &slug).await;

    let deadline = Instant::now() + Duration::from_secs(30);
    let snap = loop {
        if let Some(s) = cache.snapshot.read().await.clone() {
            break s;
        }
        if Instant::now() > deadline {
            return (StatusCode::GATEWAY_TIMEOUT, "cold cache, upstream slow").into_response();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    serve_snapshot(
        snap,
        &headers,
        "public, max-age=15, stale-while-revalidate=60",
        Some(&state.metrics),
    )
}

async fn events_list_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    state
        .metrics
        .requests_events_list
        .fetch_add(1, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(20);
    let snap = loop {
        if let Some(s) = state.events_list.snapshot.read().await.clone() {
            break s;
        }
        if Instant::now() > deadline {
            return (StatusCode::GATEWAY_TIMEOUT, "cold cache, upstream slow").into_response();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    serve_snapshot(
        snap,
        &headers,
        "public, max-age=60, stale-while-revalidate=300",
        Some(&state.metrics),
    )
}

fn serve_snapshot(
    snap: Snapshot,
    headers: &HeaderMap,
    cache_control: &'static str,
    metrics: Option<&Arc<Metrics>>,
) -> Response {
    let stale = snap.fetched_at.elapsed() > STALE_AFTER;

    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if if_none_match == Some(snap.etag.as_str()) {
        if let Some(m) = metrics {
            m.responses_304.fetch_add(1, Ordering::Relaxed);
        }
        let mut h = HeaderMap::new();
        h.insert(header::ETAG, HeaderValue::from_str(&snap.etag).unwrap());
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }

    let accept_enc = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let wants_br = accept_enc.contains("br");
    let wants_gz = accept_enc.contains("gzip");

    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&snap.etag).unwrap());
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    h.insert(
        "x-cache-age-ms",
        HeaderValue::from_str(&snap.fetched_at.elapsed().as_millis().to_string()).unwrap(),
    );
    h.insert(
        "x-cache-stale",
        HeaderValue::from_str(if stale { "1" } else { "0" }).unwrap(),
    );
    h.insert(
        "x-upstream-ms",
        HeaderValue::from_str(&snap.upstream_ms.to_string()).unwrap(),
    );

    let body = if wants_br {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        snap.body_br
    } else if wants_gz {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        snap.body_gz
    } else {
        snap.body
    };

    (StatusCode::OK, h, body).into_response()
}

fn slug_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Upstream sends naive ISO strings ("YYYY-MM-DDTHH:MM:SS"); treat as UTC.
/// Returns unix seconds, or None on parse failure.
fn parse_iso_utc(s: &str) -> Option<i64> {
    let s = s.split('.').next()?;
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i32 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: u32 = t.next()?.parse().ok()?;
    let mm: u32 = t.next()?.parse().ok()?;
    let ss: u32 = t.next()?.parse().ok()?;
    let days = days_since_epoch(y, mo, day)?;
    Some(days * 86400 + hh as i64 * 3600 + mm as i64 * 60 + ss as i64)
}

// Howard Hinnant's days_from_civil; unix epoch is 1970-01-01 = 0.
fn days_since_epoch(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era as i64 * 146097 + doe as i64 - 719468)
}

fn now_unix_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn esc_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Parse a number prefix out of strings like "1200km", "350 / 700 km" → the first float.
fn parse_km_prefix(s: &str) -> Option<f64> {
    let mut n = String::new();
    let mut seen_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            n.push(c);
            seen_digit = true;
        } else if c == '.' && seen_digit {
            n.push(c);
        } else if seen_digit {
            break;
        }
    }
    n.parse().ok()
}

/// Parse the `/api/event/{slug}` cached body and emit per-rider + per-event
/// Prometheus value lines (no HELP/TYPE — those are emitted once in `/metrics`).
fn render_event_race_metrics(slug: &str, body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let info = v.get("info")?;
    let participants = v.get("participants")?.as_array()?;

    // Don't churn Prometheus series for races that are already over.
    if let Some(end_ts) = info
        .get("end_date")
        .and_then(|v| v.as_str())
        .and_then(parse_iso_utc)
    {
        if now_unix_s() > end_ts {
            return None;
        }
    }

    let slug_esc = esc_label(slug);
    let mut out = String::with_capacity(participants.len() * 260);

    let total_km = info
        .get("distance")
        .and_then(|d| d.as_str())
        .and_then(parse_km_prefix)
        .unwrap_or(0.0);
    let _ = writeln!(
        out,
        "madcap_event_total_km{{slug=\"{slug_esc}\"}} {total_km}"
    );
    let _ = writeln!(
        out,
        "madcap_event_participants{{slug=\"{slug_esc}\"}} {}",
        participants.len()
    );

    let mut active: u32 = 0;
    let mut sleeping_total: u32 = 0;
    let mut started: u32 = 0;
    let mut finished: u32 = 0;

    for p in participants {
        let bib = p.get("bib").and_then(|v| v.as_str()).unwrap_or("");
        let first = p.get("first_name").and_then(|v| v.as_str()).unwrap_or("");
        let last = p.get("last_name").and_then(|v| v.as_str()).unwrap_or("");
        let name_raw = format!("{first} {last}");
        let name = name_raw.trim();
        let category = p
            .get("attributes")
            .and_then(|a| a.get("category"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let status = p.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let sleeping = p
            .get("sleeping")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let labels = format!(
            "slug=\"{}\",bib=\"{}\",name=\"{}\",category=\"{}\"",
            slug_esc,
            esc_label(bib),
            esc_label(name),
            esc_label(category),
        );

        let has_ping = p.get("last_ping").is_some_and(|v| !v.is_null());
        if has_ping || status == "REGISTERED" || status == "ACTIVE" {
            active += 1;
        }
        if sleeping {
            sleeping_total += 1;
        }

        if let Some(d) = p.get("distance").and_then(|v| v.as_f64()) {
            let _ = writeln!(out, "madcap_rider_distance_km{{{labels}}} {d}");
            if d > 0.0 {
                started += 1;
            }
            if total_km > 0.0 && d >= total_km - 0.5 {
                finished += 1;
            }
        }
        if let Some(sp) = p.get("speed").and_then(|v| v.as_f64()) {
            let _ = writeln!(out, "madcap_rider_speed_kmh{{{labels}}} {sp}");
        }
        if let Some(r) = p.get("overall_rank").and_then(|v| v.as_f64()) {
            let _ = writeln!(out, "madcap_rider_overall_rank{{{labels}}} {r}");
        }
        if let Some(r) = p.get("rank").and_then(|v| v.as_f64()) {
            let _ = writeln!(out, "madcap_rider_category_rank{{{labels}}} {r}");
        }
        if let Some(b) = p.get("battery").and_then(|v| v.as_f64()) {
            let _ = writeln!(out, "madcap_rider_battery_pct{{{labels}}} {b}");
        }
        let _ = writeln!(
            out,
            "madcap_rider_sleeping{{{labels}}} {}",
            if sleeping { 1 } else { 0 }
        );
        if let Some(dtc) = p
            .get("distance_to_next_cp")
            .and_then(|v| v.get("distance"))
            .and_then(|v| v.as_f64())
        {
            let _ = writeln!(out, "madcap_rider_distance_to_next_cp_km{{{labels}}} {dtc}");
        }
    }

    let _ = writeln!(
        out,
        "madcap_event_active{{slug=\"{slug_esc}\"}} {active}"
    );
    let _ = writeln!(
        out,
        "madcap_event_sleeping{{slug=\"{slug_esc}\"}} {sleeping_total}"
    );
    let _ = writeln!(
        out,
        "madcap_event_started{{slug=\"{slug_esc}\"}} {started}"
    );
    let _ = writeln!(
        out,
        "madcap_event_finished{{slug=\"{slug_esc}\"}} {finished}"
    );

    Some(out)
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    let m = &state.metrics;
    let mut out = String::with_capacity(2048);
    let _ = writeln!(
        out,
        "# HELP madcap_fast_requests_total Total API requests handled\n\
         # TYPE madcap_fast_requests_total counter"
    );
    let _ = writeln!(
        out,
        "madcap_fast_requests_total{{path=\"event\"}} {}",
        m.requests_event.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "madcap_fast_requests_total{{path=\"events_list\"}} {}",
        m.requests_events_list.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "madcap_fast_requests_total{{path=\"csv\"}} {}",
        m.csv_exports.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP madcap_fast_responses_not_modified_total Total 304 responses\n\
         # TYPE madcap_fast_responses_not_modified_total counter\n\
         madcap_fast_responses_not_modified_total {}",
        m.responses_304.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP madcap_fast_upstream_refreshes_total Successful upstream refreshes\n\
         # TYPE madcap_fast_upstream_refreshes_total counter\n\
         madcap_fast_upstream_refreshes_total {}",
        m.refreshes.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP madcap_fast_upstream_errors_total Upstream refresh failures\n\
         # TYPE madcap_fast_upstream_errors_total counter\n\
         madcap_fast_upstream_errors_total {}",
        m.upstream_errors.load(Ordering::Relaxed)
    );

    let caches: Vec<(String, Arc<EventCache>)> = state
        .events
        .read()
        .await
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let _ = writeln!(
        out,
        "# HELP madcap_fast_cache_age_seconds Age of the cached snapshot per slug\n\
         # TYPE madcap_fast_cache_age_seconds gauge"
    );
    let _ = writeln!(
        out,
        "# HELP madcap_fast_cache_body_bytes Raw JSON body size per cached slug\n\
         # TYPE madcap_fast_cache_body_bytes gauge"
    );
    let _ = writeln!(
        out,
        "# HELP madcap_fast_upstream_last_ms Last upstream refresh duration per slug\n\
         # TYPE madcap_fast_upstream_last_ms gauge"
    );
    for (slug, cache) in &caches {
        if let Some(snap) = cache.snapshot.read().await.as_ref() {
            let age = snap.fetched_at.elapsed().as_secs_f64();
            let _ = writeln!(
                out,
                "madcap_fast_cache_age_seconds{{slug=\"{slug}\"}} {age}"
            );
            let _ = writeln!(
                out,
                "madcap_fast_cache_body_bytes{{slug=\"{slug}\"}} {}",
                snap.body.len()
            );
            let _ = writeln!(
                out,
                "madcap_fast_upstream_last_ms{{slug=\"{slug}\"}} {}",
                snap.upstream_ms
            );
        }
    }

    if let Some(snap) = state.events_list.snapshot.read().await.as_ref() {
        let _ = writeln!(
            out,
            "madcap_fast_events_list_cache_age_seconds {}",
            snap.fetched_at.elapsed().as_secs_f64()
        );
        let _ = writeln!(
            out,
            "madcap_fast_events_list_cache_body_bytes {}",
            snap.body.len()
        );
    }

    // Race metrics — emit HELP/TYPE once, then concatenate per-event lines.
    let _ = writeln!(out, "# HELP madcap_event_total_km Advertised total course distance in km (parsed from info.distance)");
    let _ = writeln!(out, "# TYPE madcap_event_total_km gauge");
    let _ = writeln!(out, "# HELP madcap_event_participants Participant count in the event");
    let _ = writeln!(out, "# TYPE madcap_event_participants gauge");
    let _ = writeln!(out, "# HELP madcap_event_active Riders with any ping or REGISTERED/ACTIVE status");
    let _ = writeln!(out, "# TYPE madcap_event_active gauge");
    let _ = writeln!(out, "# HELP madcap_event_sleeping Riders currently flagged sleeping");
    let _ = writeln!(out, "# TYPE madcap_event_sleeping gauge");
    let _ = writeln!(out, "# HELP madcap_event_started Riders with non-zero distance");
    let _ = writeln!(out, "# TYPE madcap_event_started gauge");
    let _ = writeln!(out, "# HELP madcap_event_finished Riders within 0.5 km of the course total");
    let _ = writeln!(out, "# TYPE madcap_event_finished gauge");
    let _ = writeln!(out, "# HELP madcap_rider_distance_km Distance covered by the rider in km");
    let _ = writeln!(out, "# TYPE madcap_rider_distance_km gauge");
    let _ = writeln!(out, "# HELP madcap_rider_speed_kmh Current reported speed in km/h");
    let _ = writeln!(out, "# TYPE madcap_rider_speed_kmh gauge");
    let _ = writeln!(out, "# HELP madcap_rider_overall_rank Overall rank (1 = leader)");
    let _ = writeln!(out, "# TYPE madcap_rider_overall_rank gauge");
    let _ = writeln!(out, "# HELP madcap_rider_category_rank Rank within the rider's category");
    let _ = writeln!(out, "# TYPE madcap_rider_category_rank gauge");
    let _ = writeln!(out, "# HELP madcap_rider_battery_pct Tracker battery percentage");
    let _ = writeln!(out, "# TYPE madcap_rider_battery_pct gauge");
    let _ = writeln!(out, "# HELP madcap_rider_sleeping 1 if the rider is currently flagged sleeping, else 0");
    let _ = writeln!(out, "# TYPE madcap_rider_sleeping gauge");
    let _ = writeln!(out, "# HELP madcap_rider_distance_to_next_cp_km Straight-line distance to the next checkpoint");
    let _ = writeln!(out, "# TYPE madcap_rider_distance_to_next_cp_km gauge");

    for (_slug, cache) in &caches {
        if let Some(text) = cache.race_metrics.read().await.as_ref() {
            out.push_str(text);
        }
    }

    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    (StatusCode::OK, h, out).into_response()
}

fn csv_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => {
            if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.clone()
            }
        }
        _ => v.to_string(),
    }
}
fn csv_num(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}
fn csv_bool(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

async fn event_csv_handler(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
) -> Response {
    state.metrics.csv_exports.fetch_add(1, Ordering::Relaxed);
    if !slug_ok(&slug) {
        return (StatusCode::BAD_REQUEST, "bad slug").into_response();
    }
    let cache = ensure_cache(&state, &slug).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let snap = loop {
        if let Some(s) = cache.snapshot.read().await.clone() {
            break s;
        }
        if Instant::now() > deadline {
            return (StatusCode::GATEWAY_TIMEOUT, "cold cache, upstream slow").into_response();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let v: Value = match serde_json::from_slice(&snap.body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "parse").into_response(),
    };
    let participants = v
        .get("participants")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let mut csv = String::with_capacity(participants.len() * 180);
    csv.push_str("overall_rank,category,category_rank,bib,first_name,last_name,nickname,country,distance_km,speed_kmh,distance_to_next_cp_km,battery_pct,last_ping,status,sleeping\n");
    for p in participants {
        let get = |k: &str| p.get(k).cloned().unwrap_or(Value::Null);
        let category = p
            .get("attributes")
            .and_then(|a| a.get("category"))
            .cloned()
            .unwrap_or(Value::Null);
        let dtc = p
            .get("distance_to_next_cp")
            .and_then(|d| d.get("distance"))
            .cloned()
            .unwrap_or(Value::Null);
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            csv_num(&get("overall_rank")),
            csv_str(&category),
            csv_num(&get("rank")),
            csv_str(&get("bib")),
            csv_str(&get("first_name")),
            csv_str(&get("last_name")),
            csv_str(&get("nickname")),
            csv_str(&get("country")),
            csv_num(&get("distance")),
            csv_num(&get("speed")),
            csv_num(&dtc),
            csv_num(&get("battery")),
            csv_str(&get("last_ping")),
            csv_str(&get("status")),
            csv_bool(&get("sleeping")),
        );
    }
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{slug}-leaderboard.csv\""
        ))
        .unwrap(),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=15"),
    );
    (StatusCode::OK, h, csv).into_response()
}

async fn events_csv_handler(State(state): State<Arc<AppState>>) -> Response {
    state.metrics.csv_exports.fetch_add(1, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(20);
    let snap = loop {
        if let Some(s) = state.events_list.snapshot.read().await.clone() {
            break s;
        }
        if Instant::now() > deadline {
            return (StatusCode::GATEWAY_TIMEOUT, "cold cache, upstream slow").into_response();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let v: Value = serde_json::from_slice(&snap.body).unwrap_or(Value::Null);
    let events = v
        .get("events")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    let mut csv = String::with_capacity(events.len() * 120);
    csv.push_str("slug,name,start_place,end_place,start_date,end_date,distance,surface,participants\n");
    for e in events {
        let get = |k: &str| e.get(k).cloned().unwrap_or(Value::Null);
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{}",
            csv_str(&get("slug")),
            csv_str(&get("name")),
            csv_str(&get("start_place_name")),
            csv_str(&get("end_place_name")),
            csv_str(&get("start_date")),
            csv_str(&get("end_date")),
            csv_str(&get("distance")),
            csv_str(&get("surface")),
            csv_num(&get("participants")),
        );
    }
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"events.csv\""),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    (StatusCode::OK, h, csv).into_response()
}

async fn index() -> impl IntoResponse {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    (StatusCode::OK, h, include_str!("index.html"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "madcap_fast=info,tower_http=info".into()),
        )
        .init();

    let client = Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .timeout(Duration::from_secs(45))
        .user_agent("madcap-fast/0.1")
        .build()?;

    let cache_dir = std::env::var("MADCAP_CACHE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(dir) = &cache_dir {
        info!(dir = ?dir, "cache persistence enabled");
    }

    let events_list = Arc::new(EventsListCache {
        snapshot: RwLock::new(None),
    });
    let metrics = Arc::new(Metrics::default());

    let state = Arc::new(AppState {
        client: client.clone(),
        events: RwLock::new(HashMap::new()),
        events_list: events_list.clone(),
        cache_dir: cache_dir.clone(),
        metrics: metrics.clone(),
    });

    restore_from_disk(&state).await;

    spawn_events_list_refresher(
        client.clone(),
        events_list.clone(),
        cache_dir.clone(),
        metrics.clone(),
    );

    // Comma-separated list of slugs to pre-warm on boot. Each becomes its own
    // background refresher; duplicates in the cache dir are a no-op.
    let warm_slugs = std::env::var("MADCAP_WARM_SLUG")
        .unwrap_or_else(|_| "desertus-bikus-26".into());
    for raw in warm_slugs.split(',') {
        let slug = raw.trim();
        if slug.is_empty() || !slug_ok(slug) {
            continue;
        }
        let slug = slug.to_string();
        let s = state.clone();
        tokio::spawn(async move {
            let _ = ensure_cache(&s, &slug).await;
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/event/{slug}", get(index))
        .route("/api/event/{slug}", get(combined_handler))
        .route("/api/event/{slug}/csv", get(event_csv_handler))
        .route("/api/events", get(events_list_handler))
        .route("/api/events/csv", get(events_csv_handler))
        .route("/metrics", get(metrics_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9004);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "madcap_fast listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutting down");
}
