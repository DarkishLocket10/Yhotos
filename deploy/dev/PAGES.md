# Public page checks (task 011)

The fixtures are synthetic landscape drawings generated locally with the browser
Canvas API. No photographs or network-downloaded fixtures are included.

Start the isolated Compose database, build the islands, and serve sixty photos:

```sh
COMPOSE_PROJECT_NAME=yhotos011 POSTGRES_PORT=55443 source deploy/dev/start.sh
cargo sqlx migrate run --source crates/yhotos-db/migrations
npm --prefix web ci --no-audit
npm --prefix web run build
cargo run -p yhotos-http --example pages_fixture
```

The example binds only `127.0.0.1:18011`, permits plaintext database connections
only on loopback, uses a temporary content store, and seeds the dedicated
`gallery-fixture` album only when empty. Re-running reconstructs its files.
It uses the same public router, templates, middleware and file-serving code as
the server. Its session key is generated at startup. No fixture credentials are
checked in. Existing development albums are left alone.

With the fixture server running:

```sh
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
horizontal overflow or CSP error, and mobile grid images choose the 640w JPEG.
Screenshots and the check results are in `reports/011-album-nojs-*` and
`reports/011-nojs-check.json`. Lighthouse reports include the tested URL,
browser version, throttling and run time; these are local lab measurements,
not measurements on the production tower.
