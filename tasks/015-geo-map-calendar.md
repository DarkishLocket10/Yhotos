---
id: 015
slug: geo-map-calendar
model: gpt-6.1-sol
effort: high
review_effort: high
depends_on: [007, 011]
crates: [yhotos-http, yhotos-db, yhotos-jobs]
---

# 015 Reverse geocoding, map view, calendar view

## Goal

Photos with GPS get a `place` (city, region, country) from an offline gazetteer via the `GeoReverse` job; the public map shows visible photos clustered with a self-hosted or configurable tile source; the calendar view browses by year, month and day. Both views are server-rendered with small islands, respect visibility, and stay within the page budgets.

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.1, 5.5 (place filter)
- `docs/architecture.md` 4 (`places`), 8 (`/map`, `/calendar`), 9
- `docs/threat-model.md` 4.4 (SSRF: the server never fetches tiles), 4.8 (GPS privacy: stripped from derivatives, map shows only what the viewer may see)
- `docs/performance.md` 1
- Briefs 007 (job stubs), 011 (templates, visibility SQL)

## Scope

`crates/yhotos-worker/src/geo.rs` (GeoReverse handler), `crates/yhotos-db/src/{places,calendar,map}.rs` + migration for the gazetteer table, `crates/yhotos-http/src/pages/{map,calendar}.rs`, templates `map.html`, `calendar.html`, `web/src/map.ts`, `deploy/geo/` (gazetteer import script), `scripts/geo-import.sh`.

## Design

### Gazetteer
GeoNames `cities500` (CC BY 4.0, attribution in the footer and `NOTICE`) imported into a `gazetteer` table (`geonameid, name, admin1, country, lat, lon, population`) with a `point` column and a GiST index, by `scripts/geo-import.sh` from a local file (the file is downloaded by the operator, never by the app). `GeoReverse`: nearest gazetteer row within 50 km weighted by population (`ORDER BY distance / ln(population + 10)`), upsert into `places`, set `photos.place_id`. Idempotent. Photos without GPS: no-op.

### Map
`/map`: page with a MapLibre GL island (`maplibre-gl` pinned, vendored into `web/` at build, ~200 KB gz: loaded only on this page and excluded from the global JS budget, documented in `docs/performance.md` as a per-page exception). Tiles: configurable `YHOTOS_MAP_TILES` URL template (default: a self-hosted vector tile endpoint if present, else OpenFreeMap's public tiles with attribution), loaded by the browser. Data: `/map/tiles.json?bbox=&zoom=` returns server-side clusters (grid clustering at the requested zoom, max 500 features, each with count, centroid and a representative photo thumb hash), visibility-filtered, `Cache-Control: public, max-age=300` for anonymous, private otherwise. Clicking a cluster zooms; at zoom >= 15 individual photos link to `/p/`. No-JS fallback: a list of places with counts linking to `/search?place=`.

### Calendar
`/calendar` (years with counts and a cover), `/calendar/{year}` (months), `/calendar/{year}/{month}` (days, then the grid of that month's photos paginated), all from one query each using `taken_at`, visibility-filtered. Photos without `taken_at` fall back to `posted_at` and are marked.

### Tests
GeoReverse: known coordinates resolve to the expected city; outside 50 km yields null; idempotent. Map: bbox parsing rejects invalid/oversized boxes (fuzz small target), clusters respect visibility (restricted photo never in a cluster count), max features enforced. Calendar: counts match visibility; invalid dates 404; query-count tests.

## Acceptance

- [ ] `scripts/geo-import.sh` imports a provided `cities500.txt` in under 2 minutes; `GeoReverse` on the fixture photos with GPS produces places.
- [ ] `/map` works with JS disabled (list fallback) and with JS (clusters, zoom, photo links); Lighthouse on `/map` >= 90 (map library cost documented).
- [ ] `/calendar` pages pass snapshot tests; all visibility tests pass.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.4: the server never fetches tiles or gazetteer data at runtime; tile URL is config, rendered into the page, used by the browser only, CSP `connect-src`/`img-src` extended to that origin only when configured.
- 4.8: cluster counts and map data are visibility-filtered; restricted photos' locations never leak through counts (test).
- Allowed new dependencies: none in Rust beyond what exists; `maplibre-gl` in `web/`.

## Performance

- `/map/tiles.json` p99 < 30 ms at 10k photos (GiST index on `photos` point column added by migration); calendar pages one query.

## Not in this task

- Place editing in admin (part of bulk tools later).
- Reverse geocoding of videos (same path applies automatically if GPS exists).
