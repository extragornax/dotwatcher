# madcap_fast

A fast, self-hosted viewer for public [madcap.cc](https://app.madcap.cc) event pages
(live bike-race tracking). Drop-in replacement for one event page that loads in
**~0.7 s** instead of the original's **~31 s**.

Not affiliated with madcap.cc. Reads only their public read-only API.

---

## Why

`app.madcap.cc/event/desertus-bikus-26` ships a 13 MB JavaScript bundle and
then serially fires five XHRs to `api.madcap.cc`. The heavy one —
`/event/v1/tracks/<slug>?ts=now`, ~5.9 MB of GPS points — has a **6–9 second
origin TTFB** and isn't cached at the CDN (`cf-cache-status: DYNAMIC`). Every
visitor pays that cost.

End-to-end measurement with Playwright against a warm upstream:

| metric                       | `app.madcap.cc` (original) | `madcap_fast` (this project) |
| ---------------------------- | -------------------------: | ---------------------------: |
| data usable                  |                 18 s       |                **110 ms**    |
| participants rendered        |                 31 s       |                **0.7 s**     |
| `/api/event/:slug` TTFB      |                  —         |                **0.4 ms**    |
| 304 revalidation             |                  —         |                **0.3 ms**    |
| payload (brotli)             |      2.0 MB (tracks only)  | 2.1 MB (**all five** calls)  |

The speedup comes entirely from not re-hitting the slow origin on every visit.
Nothing exotic.

---

## How it works

```
             ┌─────────────────────┐
             │  api.madcap.cc      │  (5 endpoints, ~5 s aggregate)
             └──────────▲──────────┘
                        │ refresh every 30 s
                        │
┌──────────────────┐    │    ┌────────────────────────────────┐
│ Rust axum server │────┴───▶│  per-slug EventCache<Snapshot> │
│  (this binary)   │         │   body:      raw JSON (6.7 MB) │
│                  │         │   body_gz:   gzip -6 (2.2 MB)  │
│ warms default    │         │   body_br:   brotli -6 (2.1 MB)│
│ event on boot    │         │   etag:      fnv1a(body)       │
└────────▲─────────┘         └────────────────────────────────┘
         │                             ▲
         │  GET /api/event/:slug       │ one buffer memcpy, no
         │  GET /event/:slug (HTML)    │ per-request compression
         │                             │
         └── browser ──────────────────┘
             - leaderboard (list tab)
             - Leaflet map (map tab)
```

### Server (`src/main.rs`, ~290 lines)

- **Parallel fan-out.** `tokio::try_join!` fires five upstream HTTP/2 calls
  concurrently (`info`, `participants`, `tracks`, `geo`, `journals`) and unwraps
  each `{"status":"ok","data":…}` envelope.
- **One combined JSON.** The server merges everything into one object with
  `serde_json::value::RawValue` so inner payloads are never re-parsed —
  strings flow straight through.
- **Pre-compression.** Each successful refresh pre-computes brotli (q=6) and
  gzip (level 6). Responses are served as raw `bytes::Bytes` — zero work per
  request, no tower-http `CompressionLayer` in the hot path.
- **Background refresh.** The first request for a slug spawns a dedicated
  `tokio::task` that refreshes every 30 s. Subsequent requests read the cached
  `Snapshot` through a `RwLock`.
- **Cache-warming.** On boot, the server warms `MADCAP_WARM_SLUG`
  (default `desertus-bikus-26`) so the first visitor already hits a warm cache.
- **Revalidation.** `ETag` + `If-None-Match` → 304 in <1 ms with zero body.
  `Cache-Control: public, max-age=15, stale-while-revalidate=60` so any
  downstream proxy can further amplify the cache.
- **Content negotiation.** `Accept-Encoding: br` preferred, falls back to
  `gzip`, then identity. Handler reads `Accept-Encoding` once, picks the right
  pre-compressed buffer.
- **Introspection headers.** `x-upstream-ms`, `x-cache-age-ms`, `x-cache-stale`
  so the frontend can display live cache freshness.

### Frontend (`src/index.html`, embedded via `include_str!`)

Single static page, no build step. Fetches `/api/event/:slug` once, parses it,
renders two tabs:

- **List tab.** Sidebar leaderboard (rank, name, bib, distance, speed, last
  ping, sleep state) + detail pane with per-rider CP splits, status stats, and
  last-known position.
- **Map tab.** Leaflet on CARTO dark tiles (no API key), lazy-initialized on
  first open:
  - route polylines parsed from `geo.routes[].geojson`
  - checkpoint badges from `geo.cps`
  - one marker per rider at the last point of their `tracks.tracks[i].track`
  - marker color by `overall_rank` (red ≤10, amber ≤50, green 51+), dimmed if
    sleeping, dashed if no ping in 15 min
  - popup with stats + "open details →" that jumps to the list tab
  - client-side rider search that flies to and opens the popup

Full page refresh every 30 s (same cadence as the server's upstream refresh).

---

## Running

### Cargo (local dev)

```bash
cargo run --release
# then open http://127.0.0.1:8080/event/desertus-bikus-26
```

### Docker Compose

```bash
docker compose up -d --build
# then open http://127.0.0.1:8080/event/desertus-bikus-26
```

Override the host port or warm slug inline:

```bash
HOST_PORT=9000 MADCAP_WARM_SLUG=some-other-event docker compose up -d
```

The image is a multi-stage build (`rust:1-bookworm` → `debian:bookworm-slim`),
runs as a non-root user under `tini`, and drops all capabilities. Healthcheck
hits `/` every 30 s. First build takes ~90 s (deps cached layer), incremental
rebuilds only recompile `src/`.

### Config (env vars, both modes)

| var                | default               | meaning                                      |
| ------------------ | --------------------- | -------------------------------------------- |
| `PORT`             | `8080`                | bind port                                    |
| `MADCAP_WARM_SLUG` | `desertus-bikus-26`   | slug to pre-warm on boot; set empty to skip  |
| `RUST_LOG`         | `madcap_fast=info`    | standard `tracing_subscriber` filter         |
| `HOST_PORT`        | `8080`                | compose only: host-side port mapping         |

The server exposes:

- `GET  /`                       → HTML page — event picker (grid of all events)
- `GET  /event/:slug`            → HTML page — single-event view (list + map tabs)
- `GET  /api/events`             → cached events list (refreshed every 5 min)
- `GET  /api/event/:slug`        → combined JSON, pre-compressed, ETag-aware (30 s refresh)

---

## Upstream API reference

Reverse-engineered from the production SPA's network trace — all endpoints are
public and return `{ "status": "ok", "data": <payload> }`:

| endpoint                                            | method | ~size (raw) | notes                                  |
| --------------------------------------------------- | -----: | ----------: | -------------------------------------- |
| `/v1/events/list`                                   | GET    |   220 KB    | all public events (used by `/api/events`) |
| `/event/v1/<slug>/info`                             | GET    |   1.5 KB    | event metadata                         |
| `/event/v3/participants/<slug>`                     | GET    |   260 KB    | leaderboard + rider stats              |
| `/event/v1/tracks/<slug>?ts=now`                    | GET    |   5.9 MB    | **slow** (6–9 s TTFB) — all GPS tracks |
| `/event/geo/v3` (body `{"event":"<slug>"}`)         | POST   |   470 KB    | checkpoints + route GeoJSON            |
| `/event/journals` (body `{"event":"<slug>"}`)       | POST   |    80 KB    | sleep/event journal entries            |

Track points are packed arrays: `[t_offset, lat, lng, elev_m, speed_kmh, …]`.

---

## Limitations / non-goals

- No write endpoints; no auth. This is a read-through cache.
- Cache is in-memory per process. Restart = cold start (warm-on-boot mitigates).
- One cached slug per request path; not a general multi-tenant cache.
- Map uses CARTO tiles, not madcap.cc's custom Mapbox style.
- No WebSocket / SSE live updates yet — polling at 30 s both on server and
  client. The upstream doesn't expose a realtime channel.
