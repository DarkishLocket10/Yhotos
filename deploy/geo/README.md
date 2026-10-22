# Offline places and browser map

The operator downloads and unzips GeoNames cities500 (CC BY 4.0), then runs:

```sh
DATABASE_URL=... scripts/geo-import.sh /local/path/cities500.txt
```

Use an administrative connection. The script validates 19-column UTF-8 rows,
streams a bulk COPY into a temporary table, then upserts in one transaction.
It never downloads anything. Re-importing updates existing GeoNames identities.
Keep GeoNames attribution and NOTICE with deployments.

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
With MapLibre 6.15.0 the measured gzip sizes are 291,311 bytes for map.js,
149,791 bytes for its worker, and 10,191 bytes for map CSS (gzip default level).
The map JS total is 441,102 bytes, larger than the brief's ~200 KB estimate.
This page has a task-specific JS weight exception (main bundle plus worker);
the normal <50 KB compressed JS budget applies to all other public pages.
The maintainer must record this exception in docs/performance.md, which task
instructions otherwise prohibit editing. Record actual gzip sizes and a
Lighthouse result in the PR. The place list is always server-rendered and usable
without JavaScript or WebGL. The server returns at most 484 grid cells below
zoom 15, and at most 500 individual locations at zoom 15 and above. Zoom in to
see photos when a dense viewport exceeds that cap. Clustering never includes
invisible photos. Calendar counts and covers use the same visibility CTE.

Calendar dates use UTC, with posted_at substituted for absent taken_at. Month
and day grids are paginated at 60 photos, while all date counts remain complete.
