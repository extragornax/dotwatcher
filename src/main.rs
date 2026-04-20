use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
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
}

struct EventsListCache {
    snapshot: RwLock<Option<Snapshot>>,
}

struct AppState {
    client: Client,
    events: RwLock<HashMap<String, Arc<EventCache>>>,
    events_list: Arc<EventsListCache>,
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
    });
    w.insert(slug.to_string(), cache.clone());
    spawn_refresher(state.client.clone(), cache.clone());
    cache
}

fn spawn_refresher(client: Client, cache: Arc<EventCache>) {
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
                    *cache.snapshot.write().await = Some(snap);
                }
                Err(e) => warn!(slug = %cache.slug, error = %e, "refresh failed"),
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

fn spawn_events_list_refresher(client: Client, cache: Arc<EventsListCache>) {
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
                    *cache.snapshot.write().await = Some(snap);
                }
                Err(e) => warn!(error = %e, "events list refresh failed"),
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
    )
}

async fn events_list_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
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
    )
}

fn serve_snapshot(snap: Snapshot, headers: &HeaderMap, cache_control: &'static str) -> Response {
    let stale = snap.fetched_at.elapsed() > STALE_AFTER;

    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if if_none_match == Some(snap.etag.as_str()) {
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

    let events_list = Arc::new(EventsListCache {
        snapshot: RwLock::new(None),
    });
    spawn_events_list_refresher(client.clone(), events_list.clone());

    let state = Arc::new(AppState {
        client,
        events: RwLock::new(HashMap::new()),
        events_list,
    });

    let warm_slug =
        std::env::var("MADCAP_WARM_SLUG").unwrap_or_else(|_| "desertus-bikus-26".into());
    if !warm_slug.is_empty() {
        let s = state.clone();
        tokio::spawn(async move {
            let _ = ensure_cache(&s, &warm_slug).await;
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/event/{slug}", get(index))
        .route("/api/event/{slug}", get(combined_handler))
        .route("/api/events", get(events_list_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
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
