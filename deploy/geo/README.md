# Offline places and browser map

The operator downloads and unzips GeoNames cities500 (CC BY 4.0), then runs:

```sh
DATABASE_URL=... scripts/geo-import.sh /local/path/cities500.txt
```

Use an administrative connection. The script validates 19-column UTF-8 rows,
streams a bulk COPY into a temporary table, then upserts in one transaction.
It never downloads anything. Re-importing updates existing GeoNames identities.
Missing region codes remain empty text through `COPY FORCE_NOT_NULL (admin1)`.
Keep GeoNames attribution and NOTICE with deployments.

Run `python3 -m unittest discover -s deploy/geo -p 'test_*.py'` with `DATABASE_URL`
set after applying migrations to exercise the real import twice against an
isolated schema, including a city with no region code. Without `DATABASE_URL`
the PostgreSQL test is skipped; parser tests still run. CI's database job runs
the real import test after migrations.

`geo.reverse` jobs use the imported local table, within 50 km, weighted by city
population. Photos without both GPS coordinates are a no-op. Re-running jobs
reuses each city's place identity. Re-run jobs after updating the gazetteer if
existing place labels need refreshing.

`YHOTOS_MAP_TILES` accepts a vector tile template with `{z}`, `{x}` and `{y}`.
Set it to `/tiles/{z}/{x}/{y}.pbf` when a self-hosted endpoint is deployed, or a
fixed HTTPS origin. The default is OpenFreeMap's public
`https://tiles.openfreemap.org/planet/latest/{z}/{x}/{y}.pbf`. The server performs
no network probing or requests. The embedded style uses OpenMapTiles layers,
without remote fonts, sprites or styles. CSP allows only the configured origin
on /map; same-origin workers remain local. Custom providers must supply
OpenMapTiles-compatible data and their required attribution must be retained.

Run `npm ci && npm run build` in web/ to vendor the pinned MapLibre bundles,
worker, CSS and BSD license into embedded assets. MapLibre loads only on /map.
With MapLibre 6.15.0 the measured gzip sizes are 293,198 bytes for map.js,
149,791 bytes for its worker, and 10,191 bytes for map CSS (gzip default level).
The map JS total is 442,989 bytes, larger than the brief's ~200 KB estimate.
This page has a task-specific JS weight exception (main bundle plus worker);
the normal <50 KB compressed JS budget applies to all other public pages.
The maintainer must record this exception in docs/performance.md, which task
instructions otherwise prohibit editing. Record actual gzip sizes and a
Lighthouse result in the PR. The place list is always server-rendered and usable
without JavaScript or WebGL. Its links lead to `/places/{place_id}`, a
visibility-filtered photo grid with 60-photo pagination and a breadcrumb to /map. The server returns at most 484 grid cells below
zoom 15, and at most 500 distinct locations at zoom 15 and above. Photos with identical
coordinates share a count marker that opens a paginated photo list. These
photo-level groups cannot split further; lower-zoom clusters zoom towards that
level before offering the chooser. Zoom in to
see photos when a dense viewport exceeds that cap. Clustering never includes
invisible photos. Calendar counts and covers use the same visibility CTE.

Calendar dates use UTC, with posted_at substituted for absent taken_at. Month
and day grids are paginated at 60 photos, while all date counts remain complete.

Public map HTML and both JSON routes use `Cache-Tag: map,visibility`; calendar
pages use `calendar,visibility`; place pages use `places,place:<id>,visibility`.
All use `public, max-age=60, stale-while-revalidate=600` and `Vary: Cookie`;
cookie or authenticated responses are `private, no-store`. The shared visibility
tag covers aggregates spanning arbitrarily many albums without unbounded headers.

The edge purger is a missing prerequisite in this worktree: `CachePurger` and
its Cloudflare implementation are specified in task 020 but do not exist yet.
That mechanism must purge `visibility` (or all affected aggregate tags above)
on visibility/grant changes, photo/album moves, deletion/restoration, GPS/place
changes, and reverse-geocoding updates. Cache headers alone do not remove an
already cached location. Task 015 cannot verify mutation-triggered edge purges
until that API and its mutation integration are available.
