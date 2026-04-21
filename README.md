# madcap_fast

A fast, self-hosted viewer for public [madcap.cc](https://app.madcap.cc) event pages
(live bike-race tracking). Drop-in replacement for one event page that loads in
**~0.7 s** instead of the original's **~31 s**.

Not affiliated with madcap.cc. Reads only their public read-only API.

See [`CHANGELOG.md`](CHANGELOG.md) for per-iteration history, one dated
section per commit. This README describes the app as it stands on the
current tip of `master`.

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

### Server (`src/main.rs` + `src/lib.rs`)

- **Parallel fan-out.** `tokio::try_join!` fires five upstream calls
  concurrently (`info`, `participants`, `tracks`, `geo`, `journals`) and unwraps
  each `{"status":"ok","data":…}` envelope.
- **Paginated tracks.** The upstream `tracks` endpoint serves one 24-hour window
  per call and links older pages via `previous_page_ts`. `fetch_tracks_paginated`
  walks those cursors up to 30 pages, merges per participant, sorts by timestamp
  and dedups page-boundary overlaps. The rest of the pipeline sees a single
  `tracks` object containing the full event history.
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
- **Optional disk persistence.** When `MADCAP_CACHE_DIR` is set, every refresh
  atomically writes the raw combined JSON to
  `<dir>/events/<slug>.json` (and the events list to
  `<dir>/events_list.json`). On startup the server walks the directory and
  rebuilds `Snapshot`s (brotli + gzip + ETag are regenerated from the bytes),
  so the first request after a restart is already warm. The 30 s refresher
  then overwrites with fresh upstream data. Docker compose enables this by
  default with a named `cache` volume.
- **Benchmark.** `cargo bench --bench merge_tracks` exercises
  `merge_track_pages` (pure CPU work from pagination) at three realistic sizes.

### Frontend (`src/index.html`, embedded via `include_str!`)

Single static page, no build step. Fetches `/api/event/:slug` once, parses it,
renders a multi-tab UI with rich state restored from the URL.

#### Home page (`/`)
Grid of event cards with filters (**Live / Upcoming / Past / All**), a search
box, live rider counts, and banner images. Cards link to `/event/:slug`.

#### Event page (`/event/:slug`)

- **Three tabs** in the header — **List / Map / Feed** — with `ℹ` (event info
  & sponsors drawer) and `🔔` (browser notifications) controls.
- **URL state** — `?tab=`, `?p=<participant>`, `?cat=`, `?fav=1` are written via
  `history.replaceState` so any moment can be shared as a link.
- **UTC-aware time display.** Upstream timestamps are naive ISO strings but
  actually UTC; the app normalizes them and localizes at render time.

##### List tab

- Leaderboard sidebar with rank, name, bib, distance, speed, last-ping, sleep
  state, and **cactus delta** (time / km vs the virtual pacer — green ahead,
  red behind).
- Category filter pills (e.g. `F` / `H` / `M` on desertus-bikus-26) —
  selecting a category re-ranks the board by per-category rank.
- Search by name / nickname / bib.
- **★ favourites** per event (localStorage, per-slug). Favourites pin to the
  top of the board. `★ only` toggle collapses the board to favourites.
- **Detail pane** for the selected rider:
  - Headline stats (distance, speed, distance-to-next-CP, ranks, last ping,
    battery, status).
  - Inline-SVG **elevation** and **speed** sparklines across the whole track.
  - **Rest & movement timeline** — orange blocks on a green bar marking
    stretches where `speed ≤ 1.5 km/h` for ≥ 20 min, with totals and longest
    block.
  - **Segments** table — CP-to-CP split times, the rider's rank for each leg
    (across everyone who completed it) and the gap to the fastest.
  - Full **Checkpoints** table with per-CP cumulative rank + arrival time.
  - Last known position + one-click Google Maps link.

##### Map tab

Leaflet, lazy-initialized on first open:

- **6 themes** (dropdown): Dark (Carto, default), Light, Voyager, OSM,
  Satellite (Esri), Topo (OpenTopoMap). Persisted in localStorage.
- **3 marker label styles** (dropdown): coloured dots, bib pills, or name
  pills. Persisted in localStorage.
- Route polylines from `geo.routes[].geojson`; the upstream's "Cactus" route
  is recoloured from pure black to a pale sand that actually reads on dark
  tiles. Checkpoint badges from `geo.cps`.
- One marker per rider at the last point of their `tracks[].track`. Colour by
  `overall_rank` (red ≤ 10, amber ≤ 50, green 51+), dimmed if sleeping, dashed
  if no ping in 15 min, gold ring if favourited.
- **Trace polylines** for the selected rider (bright) and each favourite
  (dim), coloured by rank. `traces` toggle hides them all; `★ only` toggle
  also filters the marker set.
- **Virtual 🌵 Cactus pacer** — a marker interpolated along the Cactus route
  at `(now − start) / (end − start)` × total distance, updating every 60 s.
  Popup shows % and km.
- **Time scrubber** (bottom-centre) — range slider over `[event start, now]`,
  play / pause, 5 playback speeds (1 s = 1 min / 5 min / **20 min (default)** /
  1 h / 6 h), `live` button. Scrubbing drives markers, traces, and the cactus
  via binary search on each track; rAF-throttled.
- Client-side search that flies to and opens a rider's popup.

##### Feed tab

Global reverse-chronological timeline of journal entries. `PICTURE` entries
render with a 140×100 thumbnail (click for the full image); `SLEEP` entries
show rider + location. Filter pills: **All / Photos / Sleeps / ★ favourites**.
Clicking a rider's name opens their detail in the List tab.

##### Event info drawer (`ℹ`)

Slides in from the right with the event description, route / distance /
surface, start and end dates, rankings, website + Instagram links, emergency
/ organiser / technical phone numbers (as `tel:` links), and a 2-column grid
of sponsor logos. Closes with ✕, backdrop click, or Escape.

##### Browser notifications (`🔔`, tab-open only)

Permission-gated. On each 30 s refresh, diffs the new snapshot against the
previous one and fires desktop notifications **only for favourites**:

- CP crossed (distinguishes `reached CPn` from `finished at <name>`).
- Caught by the cactus (ahead → behind).
- Passed the cactus (behind → ahead).
- Rank gain of ≥ 10 places in a single refresh.
- Battery dropped from > 20 % to ≤ 20 %.
- Long stop of ≥ 45 min, fired once while the rider's latest fix is still
  inside the rest block.
- New `PICTURE` journal entry, with the photo as the notification icon.

Each trigger uses a unique `tag` so the browser replaces rather than stacks
same-event messages. First load seeds the snapshot without firing.

Full page data refresh every 30 s (same cadence as the server's upstream
refresh).

---

## Running

### Cargo (local dev)

```bash
cargo run --release
# then open http://127.0.0.1:9004/event/desertus-bikus-26
```

### Docker Compose

```bash
docker compose up -d --build
# then open http://127.0.0.1:9004/event/desertus-bikus-26
```

Override the host port or warm slug inline:

```bash
HOST_PORT=9000 MADCAP_WARM_SLUG=some-other-event docker compose up -d
```

The image is a multi-stage build (`rust:1-bookworm` → `debian:bookworm-slim`),
runs as a non-root user under `tini`, and drops all capabilities. Healthcheck
hits `/` every 30 s. First build takes ~90 s (deps cached layer), incremental
rebuilds only recompile `src/`.

### Prometheus + Grafana (optional)

`/metrics` is always on. **Prometheus ships with the main compose stack**
so metrics collection starts automatically whenever `madcap_fast` is up —
you always have 90 days of operational history even without Grafana.
**Grafana lives in a separate compose** so you can bring it up only when
you actually want a dashboard, without disturbing metric collection.

```bash
docker compose up -d                                   # madcap_fast + prometheus
docker compose -f docker-compose.monitoring.yml up -d  # + grafana
# Grafana    http://127.0.0.1:9007   (admin / admin — change it)
# Prometheus http://127.0.0.1:9090
```

Files:
- `docker-compose.yml` — runs **Prometheus** alongside `madcap_fast` on a
  shared `madcap_fast_monitoring` Docker network so Prometheus scrapes
  `madcap_fast:9004` by service name. Named volume `prometheus-data`,
  90-day retention. Prometheus's `:9090` is bound to `127.0.0.1` only —
  **not reachable from the public IP**; drop the `ports:` block entirely
  to make it container-only.
- `docker-compose.monitoring.yml` — Grafana only; joins the same
  `madcap_fast_monitoring` network (as `external`) and reaches Prometheus
  by service name (`http://prometheus:9090`). Named volume `grafana-data`
  for its own state.
- `monitoring/prometheus.yml` — scrape config targeting `madcap_fast:9004`.
- `monitoring/grafana/provisioning/datasources/prometheus.yml` — auto-wires
  Prometheus as Grafana's default data source on first boot (UID
  `prometheus`).
- `monitoring/grafana/provisioning/dashboards/dashboards.yml` + `monitoring/grafana/dashboards/madcap_fast-race.json` — auto-loaded multi-race dashboard. An **Event** dropdown lists every cached & live slug via `label_values(madcap_event_total_km, slug)`, plus a **Top N riders** slider. Panels: 6 stat cards (participants / active / started / sleeping / finished / course total), rank-over-time worms, distance-over-time, current leaderboard table (rank + bib + name + distance + speed + battery with color-gradient), low-battery watchlist, and the operational refresh-latency + cache-age trends. Folder in Grafana: **madcap_fast**.

Env overrides: `PROMETHEUS_PORT`, `GRAFANA_PORT`, `GRAFANA_USER`,
`GRAFANA_PASSWORD`, `GF_SERVER_ROOT_URL`, `GF_SERVER_DOMAIN`.

For public read-only access, use Grafana's built-in **dashboard / panel
sharing** (external sharing toggle) — it bypasses the newer
Kubernetes-style dashboard API that would otherwise 403 anonymous GETs,
and doesn't require any extra env vars.

#### Serving Grafana behind a public domain

When Grafana is fronted by a reverse proxy (Caddy, nginx, …) at something
like `https://grafana.extragornax.fr`, set two env vars so share / OAuth
redirect URLs match the public hostname instead of `http://localhost:3000`:

```bash
export GF_SERVER_ROOT_URL=https://grafana.extragornax.fr/
export GF_SERVER_DOMAIN=grafana.extragornax.fr
docker compose -f docker-compose.monitoring.yml up -d --force-recreate grafana
```

Caddy snippet (auto-HTTPS, proxies to the host-bound port 9007):

```caddy
grafana.extragornax.fr {
    reverse_proxy 127.0.0.1:9007
}
```

Grafana reads `Host` / `X-Forwarded-*` correctly out of the box, so no
extra header rewriting is needed. If you're behind Cloudflare and want
to serve it on plain HTTP, use `http://grafana.extragornax.fr { … }` and
keep `GF_SERVER_ROOT_URL=http://grafana.extragornax.fr/`.

Once Grafana is up, build panels from queries like:

**Operational**
- **Cache age per slug** — `madcap_fast_cache_age_seconds`
- **Upstream latency trend** — `madcap_fast_upstream_last_ms`
- **304 rate** — `rate(madcap_fast_responses_not_modified_total[5m])`
- **Error rate** — `rate(madcap_fast_upstream_errors_total[5m])`
- **Cache body size** — `madcap_fast_cache_body_bytes / 1024 / 1024` (MB)

**Race data** (per-rider gauges, refreshed every 30 s)
Each cached event contributes a block labelled
`{slug,bib,name,category}`:

- `madcap_rider_distance_km`
- `madcap_rider_speed_kmh`
- `madcap_rider_overall_rank`  /  `madcap_rider_category_rank`
- `madcap_rider_battery_pct`
- `madcap_rider_sleeping` (0 / 1)
- `madcap_rider_distance_to_next_cp_km`

Plus event-level gauges labelled `{slug}`:
`madcap_event_total_km`, `madcap_event_participants`,
`madcap_event_active`, `madcap_event_sleeping`,
`madcap_event_started`, `madcap_event_finished`.

Example Grafana queries:

- **Rank-over-time worms** — `madcap_rider_overall_rank{slug="desertus-bikus-26"}` with legend `{{name}}`
- **Cactus delta per rider** (use your own start/end dates) —
  `madcap_rider_distance_km - on(slug) group_left() madcap_event_total_km * ((time() - $start_ts) / ($end_ts - $start_ts))`
- **Field speed histogram** —
  `histogram_quantile(0.5, sum by (le) (rate(madcap_rider_speed_kmh_bucket[5m])))`
  (not a histogram out of the box — use `quantile_over_time(0.5, madcap_rider_speed_kmh[15m])` for a quick median)
- **Finishers counter** — `madcap_event_finished{slug="desertus-bikus-26"}`
- **Low batteries** — `madcap_rider_battery_pct < 20`

Cardinality on desertus-bikus-26: ~376 riders × 7 metrics ≈ 2 500 active
series per event. Multi-event Prometheus handles tens of thousands
comfortably.

### Auto-deploy from GitHub

`.github/workflows/deploy.yml` SSHes into a target host on every push to
`master` (and supports manual `workflow_dispatch`), does `git pull --ff-only`
+ `docker compose up -d --build`, then polls the container's health status
until it reports `healthy`.

Required GitHub **secrets**: `DEPLOY_HOST`, `DEPLOY_USER`, `DEPLOY_SSH_KEY`
(and optionally `DEPLOY_PORT`). Optional repo **variable**: `DEPLOY_PATH`
(defaults to `/srv/madcap_fast`). Full setup notes are in the comment header
of the workflow file.

### Config (env vars, both modes)

| var                | default               | meaning                                      |
| ------------------ | --------------------- | -------------------------------------------- |
| `PORT`             | `9004`                | bind port                                    |
| `MADCAP_WARM_SLUG` | `desertus-bikus-26`   | slug to pre-warm on boot; set empty to skip  |
| `MADCAP_CACHE_DIR` | *(unset)*             | directory to persist snapshots to; unset = in-memory only. Compose sets this to `/var/cache/madcap_fast` and mounts a named volume there. |
| `RUST_LOG`         | `madcap_fast=info`    | standard `tracing_subscriber` filter         |
| `HOST_PORT`        | `9004`                | compose only: host-side port mapping         |

The server exposes:

- `GET  /`                       → HTML page — event picker (grid of all events)
- `GET  /event/:slug`            → HTML page — single-event view (list + map + feed tabs)
- `GET  /api/events`             → cached events list (refreshed every 5 min)
- `GET  /api/events/csv`         → events list as CSV
- `GET  /api/event/:slug`        → combined JSON, pre-compressed, ETag-aware (30 s refresh)
- `GET  /api/event/:slug/csv`    → current leaderboard (rank, bib, name, distance, speed, etc.) as CSV
- `GET  /metrics`                → Prometheus text-format metrics (request counters, cache ages and sizes per slug, upstream latencies, refresh/error counts)

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
