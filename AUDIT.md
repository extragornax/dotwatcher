# Code audit — dotwatcher (madcap_fast)

Audit performed 2026-07-27 against `master` @ `a54df91`, working tree clean.

Scope: `src/main.rs` (1499 L), `src/index.html` (3482 L), `src/lib.rs` (50 L), `Cargo.toml`,
`Dockerfile`, `docker-compose.yml`, `monitoring/`, `README.md`.

Scale context that drives most findings: the combined API payload is ≈ 6.7 MB raw
(2.1 MB br), of which ≈ 5.9 MB is tracks; it is refreshed every 30 s per event and
re-downloaded in full by every browser on the same cadence. `index.html` is 158 KB and
is currently served uncompressed. Events reach ~300 participants.

### Verification legend

- **[V]** — verified directly against the source during this audit.
- **[R]** — reported by an analysis pass, plausible but not independently confirmed.
  Line numbers may drift; locate by symbol name.

---

## 1. Security

### S1 — XSS via pacer logo URL **[V]** `src/index.html:1602-1605`

```js
function cactusIcon() {
  const cp = computeCactusPath();
  return cp && cp.logoUrl ? `<img src="${cp.logoUrl}" alt="pacer">` : '🌵';
}
```

`logoUrl` originates from `snail.logo_url` in the geo API and is interpolated with no
escaping, then injected as `L.divIcon({ html: … })`. A value of `x" onerror="…`
executes script. This is the clearest injection in the codebase.

**Fix:** escape the attribute and allow only `http(s):` schemes; fall back to `'🌵'`.

### S2 — Further injection gaps **[R]**

| Location | Issue |
| --- | --- |
| `renderFeed` ~`:2306` | `class="type-badge ${e.type.toLowerCase()}"` — text node is escaped, attribute is not |
| `renderHome` ~`:3389` | `background-image:url('${escapeAttr(…)}')` — the HTML parser decodes `&#39;` back to `'` before CSS parses it, reopening the string |
| ~`:1909`, ~`:2299`, ~`:2177` | `href` interpolations of API strings; `escape()` does not block `javascript:` |

**Fix:** `escapeAttr` the class; convert the home banner to `<img loading="lazy">`
(also a perf win, see P20); add a `safeHref()` helper permitting only `http(s)://`.

### S3 — Image proxy hardening **[R]** `src/main.rs:1314-1392`

1. **SSRF via redirect.** The allowlist at `:1319` validates only the *initial* URL, but
   reqwest follows up to 10 redirects by default and the client never overrides this. A
   `storage.googleapis.com` object returning a 302 to an internal address would be
   followed.
2. **Decompression bomb.** `image::load_from_memory` at `:1382` runs with no
   `image::Limits`; a small PNG declaring 30000×30000 forces a multi-GB allocation.
3. **Lossless-WebP regression.** `image-webp 0.2.4` **[V, Cargo.lock]** only implements
   VP8L lossless encoding. Journal photos are JPEG, so the proxy is very likely emitting
   thumbnails *larger* than the source while burning far more CPU.

**Fix:** dedicated client with `redirect::Policy::none()`; set `Limits`; encode JPEG q80
instead of WebP and version the disk-cache key so stale entries miss.

### S4 — Prometheus published on all interfaces **[V]** `docker-compose.yml:47`

```yaml
- "${PROMETHEUS_PORT:-9090}:9090"
```

`README.md:300-302` and the comment at `docker-compose.yml:43-45` both state this is
loopback-only. It is not — Docker publishes it on every interface and punches through
the host firewall.

**Fix:** `"127.0.0.1:${PROMETHEUS_PORT:-9090}:9090"`.

### S5 — Unbounded slug creates an immortal refresher **[V]** `src/main.rs:345-371`

`ensure_cache` is reachable from three public handlers. For *any* string passing
`slug_ok` (alnum/`-`/`_`, ≤100 chars) it inserts a permanent `EventCache` **and spawns a
refresher task that polls five upstream endpoints every 30 s, forever**. The task
survives even when upstream 404s. A loop over random slugs yields unbounded task growth
and unbounded amplified load against `api.madcap.cc`.

**Fix:** validate the slug against the cached events list before creating a cache
(fail-open if the list is unavailable); stop the refresher after ~20 consecutive
failures.

Related: `/metrics` itself is served on `0.0.0.0:9004` with `CorsLayer::permissive()`
and no auth, while Prometheus is *intended* to be loopback-only — inconsistent posture.

---

## 2. Correctness

### C1 — UTF-8 boundary panic permanently kills a refresher **[R]** `src/main.rs:185, 216`

`&text[..text.len().min(200)]` panics with *"byte index 200 is not a char boundary"*
whenever an upstream error body exceeds 200 bytes and byte 200 lands mid-sequence — any
accented character or emoji in a CDN error page. Because this runs inside the spawned
refresher loop, the panic aborts that task: **the slug's snapshot freezes forever**, with
no log or metric signalling it.

**Fix:** char-boundary-safe truncation.

### C2 — `MADCAP_WARM_SLUG` silently ignored **[V]**

`src/main.rs:1456` reads `DOTWATCHER_WARM_SLUG`. Every deployment path sets the other
name: `Dockerfile:35`, `docker-compose.yml:13`, and `README.md:81/272/423`. The
documented recipe `MADCAP_WARM_SLUG=some-other-event docker compose up -d` is a no-op —
it only appears to work because the hardcoded fallback happens to match the compose
default.

**Fix:** read `MADCAP_WARM_SLUG` first, fall back to `DOTWATCHER_WARM_SLUG`.

### C3 — SIGTERM not handled **[R]** `src/main.rs:1496-1499`

`shutdown()` awaits only `ctrl_c()` (SIGINT). Docker sends SIGTERM, tini forwards it,
nothing handles it — `docker stop` hard-kills and graceful shutdown never runs. The
tokio `signal` feature is already enabled.

### C4 — Lap-event metrics are wrong **[V]** `src/main.rs:786-790`

`parse_km_prefix("3.56km x 24h")` returns **3.56** — the lap length, not the course
total. The backend never reads `ranking_type` at all (`grep` returns nothing), while the
frontend already models this correctly at `src/index.html:809-823`.

| Metric | Line | Failure |
| --- | --- | --- |
| `dotwatcher_event_total_km` | `:791-794` | reports 3.56 as the course total |
| `dotwatcher_event_finished` | `:871-873` | `d >= total_km - 0.5` ⇒ every rider past 3.06 km counts as finished |
| `_event_cactus_km`, `_rider_cactus_delta_km` | `:806-816`, `:874-876` | pacer caps at 3.56 km, delta ≈ rider distance — meaningless |

Two further `parse_km_prefix` failure modes: `"350 / 700 km"` → 350 (total is 700), and
`"1,200km"` → 1.0 (comma terminates the scan; the frontend handles commas, the backend
does not).

**Fix:** branch on `ranking_type == "ranked_by_laps"` — emit `dotwatcher_event_lap_km`,
suppress `finished` and the cactus series; handle the comma separator.

### C5 — Disk-restored snapshots misreport their age **[R]** `src/main.rs:309, 404`

`snapshot_from_bytes` stamps `fetched_at: Instant::now()`, so a snapshot restored from a
three-day-old file advertises `x-cache-age-ms: 0`, `x-cache-stale: 0`, and
`dotwatcher_cache_age_seconds = 0`. The truthful value is in the body as
`fetched_at_unix` but is never read back. Restored snapshots also report
`upstream_ms = 0`.

Related: `STALE_AFTER` (`:33`) is computed and exposed but **nothing acts on it** — no
forced refresh, no status change.

### C6 — Detail panel shows data frozen at click time **[V]** `src/index.html:878`

`load()` replaces `state` wholesale but never re-resolves `selected` against the new
`state.participants`. `render()` then calls `renderDetail(selected)` with the object
captured whenever the user clicked, so **distance, speed, rank, battery, and status in
the detail panel never update** — only track-derived sections refresh.

**Fix:** `selected = state.participants.find(x => x.id === selected.id) || selected;`
immediately after the parse.

### C7 — Map popups frozen at marker-creation time **[R]** ~`:1218`, `:1254-1259`

`partsById` is rebuilt per render but captured by the popup and click closures. Markers
are created once and reused, so every marker permanently pins the `state.participants`
array from the poll that created it — stale popup content *and* a retention leak holding
multiple payload generations alive.

### C8 — Feed render aborts on journals without coordinates **[R]** ~`:2307`, ~`:2521`

`e.latitude.toFixed(4)` with no null guard. `renderJournalPins` (~`:1898`) explicitly
guards the same data, so such entries demonstrably exist; one of them kills the entire
feed render mid-fragment.

### C9 — A failed first load is permanent **[R]** ~`:3466-3476`

`refreshTimer = setInterval(load, 30000)` lives *inside* `load().then()`. One failed
initial request — a blip, a 500 — and the app never polls again.

### C10 — Overlapping, out-of-order polls **[V]** `src/index.html:866`

No in-flight guard and no `AbortController`. The server's upstream timeout is 45 s
(`main.rs:1419`) against a 30 s poll interval, so responses can land out of order and an
older payload can clobber newer state. User interaction between `await fetch` and
`render()` is also silently overwritten.

### C11 — Concurrent wind renders orphan animated layers **[R]** ~`:1791`

`renderWindOverlay` is `async` and called un-awaited from `renderMap`. Two interleaved
invocations leave the loser's leaflet-velocity canvas on the map with a **particle
animation loop that never stops**. Since `renderMap` fires from the playback rAF, these
can stack.

---

## 3. Performance

### Backend

**P1 — `index.html` served uncompressed, no ETag [V]** `src/main.rs:1394-1405`.
The handler sets only `Content-Type` and `Cache-Control: max-age=60`; the router
(`:1469-1481`) has `CorsLayer` and `TraceLayer` but no `CompressionLayer`. 158 KB on the
critical path for first paint, on both `/` and `/event/{slug}`. Since the content is a
compile-time constant, compress once at startup into a `OnceLock` (brotli q11 + gzip 9)
with a stable ETag and reuse the existing Accept-Encoding negotiation. **158 KB → ~30 KB.
Cheapest, highest-impact change in the codebase.**

*Update — implemented; see CHANGELOG.* Measured: 162,533 B → **34,007 B** brotli
(79 % smaller), 40,785 B gzip, with `ETag` + `If-None-Match` → 304 on both `/` and
`/event/{slug}`.

**P2 — parse → deep-clone → reserialize → revalidate [R]** `src/main.rs:188-192`,
`:219-241`. `fetch_raw` makes four full passes over every payload: `res.text()` (UTF-8
scan), `from_str` (full DOM), `.cloned()` (**deep clone of the whole DOM**), then
`RawValue::from_string(to_string(…))` — which serializes *and* re-parses to validate.
`README.md:73` claims inner payloads are never re-parsed; that is false for all five
endpoints. The 5.9 MB tracks blob currently absorbs 2 DOM builds + 2 deep clones + 2
serializations per refresh.
**Fix:** `struct Env { data: Option<Box<RawValue>> }`; `Value::take` instead of
`.cloned()`; `serde_json::value::to_raw_value` instead of the string round-trip.

**P3 — `merge_track_pages` allocation storm [R]** `src/lib.rs:12-50`. Not O(n²), but
allocation-bound: `.extend(points.iter().cloned())` deep-clones every point (192 k array
allocations at the benchmark's realistic size, 640 k for ten pages); `entry(pid.to_string())`
allocates on every hit; `sort_by` re-extracts both keys on each of ~3.4 M comparisons.
**Fix:** take `Vec<Value>` by ownership, `mem::take` + `Vec::append`, `sort_by_cached_key`.
A criterion bench already exists at `benches/merge_tracks.rs` to prove the delta.

**P4 — The 6.7 MB body is DOM-parsed twice per refresh [R]** `src/main.rs:474`, `:479`.
`render_event_race_metrics` and `ranks_from_body` each `from_slice` the entire payload
though both need only `info` + `participants` (~260 KB), and both run on the async
runtime thread rather than `spawn_blocking`. Same double-parse in `restore_from_disk`.

**P5 — Blocking fs I/O held under two write locks [R]** `src/main.rs:481-513`. Both
guards are taken, then held across `to_vec` and `persist_bytes` — `create_dir_all` +
`write` + `rename`, blocking syscalls inside an async task. This stalls a tokio worker
and blocks every concurrent `/overtakes` reader. Also blocking-in-async at `:469`,
`:397`, `:437`, `:1331`, `:1369`.

**P6 — 6.7 MB clone + serial hash on the async thread [R]** `src/main.rs:283`, `:302`.
`bytes.clone()` exists only to move into `spawn_blocking`; `fnv1a` then walks 6.7 MB
byte-at-a-time outside it. Move both inside. Transient peak per refresh is ~29 MB.

**P7 — No refresh jitter [R]** `src/main.rs:523`. Every refresher sleeps exactly
`REFRESH_INTERVAL`, and all were spawned near-simultaneously at boot, so they stay
phase-locked forever: a synchronised burst of N×5 upstream requests followed by N
simultaneous 6.7 MB parses and brotli compressions, every 30 s.

**P8 — Per-request work that should be pre-rendered [R]**. Both CSV handlers
(`:1157`, `:1254`) re-parse the full 6.7 MB snapshot and deep-clone the participants
array on every request; `/overtakes` (`:1225`) clones and re-serializes the whole deque.
All three outputs change only every 30 s — render them in the refresher.

**P9 — Cold-start path polls a lock at 100 ms [R]**, duplicated at `:575`, `:599`,
`:1147`, `:1245`. A `tokio::sync::Notify` would wake instantly. Thundering-herd
protection for event data is otherwise structurally correct (one refresher per slug), but
`/img` has **no coalescing and no negative caching** — 50 concurrent misses on one URL
produce 50 fetches, 50 decodes, and 50 racing non-atomic writes.

**P10 — Build [R]**: `brotli` 7 (direct) and 8 (via reqwest) are both compiled and
linked — bump to 8 to unify. `tower-http`'s `set-header` feature is enabled but unused.
`lto = "fat"` would buy a few percent over the current `thin`. No `zstd` encoding, which
modern browsers now request and which compresses faster than brotli-6 at similar ratios.
No disk eviction for `<dir>/img/` or `<dir>/events/`, on a `read_only: true` container
whose cache volume is the only writable path.

### Frontend

**P11 — Every poll invalidates every memo cache [V]** `src/index.html:878`.
`state = JSON.parse(txt)` replaces the object graph wholesale, and four caches key on
object *identity* — `courseProfileCache` (`:1487`), `topSpeedsCache` (`:2712`),
`kmMarkersCache` (`:2750`), `routeIndex` (`:1338`). All four miss on every poll even when
the payload is byte-identical.
**Fix:** compare the response `ETag` against the last seen value and skip parse + render
entirely when unchanged.

*Update — implemented; see CHANGELOG.* Measured on a live 117-rider event: unchanged
polls now cost ~2 ms instead of a 4.5 MB parse (22–44 ms) plus a full re-render.
One claim in the original finding did **not** reproduce: the detail panel does *not*
lose its scroll position on re-render — `scrollTop` was preserved across render polls in
testing, because `renderDetail` rewrites `#body` inside the scroll container and the
content height is stable. The profile-cursor snap was not re-tested.

**P12 — The map pipeline runs while the map is hidden [V]** `src/index.html:902`.
`if (map) renderMap();` — once the user has *ever* opened the map tab, the full
traces + pins + night + wind + cactus + elevation pipeline re-runs every 30 s from the
List tab. Gate on `currentTab === 'map'` with a dirty flag.

**P13 — O(n²) rider × track scans [R]**. `lastPointOf` (~`:944`) does a linear `.find`
over all tracks and is called once per participant by `renderMap`, and again inside
`renderNearby` (~`:2903`) — 90 000 comparisons per render at 300 riders, on top of up to
600 000 `snapToRoute` segment projections per frame when interpolation is on.
`renderTraces` and `renderFeed` already build a `Map<pid, track>`; `renderMap` and
friends should reuse one built once per poll.

**P14 — `renderList` teardown [R]** ~`:2447-2489`. Correctly uses `DocumentFragment` +
`replaceChildren`, but rebuilds ~300 rows and attaches **900 listeners** per render, and
re-renders the category and sort bars on every keystroke. Event-delegate to `#plist`.

**P15 — Playback rebuilds the list at 5 Hz [R]** ~`:1130`, ~`:1106`. `playTick` runs on
a 200 ms `setInterval` and calls `renderList()` synchronously *outside* the existing rAF
scheduler; `setPlaybackTime` does the same on every scrubber `input` event, so dragging
the slider triggers one full list rebuild per pointer move.

**P16 — Timers never pause [R]**. No `visibilitychange` handling: polling and full
re-render continue indefinitely in background tabs. `startInterpTicker` (~`:1035`) runs
every 2 s forever, even with the estimate toggle off and the map tab hidden.

**P17 — Unbounded caches [R]**. `WEATHER_CACHE` (~`:2807`) checks a TTL on read but never
deletes. `cactusPath` is memoized once and never invalidated. The three identity-keyed
caches each pin a full previous `tracks` payload, so up to 3× the payload can be retained
beyond the live copy.

**P18 — SVG costs [R]**. `sparklineSvg` (~`:2556`) emits one `L` command per track point
with no downsampling — a 5000-point track becomes three ~5000-segment paths in a
600×80 px box, while `computeCourseProfile` (`:1502`) already downsamples to 600.
`renderProfiles` (~`:3012-3016`) uses `Math.min(...spread)`, which is slow and a
`RangeError` risk past ~100 k arguments. `renderCourseElev` (~`:1518-1591`) rewrites the
whole banner `innerHTML` per render *and per playback frame*, though only the cursor
x-coordinate changes.

**P19 — Network [V/R]**. The `Accept-Encoding` header at `:869` is a forbidden header
name and is silently dropped by `fetch` — dead code. There is no explicit
`If-None-Match`, and since `fetch` transparently returns the cached body on a 304, the
full parse cost is paid regardless of revalidation. `Cache-Control:
stale-while-revalidate=60` on the API (`main.rs:588`) combined with a 30 s poll means the
browser can serve a body up to 75 s old, so the UI may run a poll behind. Tracks are
append-only yet the full 5.9 MB is re-sent every 30 s — a `?since=` delta endpoint is the
single largest network win available.

**P20 — CDN hygiene [R]** `:7-11`, `:432-435`. SRI is present on `leaflet.js` only;
`leaflet.markercluster.js` and `leaflet-velocity.js` have neither `integrity` nor
`crossorigin`. No `preconnect` for unpkg, cartocdn, or open-meteo. All three scripts load
synchronously and block the inline script, though `leaflet-velocity` (~50 KB) is only
needed when the wind overlay is enabled — which is off by default.

---

## 4. Tests

There are none. `grep -rn "cfg(test)"` over `src/` and `benches/` returns nothing.
`parse_km_prefix`, `parse_iso_utc`, `days_since_epoch`, `esc_label`, `slug_ok`, and
`merge_track_pages` are all pure and trivially testable, and C4 and C1 are exactly the
kind of bug a three-line unit test would have caught.

---

## 5. Metrics cardinality

`src/main.rs:850-856` attaches `bib`, **`name`**, and `category` labels to nine per-rider
series — roughly 2700 series per event, retained for 90 days
(`docker-compose.yml:38`). Rider *name* as a label value is the classic Prometheus
anti-pattern: a display-name edit mints an entirely new series set. Combined with the
unbounded slug cache (S5), this is a real Prometheus-side memory risk.

**Fix:** move name and category to a `dotwatcher_rider_info{bib,name,category} 1` metric
and key the rest on `bib` alone.

Note: `/metrics` correctly does *not* refetch upstream — race metrics are pre-rendered
once per refresh and the handler only concatenates. That part is the right architecture.
The comment at `:796` claiming "at scrape time" is stale; values are up to 30 s old.

---

## 6. Documentation drift

- `README.md:81/272/423` document `MADCAP_WARM_SLUG`, which the binary ignores (C2).
- `README.md:300-302` claims Prometheus is loopback-bound; it is not (S4).
- `README.md:463` — "Cache is in-memory per process. Restart = cold start" — superseded
  by disk persistence at `main.rs:373`.
- `README.md:73` — the `RawValue` pass-through claim is inaccurate (P2).
- `README.md:456` omits the battery channel at track-point index `[5]`.
- `.idea/` is committed and still carries the pre-rename project name.

---

## 7. False positive

An analysis pass reported that the keyboard-shortcut block documented at
`README.md:217-229` had been lost, leaving `moveSelection` as dead code. **This is
wrong.** The full handler is present and correct at `src/index.html:3203-3228`, covering
`j`/`k`/arrows, `m`/`l`/`e`, `f`, `Space`, `/`, and `Esc`, with input suppression at
`:3211` and modifier guards at `:3212`. `moveSelection` is called from `:3214` and
`:3215`. No action needed — recorded here so the claim is not acted on later.

---

## 8. Suggested order of work

1. **Security** — S1, S2, S3.
2. **Backend correctness** — C1, C2, C3, C4, C5, plus S5.
3. **Frontend correctness** — C6 – C11.
4. **Backend performance** — P1 first (largest user-visible win), then P2 – P10, with
   unit tests landing alongside.
5. **Frontend performance** — P11 and P12 first (they subsume much of the rest), then
   P13 – P20.
6. **Docs and compose** — S4 and section 6.

Each step should carry its `CHANGELOG.md` entry in the same commit, per existing repo
convention.

---

## 9. Feature roadmap

Ordered by stated preference. None of these are implemented yet.

1. **Ranking-type aware UI.** `info.ranking_type` has several values upstream
   (`ranked_by_cp` ×113, `no_ranking` ×53, `ranked_by_distance_along_route` ×8,
   `ranked_by_laps` ×5, `ranked_by_points` ×2, `ranked_by_next_cp_distance` ×2,
   `ranked_by_monaco` ×2) but the UI branches on exactly one of them (`isLapEvent()`,
   `:809`). Everything else falls through to a distance/rank display that is simply wrong
   for points- and CP-ranked events. Generalise into a strategy: a points column for
   `ranked_by_points`, a CP-order board for `ranked_by_cp` (the data is already in
   `cp_rank`), and no rank UI at all for `no_ranking`. The lap-unit toggle is the
   template to follow.

2. **Richer rider data.** These fields are already in the cached payload at zero
   additional cost and are currently unused: `gps_accuracy`, `penalty`,
   `start_time_offset`, `age`, `ranks[]`, `attributes.variant`. `country` appears only as
   plain text in the detail tagline. Highest value among them is
   `start_time_offset` — without it, elapsed times are simply wrong for wave starts.
   Then: country flags with a nationality filter, a `penalty` badge on the leaderboard,
   a GPS-accuracy indicator, and age-group ranks.

3. **Mobile and PWA.** One `@media (max-width: 880px)` rule exists (`:282-286`). The map
   overlay, scrubber, and settings popover have no mobile treatment and the three-tab
   header is desktop-shaped. There is no manifest, no service worker, and no
   `theme-color`. The server already serves immutable, pre-compressed, ETagged snapshots —
   an SW cache is a natural fit for dot-watching on a patchy signal.

4. **Per-lap analytics.** The km/laps toggle currently only relabels a number. Lap split
   table, lap-time consistency chart, and a current-lap indicator are all reachable now
   that `lapLengthKm()` (`:815`) has solved the parsing problem.

5. **Two-sided overtakes and wider notifications.** `OvertakeRecord`
   (`main.rs:46-56`) stores only `{t, pid, from, to}`, so the feed can render "climbed
   +N" but not the "who passed whom" that `README.md:206` promises — adding the displaced
   rider's id server-side fixes both. Seven notification triggers exist; nothing fires on
   finish/DNF, `penalty` applied, a tracker going silent, or a favourite entering the
   top 10, and all the inputs are already in `state`.

6. **Delta tracks endpoint** (`?since=`) — see P19; the largest network win available.

7. **Further out:** SSE push to replace the 30 s poll (the server already knows exactly
   when a refresh lands); zstd encoding; a spatial index for `snapToRoute`; pre-rendered
   CSV; additional exports (CP splits, segment times, overtake history) for data already
   computed client-side and discarded; multi-event comparison; Prometheus alert rules,
   for which `dotwatcher_upstream_errors_total` and `_cache_age_seconds` are ready-made
   SLO signals.
