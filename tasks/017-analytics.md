---
id: 017
slug: analytics
model: gpt-6.1-sol
effort: high
review_effort: xhigh
depends_on: [010, 011, 013]
crates: [yhotos-http, yhotos-db, yhotos-worker]
---

# 017 First-party analytics and the admin dashboard

## Goal

Every meaningful public interaction is recorded server-side without cookies or third-party scripts, rolled up nightly, and shown to the admin in a fast dashboard: views per photo and album, downloads by size, zip downloads, searches, referrers, countries and cities, device classes, share-link stats, and real-user Web Vitals. Privacy rules from `docs/threat-model.md` 4.8 are enforced in code and tested.

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.6
- `docs/architecture.md` 4 (`events`, `events_daily`), 12
- `docs/threat-model.md` 4.8, 4.4 (DoS through event writes)
- `docs/performance.md` 2 (real user monitoring)

## Scope

Areas: `crates/yhotos-http/src/analytics/` (middleware, beacon endpoint, GeoIP lookup), `crates/yhotos-db/src/events.rs` and migrations for partitioning/indexes, `crates/yhotos-worker/src/rollup.rs`, admin dashboard routes and templates (feature `admin`), `web/src/vitals.ts`, `web/src/dashboard.ts`, `scripts/geoip-update.sh`, plus supporting files.

## Design

### Collection
- Middleware on public HTML routes records `page_view` with `kind` (home, album, photo, search, map, calendar, share), target ids, `referrer_host` (host only, from `Referer`, our own host recorded as internal), `device_class` (desktop, mobile, tablet, bot from a small UA classifier; bots are recorded with `device_class = bot` and excluded from dashboard totals by default), `country`/`city` from a local MaxMind-format MMDB (`maxminddb` crate; file path from config; missing file means null geo), `visitor_hash` = BLAKE3(ip || ua_class || daily_salt) truncated to 16 bytes.
- Daily salt: 32 random bytes generated at process start and at each UTC midnight, kept only in memory, never logged or persisted. Multiple server processes therefore produce different hashes for the same visitor on the same day; accept this (documented) rather than persisting the salt.
- File responses: originals and zip downloads record `download` with variant/size; derivative image hits are not recorded per request (most never reach origin), except a sampled 1% `image_hit` to estimate edge offload.
- Lightbox opens, likes, flags, slideshow starts arrive from islands via `POST /api/v1/e` (batched, max 20 events per request, max 2 KB body, rate limited `cheap`, same-origin required). Only an allow list of event kinds and fields is accepted.
- Web Vitals: `vitals.ts` (using the `web-vitals` attribution-free build, vendored, ~2 KB) sends LCP, CLS, INP, TTFB once per page via `navigator.sendBeacon` to `/api/v1/e`.
- Writes are buffered in-process (bounded channel, 10k events) and flushed in batches every second with `COPY` or multi-row `INSERT`; a full buffer drops events and increments a metric rather than blocking requests.
- Search queries are stored normalized (lowercased, filters kept, free text truncated to 64 chars).

### Storage
`events` is range-partitioned by month (declarative partitioning); a worker job creates next month's partition and drops partitions older than 90 days. `events_daily` rollup keyed by (day, kind, target_kind, target_id, dimension, value) for referrer, country, device, size. `share_link_opens` from task 010 is folded into the same rollups.

### Rollup job
Nightly `Rollup { day }` (and hourly for the current day) aggregates into `events_daily` idempotently (delete-and-insert for the day in one transaction). Unique visitors are counted per day only (hashes do not link across days by design).

### Dashboard (admin)
`/admin/analytics`: date range picker (7d, 30d, 90d, custom), totals (views, unique visitors per day averaged, downloads, searches), time series (server-rendered SVG sparkline and bar charts, no chart library), top photos, top albums, top referrers, top countries, device split, top searches, share-link table with opens and unique visitors, Web Vitals p75 by device class with pass/fail against `docs/performance.md`. Per-photo and per-album detail pages. CSV export of any table. All queries hit `events_daily` (plus `events` for today), each under 50 ms.

### Tests
- No cookie is ever set by analytics (assert response headers on every recorded route).
- Raw IP never stored: insert path test plus a grep-based test over the schema that no column named like `ip` exists except `ip_hash` in sessions.
- Salt rotation changes the hash; salt is not in logs (tracing capture test).
- Event endpoint rejects unknown kinds, oversized batches, cross-origin requests.
- Buffer overflow drops and counts, never blocks (test with a stalled database).
- Rollup idempotency and partition maintenance.
- Bot traffic excluded from default totals.
- Dashboard queries' `EXPLAIN` uses `events_daily` indexes.

## Acceptance

- [ ] All tests above pass.
- [ ] A load test of 1,000 page views per second for 30 s on the dev stack records every event without raising request p99 by more than 1 ms (record numbers).
- [ ] Dashboard screenshots in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.8 is the checklist. The MMDB file is operator-supplied (GeoLite2 requires a free licence key the operator obtains); the app never downloads it.
- Event endpoint is a write path on the public listener: strict schema, size caps, rate limits, same-origin.
- Allowed new dependencies: `maxminddb`, `woothee` or hand-written UA classification (hand-written, small, preferred), `web-vitals` (vendored JS).

## Performance

- Recording adds no database round trip to the request path (buffered).

## Not in this task

- Cloudflare analytics ingestion.
- Per-user analytics for logged-in viewers (admin sees likes/flags per user already).
