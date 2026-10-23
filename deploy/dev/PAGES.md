# Public page checks (task 011)

The fixtures are synthetic landscape drawings generated locally with the browser
Canvas API. No photographs or network-downloaded fixtures are included.

Start the isolated Compose database, build the islands, and serve sixty photos:

```sh
COMPOSE_PROJECT_NAME=yhotos011 POSTGRES_PORT=55443 source deploy/dev/start.sh
cargo sqlx migrate run --source crates/yhotos-db/migrations
npm --prefix web ci --no-audit
npm --prefix web run build
# Requires an existing Playwright installation and Chromium. Set PLAYWRIGHT_MODULE
# to its module path and CHROME_PATH to the browser binary when needed.
node deploy/dev/fixtures/generate.mjs
cargo run -p yhotos-http --example pages_fixture
```

The example binds only `127.0.0.1:18011`, permits plaintext database connections
only on loopback, uses a temporary content store, and seeds the dedicated
`gallery-fixture` album. Re-running refreshes its photo and derivative rows and
reconstructs its files, including fixtures seeded before task 011's review.
It also seeds `/a/mobile-fixture` with an eager child-album cover and
`/a/single-fixture` with a single-photo grid, both using the same derivative sizes.
The generator draws sixty visibly distinct compositions, each at 320, 640, 1280 and
2048 pixels plus a separate 320-pixel square crop. The server rejects repeated
content hashes across the 300 files, so different photos require independent
image transfers. Generated image data
is ignored by Git; the three older single-composition JPEGs are not used.
It uses the same public router, templates, middleware and file-serving code as
the server. Its session key is generated at startup. No fixture credentials are
checked in. Existing development albums are left alone.

With the fixture server running (reports stay local and are ignored by Git):

```sh
mkdir -p deploy/dev/reports
node deploy/dev/fixtures/check-mobile.mjs
npx lighthouse http://127.0.0.1:18011/a/gallery-fixture \
  --chrome-flags='--headless --no-sandbox' --only-categories=performance \
  --output=html --output=json --output-path=deploy/dev/reports/011-album-mobile
npx lighthouse http://127.0.0.1:18011/a/gallery-fixture \
  --chrome-flags='--headless --no-sandbox' --preset=desktop \
  --only-categories=performance --output=html --output=json \
  --output-path=deploy/dev/reports/011-album-desktop
cargo bench -p yhotos-bench --bench pages
scripts/check.sh
```

Lighthouse requires an installed Chromium. `CHROME_PATH` can select an existing
binary. On sandboxed WSL, launch Chromium through Playwright with an explicit
remote debugging port and give Lighthouse `--port=PORT` to reuse it. This avoids
chrome-launcher attempting to create its profile on a Windows filesystem.

The recorded JavaScript-disabled check uses Chromium at 390×844 (2× pixel ratio)
and 1280×900. With JavaScript disabled, open the album, inspect its complete grid,
submit the sort form, follow a photo link and its next-photo link, and open the
native download disclosure. Check that each image has dimensions, there is no
horizontal overflow or CSP error, and mobile grid images choose the 320w full-frame
JPEG (never the square crop or a representation wider than 640 pixels).
Record results in the PR description, including performance score, LCP, CLS,
INP, total transfer bytes and request count for each profile. Lighthouse navigation
runs do not measure INP; use a separate interaction trace or report it as unavailable.
Check the network audit for distinct image URLs rather than a shared image fetch.
Reports and screenshots must remain local under the ignored `reports/` directory.
These are local lab measurements, not measurements on the production tower.

`scripts/check.sh` also runs task 021's standalone video source-selection browser
tests, without the fixture server. Install the Chromium version supplied by the
pinned Playwright package (`cd web && npx playwright install chromium`), or set
`CHROME_PATH` to an existing Chromium executable. In a restricted workspace, set
`PLAYWRIGHT_BROWSERS_PATH` to an absolute path under `target/` for both installation
and checks. If the home directory is read-only, also point `XDG_CONFIG_HOME` and
`XDG_CACHE_HOME` at writable directories. The tests generate a small AV1 clip
locally with the pinned static ffmpeg extracted by `deploy/dev/video-tools.sh`,
and verify actual AV1 selection
and playback through the photo template and lightbox island. CI installs Chromium
and runs the same regression tests.

The mobile network regression check opens all three fixture albums in headless
Chromium at 390×844 with DPR 2 and JavaScript disabled. It records actual image
responses and rejects requests for any candidate above 640w, including child
covers and single-photo grids whose slots exceed 320 CSS pixels. The report is
`deploy/dev/reports/011-mobile-network.json`. Narrow-screen source sets are
capped across JPEG, AVIF, JXL and HDR; desktop retains all available widths and
`sizes` continues to describe the actual slot. Format coverage is also checked
by the Rust page tests. Set `PLAYWRIGHT_MODULE` and `CHROME_PATH` as above.

On hosts with many CPU cores, simultaneous SQLx test migrations can exhaust the
development PostgreSQL lock table. Use `RUST_TEST_THREADS=4 scripts/check.sh` to
bound test concurrency without changing database settings.


The fixture also seeds `/map` with sixty photos at one coordinate. With JavaScript
disabled, follow its place link to the shared photo grid and the breadcrumb back
to the map. With JavaScript enabled, the count marker opens a paginated list of
all photos at that point. Open a photo and use Back to check history restoration;
`web/tests/map.mjs` also exercises repeated persisted pagehide/pageshow cycles.
