# Changelog

All notable changes to this project are recorded in this file.

Format loosely follows [Keep a Changelog](https://keepachangelog.com/). Entries are grouped per iteration (commit or uncommitted change set), most recent first.

## [Unreleased]

### Added
- **"Nearby riders" section in the detail view** — for the selected rider, a table of the 10 closest other riders by straight-line distance (haversine on their latest GPS fixes, honouring the scrubber). Shows overall rank, name, star, staff badge, gap in km, and a coloured ± course-distance delta. Clicking a name opens that rider's detail.
- **Overtake feed** — every refresh diffs `overall_rank` against the previous snapshot. When a rider's rank improves, a lightweight entry is pushed into a rolling `overtakes` array (newest first, capped at 200). The Feed tab gained an **Overtakes** filter and the "All" view now merges overtakes with journal entries, sorted by timestamp. Each entry shows the rider, places gained, rank arrow, relative + absolute time, and links to their detail.
- **`monitoring/setup-public-dashboards.sh`** — one-shot / idempotent bash script that clones the `madcap-race` dashboard once per slug currently emitting race metrics (uid becomes `madcap-race-<slug>`, the `slug` variable is pinned and hidden, title includes the slug), upserts each clone via Grafana's API, enables its public dashboard, and prints the resulting `…/public-dashboards/<accessToken>` URLs. Requires `curl` + `jq`; Grafana admin creds via `GRAFANA_USER` / `GRAFANA_PASSWORD`.

### Removed
- **Anonymous Grafana access + Grafana 11.3.0 pin reverted.** The new Kubernetes-style dashboard API in Grafana 12+ refuses anonymous GETs; we briefly pinned to 11.3.0 and enabled `GF_AUTH_ANONYMOUS_*` to work around it. Replaced by Grafana's built-in external **dashboard / panel sharing** (which works regardless of version), so the pin and the four anonymous env vars are gone and the image is back to `grafana/grafana:latest`.

### Fixed
- **Grafana no longer crash-loops on the provisioned dashboards mount.** The previous setup overlaid `./monitoring/grafana/dashboards` onto `/var/lib/grafana/dashboards`, which lives inside Grafana's protected data directory — Grafana's startup tried to manage that directory as its own and failed. The bind-mount now lands at `/etc/dashboards` (outside the data dir), the provider config points at the same new path, and `disableDeletion: true` + `allowUiUpdates: false` match the read-only mount so Grafana never tries to write there.

### Changed
- **Grafana default host port moved from 9006 to 9007** — `9006` collided with another service; `9007` keeps everything in the 900x band (`madcap_fast:9004`, `prometheus:9090` loopback, `grafana:9007`). Override with `GRAFANA_PORT` if you need something else.

### Added
- **Auto-provisioned Grafana dashboard** — `monitoring/grafana/dashboards/madcap_fast-race.json` + a provisioning config that drops it into a "madcap_fast" folder on first boot. The dashboard has an **Event** variable populated from `label_values(madcap_event_total_km, slug)` (every live cached race appears automatically) and a **Top N riders** slider. Panels: participants / active / started / sleeping / finished / course-total stat cards, overall-rank-over-time worms, distance-over-time, a joined current leaderboard table (bib / name / distance / speed / battery with colour-gradient cells), a low-battery watchlist, and the operational refresh-latency + cache-age trends. The Prometheus datasource is pinned with `uid: prometheus` so the dashboard JSON is portable. `docker-compose.monitoring.yml` now bind-mounts the dashboards directory into the Grafana container.
- **Race metrics stop at the finish line** — `render_event_race_metrics` now parses `info.end_date` (hand-rolled UTC ISO parser, zero new deps) and returns `None` once the race is over. On the next 30 s refresh, the event's `race_metrics` slot goes empty and those Prometheus series disappear. Operational gauges (`cache_age`, `upstream_last_ms`, `cache_body_bytes`) keep reporting.
- **`MADCAP_WARM_SLUG` accepts a comma-separated list** — e.g. `desertus-bikus-26,via-race-25,aux-origines-26` warms each slug in a background task on boot. A Grafana dashboard can then use `label_values(madcap_event_total_km, slug)` to populate a per-event dropdown covering every live race.

### Added
- **Per-rider race metrics on `/metrics`** — each cached event now contributes a block of gauges labelled `{slug, bib, name, category}`: `madcap_rider_distance_km`, `madcap_rider_speed_kmh`, `madcap_rider_overall_rank`, `madcap_rider_category_rank`, `madcap_rider_battery_pct`, `madcap_rider_sleeping`, `madcap_rider_distance_to_next_cp_km`. Plus event-level gauges `{slug}`: `madcap_event_total_km`, `madcap_event_participants`, `madcap_event_active`, `madcap_event_sleeping`, `madcap_event_started`, `madcap_event_finished`. Rendered once per 30 s refresh (not per scrape), stored on `EventCache.race_metrics`, stitched into `/metrics` under a single set of HELP/TYPE headers. For desertus-bikus-26 that's ~2 500 series per event — trivial for Prometheus. README gains a "Race data" section with example Grafana queries (rank-over-time worms, cactus delta, speed quantiles, finisher count, low-battery alerting).

### Changed
- **Prometheus is no longer exposed on the public IP.** A shared `madcap_fast_monitoring` Docker network is now declared in the main compose and joined by `madcap_fast` + `prometheus`; the monitoring compose joins it too as `external: true`. Grafana talks to Prometheus by service name (`http://prometheus:9090`) instead of via the host bridge, so no `host.docker.internal` / `extra_hosts` is needed. Prometheus's `:9090` is still bound to the host but **on `127.0.0.1` only** (drop the `ports:` block to make it container-only). `/metrics` on `madcap_fast:9004` remains on the same public port as the app itself (nothing there is sensitive — just counters and gauges).
- **Monitoring stack split across the two compose files** — Prometheus now runs inside the main `docker-compose.yml` so metrics are scraped automatically whenever `madcap_fast` is up (90-day retention via a new `prometheus-data` named volume). Scrape config targets `madcap_fast:9004` by service name. `docker-compose.monitoring.yml` is now Grafana only — bring it up when you want dashboards without disturbing metric collection.

## [2026-04-21] 53874c1 — Metrics endpoint, CSV exports, Grafana how-to

### Added
- **Prometheus `/metrics` endpoint** — hand-rolled, zero new deps. Exposes counters (`madcap_fast_requests_total{path}`, `madcap_fast_responses_not_modified_total`, `madcap_fast_upstream_refreshes_total`, `madcap_fast_upstream_errors_total`) and per-slug gauges (`madcap_fast_cache_age_seconds`, `madcap_fast_cache_body_bytes`, `madcap_fast_upstream_last_ms`) plus the events-list cache age/size. Cache-Control is `no-store`.
- **CSV exports** — `GET /api/event/:slug/csv` dumps the current leaderboard (overall_rank, category, category_rank, bib, first/last name, nickname, country, distance_km, speed_kmh, distance_to_next_cp_km, battery_pct, last_ping, status, sleeping) with an `attachment` Content-Disposition. `GET /api/events/csv` dumps the events list.
- **README: Prometheus + Grafana setup guide** — sibling `docker-compose.monitoring.yml` + `prometheus.yml` snippets, Prometheus data source URL, and example Grafana queries (cache age / upstream latency / 304 rate / error rate / cache body size). Notes where per-rider time-series would go if we want race analytics later.

## [2026-04-21] 275049a — Map overlay declutter

### Changed
- **Map overlay decluttered** — the overlay used to carry 5 toggle buttons + 2 selects + search + count on one row, which got crowded as features piled up. Now it holds only the high-frequency controls (search, ★ only, ⚙ settings, count); clicking ⚙ opens a popover below with the rest (tile style, marker labels, traces / elev / journals toggles) grouped into a Style section and an Overlays section. Outside click closes it.

## [2026-04-21] 710885b — ETA-to-next-CP and ETA-finish predictions

### Added
- **Finish-time prediction + ETA to next CP** in the detail view — two new stat cells. `ETA next CP` uses the rider's current speed (or a rolling 1-hour average if they're barely moving) against `distance_to_next_cp.distance`. `ETA finish` projects the whole-course completion time from the remaining km divided by the rolling average, falling back to an event-wide average pace if the rolling window is empty. Returns `—` for stopped / finished riders.

## [2026-04-21] 1864ac0 — Top peak speeds leaderboard

### Added
- **Top peak speeds leaderboard** — new "Peak speeds — top 10" table in the overview (shown when no rider is selected) listing riders by the single highest `point[4]` value across their whole track. Columns: rank, name (clickable to open detail), ★ + staff badges, max km/h, localized timestamp of when they hit it. Cached by `state.tracks` identity.

## [2026-04-21] 0a00d75 — Journal pins layer on the map

### Added
- **Journal pins on the map** — new `journals` toggle in the map overlay (default off, persisted). Renders each `SLEEP` / `PICTURE` entry as a small circular marker (📸 / 🛌) at the entry's lat/lng. Clicking opens a popup with the rider name, type, timestamp, thumbnail (for photos) and an "open details →" shortcut. Honors the ★-only filter — journal pins follow the same favourites-only mode as the rider markers.

## [2026-04-21] 63cdcc6 — Organizer / staff badge

### Added
- **Organizer / staff badge** — participants with `attributes.orga === "1"` get a small orange `staff` chip next to their name in the leaderboard row, detail header, map popup, and feed entries. Makes race organizers easy to tell apart from actual competitors.

## [2026-04-21] 7a7cde5 — Disk cache persistence

### Added
- **Disk cache persistence across restarts** — new `MADCAP_CACHE_DIR` env var. When set, each refresh atomically writes the raw combined JSON to `<dir>/events/<slug>.json` (and the events list to `<dir>/events_list.json`) via tmp-file + rename. On startup the server walks the directory and rebuilds brotli / gzip / ETag via `snapshot_from_bytes`, so the first request after a restart is already warm instead of paying the ~2 s cold-fetch. `docker-compose.yml` enables this by default against a named `cache` volume at `/var/cache/madcap_fast`. Unset env var = in-memory only (previous behavior).

## [2026-04-21] 83dd56c — Shared cursor on the 3 profile graphs

### Added
- **Shared cursor on the 3 profile graphs** — a range slider below the elevation / speed / battery sparklines moves a gold vertical line across all three in lockstep, and hovering any of the graphs drives the same cursor. A readout below the slider prints the timestamp (localized) + elevation / speed / battery at the nearest point.

## [2026-04-21] 298f883 — Battery sparkline

### Added
- **Battery sparkline** in the rider detail view — third profile strip (blue) under elevation and speed, reading `point[5]` from the track. Header shows the current and minimum battery percentage. Hidden if the tracker never reports battery.

## [2026-04-21] 90649f5 — GitHub Actions auto-deploy + next-CP NaN fix

### Added
- **GitHub Actions auto-deploy** — `.github/workflows/deploy.yml` SSHes into a host on every push to `master` (or manual `workflow_dispatch`), runs `git pull --ff-only` + `docker compose up -d --build`, then polls the container's healthcheck until it's `healthy`. Parameterized via repo secrets (`DEPLOY_HOST`, `DEPLOY_USER`, `DEPLOY_SSH_KEY`, optional `DEPLOY_PORT`) and an optional `DEPLOY_PATH` variable (default `/srv/madcap_fast`). README gained a short "Auto-deploy from GitHub" section.

### Fixed
- **"To next CP" no longer shows `NaN km`** — upstream sends `distance_to_next_cp` as `{ cp_id, distance }`, not a scalar. `fmtKm` now unwraps objects and returns `—` on missing / NaN values.

## [2026-04-20] 8823ce3 — Map hotfix

### Fixed
- **Map no longer crashes with "Map has no maxZoom specified"** — `leaflet.markercluster` refuses to attach to a map with no `maxZoom`; added `maxZoom: 19` to `L.map()` options. Also falls back to a plain `L.layerGroup` if the cluster script failed to load, calls `refreshClusters()` after marker-position updates (cluster index would otherwise desync on `setLatLng`), and sets `pointer-events: none` on the elevation banner so it doesn't intercept zoom-control clicks.

## [2026-04-20] e34e1ba — Marker clustering, course elevation banner, port 9004

### Added
- **Marker clustering** on the map via `leaflet.markercluster` — rider markers now cluster below zoom 11, keeping the zoomed-out view legible with 300+ riders. Cluster bubbles are themed to match the dark UI (green / amber / red based on cluster size).
- **Course elevation banner** at the top of the map (toggle `elev` in the overlay, default on). Profile derived from the leading rider's track (the one who's covered the most ground); a gold vertical cursor shows where the 🌵 cactus pacer is, so it also reflects the scrubber. Persisted in `localStorage:madcap_map_elev`.

### Changed
- **Default bind port is now `9004`** (was `8080`) — matches what `docker-compose.yml` was already exposing on the host. Updated in `src/main.rs`, `Dockerfile`, `docker-compose.yml`, and README.

## [2026-04-20] 0b37537 — 100 km segments

### Added
- **100 km segments** — second split table below the CP segments, bucketing the track by every 100 km of actual distance covered (haversine along the points) with the same rank + gap columns. Markers per rider are cached by `state.tracks` identity so repeated detail views don't recompute.

## [2026-04-20] 7950691 — Segment timings + README rewrite

### Added
- **Segment timings** in the participant detail view — table of CP-to-CP splits showing each segment's duration, the rider's rank for that segment (across everyone who completed it) and the gap to the fastest rider on that leg.

### Changed
- **README rewritten** to match the current feature set: documents every tab (List / Map / Feed), controls (`ℹ`, `🔔`), playback scrubber, profiles, rest timeline, segments, cactus pacer, notification triggers, URL state, UTC-aware time display, and the server's paginated-tracks pipeline.

## [2026-04-20] 8301b7c — Changelog, event info drawer, UTC fix

### Added
- **Event info & sponsors drawer** — new `ℹ` button in the header opens a right-side panel with the event description, route / distance / surface, dates, website + Instagram links, emergency / organiser / technical phone numbers (as `tel:` links), and a 2-column grid of sponsor logos. Closes on ✕, backdrop click, or Escape.
- **`CHANGELOG.md`** itself, plus a persistent project memory telling future iterations to keep it current alongside code changes.
- **Finish detection** in CP notifications — title distinguishes `reached CPn` vs `finished at <name>` when the CP is `FINISH`-type.

### Fixed
- **UTC timestamp parsing** — upstream returns naive ISO strings (no `Z`). New `parseUtc()` helper appends `Z` when no offset is present and is routed through `fmtTime`, `sinceText`, `eventStartMs/EndMs`, `computeCactusPath`, leaderboard / map stale detection, feed sort, and the home-page classifier. Fixes "since" text, cactus position, and event-duration math for anyone whose browser isn't on UTC.

## [2026-04-20] 6d6e375 — Journals feed + more notifications

### Added
- **Journals feed (new "Feed" tab)** — global reverse-chronological timeline of `PICTURE` (with 140×100 thumbnails linking to the full image) and `SLEEP` entries. Filter pills: All / Photos / Sleeps / ★ favorites. Clicking a rider name opens their detail view. Participates in URL state (`?tab=feed`).
- **Extra notification triggers** on top of the initial CP / caught-by-cactus / low-battery set:
  - **Passed the cactus** (behind → ahead).
  - **Rank gain ≥ 10 places** in a single refresh.
  - **Long stop ≥ 45 min** while the rider's last fix is still inside the rest block. Fires once per stop; resets when they resume moving.
  - **New PICTURE** from a favourite, with the photo URL as the notification icon where supported.

## [2026-04-20] bb61142 — Notifications on selected runners

### Added
- **Browser notifications** (tab-open only, permission-gated). 🔔 toggle in the event header. Fires on 30 s refresh diff, only for favourites:
  - CP crossed (`p.cp_rank[i]` became non-null).
  - Caught by the cactus (distance delta flipped positive → negative).
  - Battery dropped from > 20 % to ≤ 20 %.
- Dedup via unique `Notification.tag` per trigger + event; first load seeds without firing.

### Changed
- `cargo fmt` across the Rust sources.

## [2026-04-20] a71603f — Graphs, stats, rest timeline

### Added
- **Cactus delta on every leaderboard row** — `p.distance − cactus_km` → ±time and ±km vs the pacer, green / red / gray colouring. Honors the scrubber (rows re-render on playback changes when the list tab is visible).
- **Rest & movement timeline** in the detail view — orange blocks on a green bar for stretches where `speed ≤ 1.5 km/h` for ≥ 20 min; header shows total moving / resting time and longest block.

## [2026-04-20] 737a373 — Clean Dockerfile

### Changed
- Dockerfile prime step stubs `src/lib.rs` and `benches/merge_tracks.rs` so the manifest parses with the new `[lib]` + `[[bench]]` targets. Real bench sources are not copied into the builder — only the stub is needed to satisfy `Cargo.toml`.

## [2026-04-20] 8c71f2e — Replay scrubber, elevation & speed profiles

### Added
- **Time scrubber + auto-play** (map tab) — range slider over `[event start, now]`, play / pause, speed dropdown (1 s = 1 min / 5 min / **20 min (default)** / 1 h / 6 h), `live` jump button, localized time label. rAF-throttled redraws. Scrubbing pauses playback automatically.
- Scrubber drives markers (last point ≤ T via binary search), traces (sliced to T), and the cactus marker.
- **Elevation and speed sparklines** in the participant detail view, inline SVG (no chart lib). Elevation shows min/max range; speed shows max + avg.

## [2026-04-20] 9abacac — Benchmarks

### Added
- `src/lib.rs` exposing a pure `merge_track_pages(&[Value]) -> Value` extracted from `fetch_tracks_paginated`.
- `benches/merge_tracks.rs` with three criterion workloads: small (3 × 50 × 100 pts), realistic (3 × 320 × 200 pts, "desertus today") and worst-case (10 × 320 × 200 pts, ~10-day event).
- `criterion = "0.5"` dev-dependency and `[[bench]]` stanza in `Cargo.toml`.

### Changed
- `fetch_tracks_paginated` now delegates merge/sort/dedup to `madcap_fast::merge_track_pages`.

## [2026-04-20] 5c35857 — Paginated tracks

### Added
- **Server-side tracks pagination** — new `fetch_tracks_paginated` walks the upstream's `previous_page_ts` cursors (cap 30 pages), merges per participant, sorts by timestamp and dedups page-boundary overlaps. The frontend continues to consume a single `tracks` field; cache payload is now full event history instead of just the latest ~24 h window.

## [2026-04-20] 4a7421f — Cactus pacer & marker display styles

### Added
- **Virtual Cactus pacer** on the map — a 🌵 marker interpolated along the Cactus route at `(now − start) / (end − start)` × total distance. Click for popup with % and km. Auto-updates every 60 s.
- **Marker label styles** — dropdown in the map overlay to toggle between dots / bibs / names. Persisted in `localStorage:madcap_map_labels`.
- **Pale sand colour** for the Cactus route polyline (upstream sent pure black, unreadable on dark tiles).

## [2026-04-20] fdcd254 — Traces + dim cactus

### Added
- **Participant trace polylines** — the selected rider's trace is drawn bright; each favourite's trace is dimmer (coloured by rank).
- **Trace toggle** (`traces` button) and **★ only** toggle in the map overlay, synced with the sidebar ★ filter.
- **URL state** — `?tab=`, `?p=`, `?cat=`, `?fav=` in the URL so paste-back restores the map tab, selected rider, category filter, and favorites-only view.

## [2026-04-20] fd122ad — Gender / category sorting

### Added
- **Category filter pills** in the sidebar (values read from `attributes.category` — on desertus-bikus-26: `F` femme, `H` homme, `M` mixte). Selecting a category filters the leaderboard and re-ranks by `p.rank` (per-category rank) instead of `p.overall_rank`.

## [2026-04-20] d2baac9 — Favorites

### Added
- **★ favorites** — per-event localStorage (`madcap_fav:{slug}`). Star button on leaderboard rows, detail header, and map popup. Favorites pin to the top of the leaderboard. Favorite map markers get a gold ring. "★ only" filter in the sidebar search bar.
- **6 map themes** — dropdown in the map overlay: Dark (default) / Light / Voyager (Carto) / OSM / Satellite (Esri) / Topo (OpenTopoMap). Persisted in `localStorage:madcap_map_theme`.

## [2026-04-20] cbbccb0 — Dockerized

### Added
- `Dockerfile` (multi-stage Rust + debian slim) and `docker-compose.yml` with a health check.

## [2026-04-20] dc482c3 — README

### Added
- Project README.

## [2026-04-20] a85dbde — First run

### Added
- Initial Rust/Axum caching proxy for the Madcap API.
- Per-event combined snapshot (info + participants + geo + journals + tracks), refreshed every 30 s.
- Events list cache, refreshed every 5 min.
- Brotli + gzip precomputed bodies, FNV-1a ETags, `x-cache-*` debug headers.
- Embedded single-page HTML with home events grid, leaderboard sidebar, detail view, and Leaflet map view.
