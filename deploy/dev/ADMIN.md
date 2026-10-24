# Admin library verification

The library is compiled only with `yhotos-http/admin` and uses the separate
admin listener. JSON actions require a completed admin MFA session, a CSRF
header, and the configured admin origin.

Install the pinned island build tool with `npm --prefix web ci` once.
Run `umask 027; scripts/check.sh` with the disposable development DATABASE_URL.
The script builds the islands and runs Rust, browser-island, dependency,
advisory, and secret checks. It now sets umask 027 itself.

## Page walkthrough

The populated HTTP integration test covers the dashboard, album tree, album grid,
photo editor, trash, flags, likes, users, groups, links, jobs, audit, and upload.
It asserts that all thirteen pages render and exclude credential material.

With Playwright installed in a development tool directory and a working Chrome
installation, capture all thirteen pages:

```sh
YHOTOS_ADMIN_WALKTHROUGH=1 \
YHOTOS_PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
cargo test -p yhotos-http --features admin --test auth \
  admin_library::every_page -- --nocapture
```

The browser receives only the disposable test session over stdin. No credential
files are written. Screenshots go under `deploy/dev/admin-walkthrough/`.
`YHOTOS_BROWSER_CHANNEL` optionally selects an installed browser channel.
The fixture login handler exists only in the test harness and binds to loopback.
It is never part of either product router.

Screenshot capture in the Codex sandbox was blocked: native headless Chrome
could not launch, and the connected browser approval gate declined the local
fixture origin. The ordinary HTTP page assertions run without a browser.

## Edge purges

Set both `YHOTOS_CLOUDFLARE_ZONE` (32 hex characters) and
`YHOTOS_CLOUDFLARE_API_TOKEN` to enable Cloudflare cache-tag purges. Leave both
unset for the no-op implementation. The admin runtime needs `/usr/bin/curl`
and its normal trusted CA certificates. No additional Rust HTTP client is used.
The token is provided over an anonymous stdin pipe, never argv or logs. Curl
uses a fixed HTTPS API origin, no redirects or proxies, an empty environment,
a five-second deadline, and a bounded JSON response.

Mutations invalidate local file/session/share snapshots before responding and
commit an outbox batch with their audit row. PostgreSQL commit notifications
advance public-process cache generations too. The admin background worker drains
and retries the durable outbox, keeping network latency out of the request path.
Ingest publication also enqueues purges after derivatives become ready.

Copy creates a new photo identity that shares immutable original and derivative
bytes. It copies metadata, tags, grants and rendition references; likes, flags
and events remain attached to the source identity. Replacement retains the photo
identity, increments its ingest generation, and retains the old original in trash
for thirty days. Purge queues GC. GC checks every photo reference (including
trash) and retained original root under the storage publication lock. A 24-hour
grace period protects uploads that have published bytes but have not yet committed.

## Measured budgets

Run `cargo bench -p yhotos-bench --bench admin -- --noplot` against the disposable
local development database. The benchmark rolls fixture transactions back.
Criterion point estimates on this machine, in the initial implementation and after
optimizing SQL projection and copy statements:

| Benchmark | Before | After |
| --- | ---: | ---: |
| `admin/album_1000_query_and_render` | 87.671 ms | 32.309 ms |
| `admin/bulk_1000_move` | 273.31 ms | 326.04 ms |
| `admin/bulk_1000_visibility` | 298.34 ms | 294.90 ms |
| `admin/bulk_1000_copy` | 2,129.3 ms | 432.65 ms |

The final album template alone renders in 0.567 ms. The final measurements include
full audit snapshots and purge enqueueing for mutations; edge network time is
asynchronous. There was no pre-task admin-library implementation to benchmark.

## Merge revalidation

The merge with the lightbox, map/calendar, upload and share-expiry changes was
checked on macOS on 2026-10-22. `cargo sqlx migrate run` and
`cargo sqlx prepare --workspace -- --all-targets --all-features` passed against
the supplied disposable database, and `npm ci --prefix web` passed.

`RUST_TEST_THREADS=1 scripts/check.sh` passed, including the dependency, advisory
and secret scans. The initial parallel run hit the existing queue test's
200 ms shutdown deadline; that test passed in isolation and in the serialized
full run. No job implementation or deadline was changed.

All four conflicted snapshots were restored from main, regenerated with
`INSTA_UPDATE=new`, reviewed and accepted. Their only markup differences are
the sanitized Markdown description container and its paragraph whitespace.

An idle Criterion recheck using 1,000 photos produced these point estimates:

| Benchmark | Merge recheck |
| --- | ---: |
| `admin/album_1000_render` | 0.579 ms |
| `admin/album_1000_query_and_render` | 30.300 ms |
| `admin/bulk_1000_move` | 297.06 ms |
| `admin/bulk_1000_visibility` | 306.77 ms |
| `admin/bulk_1000_copy` | 376.34 ms |

The historical before/after optimization measurements above remain separate
from this merge recheck. Screenshot capture remains blocked by the local
browser origin approval gate.
