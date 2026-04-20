# Changelog

All notable changes to this project are recorded in this file.

Format loosely follows [Keep a Changelog](https://keepachangelog.com/). Entries are grouped per iteration (commit or uncommitted change set), most recent first.

## [Unreleased]

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
