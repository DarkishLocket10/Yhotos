# Architecture

This document is binding for implementation. `docs/scope.md` says what; this says how. If something here turns out to be wrong, the fix is a change to this document first, then code.

## 1. Shape of the system

Four processes, one database, one filesystem tree. All run as containers on one host (tower) but nothing assumes that.

```
                  Cloudflare edge (cache, WAF, tunnel)
                               |
                        yhotos-server (public listener)
                               |
   Tailscale ----- yhotos-server (admin listener, separate container, admin feature on)
                               |
                 +-------------+-------------+
                 |                           |
           PostgreSQL 16                Filesystem
        (data, jobs, vectors)     originals / derivatives / video
                 |                           |
        +--------+---------+                 |
        |                  |                 |
  yhotos-worker       yhotos-ml  ------------+
 (CPU: derivatives,  (GPU: embeddings,
  video, zips, gc)    faces, OCR, tags)
```

- **yhotos-server**: one binary, two deployments. The public container is built without the `admin` Cargo feature, so admin routes, upload handlers, and the theme editor do not exist in it at all. The admin container has the feature on and binds only to the Tailscale IP.
- **yhotos-worker**: consumes the `jobs` table. Runs decoders in a sandboxed child process (`yhotos-codec-host`). CPU bound.
- **yhotos-ml**: consumes ML jobs. Separate container with the GPU, no outbound network. May be Python if a model only ships that way; the interface is the `jobs` table and Postgres, nothing else.
- **PostgreSQL**: the only shared state. Also the job queue.
- **Filesystem**: originals, derivatives, video renditions, zips in progress. Never served by the server from arbitrary paths; only through the storage layer.

## 2. Repository layout

```
Cargo.toml                workspace
crates/
  yhotos-core/            domain types, ids, errors, permissions, slugs, time. No IO.
  yhotos-config/          typed configuration loaded from env (and optional TOML).
  yhotos-db/              sqlx, migrations/, repositories. The only crate that writes SQL.
  yhotos-storage/         content-addressed file store, safe paths, streaming IO.
  yhotos-auth/            passwords, sessions, TOTP, WebAuthn, share tokens, rate limit keys.
  yhotos-codec/           the ONLY crate with unsafe: libvips, libraw/rawler, libheif FFI.
  yhotos-codec-host/      binary: sandboxed child that runs yhotos-codec jobs over a pipe.
  yhotos-pipeline/        ingest orchestration: validate, hash, metadata, variant plan, enqueue.
  yhotos-jobs/            job queue client and worker loop (Postgres SKIP LOCKED + NOTIFY).
  yhotos-http/            axum routers, handlers, templates, middleware, static assets.
  yhotos-server/          binary: wires config, db, http, listeners.
  yhotos-worker/          binary: job consumer for CPU jobs.
  yhotos-migrate-piwigo/  binary: one-shot migration from Piwigo's MariaDB and upload dir.
  yhotos-bench/           criterion benchmarks and load-test fixtures.
web/                      TypeScript islands and CSS; built by esbuild into crates/yhotos-http/assets/
ml/                       yhotos-ml service (Rust or Python), own Dockerfile.
fixtures/                 small test images and malicious samples (large ones are fetched from tower, see docs/process.md).
fuzz/                     cargo-fuzz targets.
deploy/                   docker compose, Dockerfiles, Cloudflare and Tailscale notes.
docs/                     these documents.
tasks/                    briefs.
scripts/                  check.sh, agent-run.sh, agent-review.sh, fixtures-sync.sh.
```

Dependency direction: `core <- config, db, storage, auth, codec, pipeline, jobs <- http <- server`. Nothing depends on `http` except `server`. `codec` depends on nothing in the workspace except `core`.

## 3. Conventions

- Rust 2024 edition, stable toolchain pinned in `rust-toolchain.toml`.
- Ids: `Uuid` v7 (time-ordered) for all rows, newtype wrapped (`PhotoId`, `AlbumId`, `UserId`...). Never expose integer ids.
- Public URLs use slugs for albums and a short id for photos (base32 of the low 60 bits of the UUID, lowercase, no padding), never raw UUIDs.
- Time: `jiff` or `time` with UTC everywhere; convert at the template edge.
- Errors: each crate has an `Error` enum via `thiserror`. `yhotos-http` maps them to responses in exactly one place (`error.rs`). Public responses are generic; details go to logs with a request id.
- Logging: `tracing` with JSON output in production, request id per request, no bodies, no tokens.
- Config: `yhotos-config` reads `YHOTOS_*` environment variables, validates once at startup, and the rest of the code gets a `&'static Config`. Secrets are `SecretString` (`secrecy` crate) and never `Debug`-printed.
- Async: `tokio` multi-thread runtime. Blocking work (`libvips`, file hashing) goes through `spawn_blocking` or the worker, never on the async executor.
- Templates: `askama` (compile-time, typed). Each template gets a `Nonce` for CSP.
- Feature flags: `admin` (admin routes and upload), `ml-client` (search endpoints that need embeddings). Public image: no `admin`.

## 4. Data model

PostgreSQL. All tables have `id uuid primary key`, `created_at timestamptz not null default now()`, and `updated_at` where rows change. Soft delete only where the scope needs a trash.

```
users            id, username (citext unique), password_hash, must_change_password bool,
                 is_admin bool, disabled_at, last_login_at
user_mfa         user_id, kind (totp|passkey|email), secret_enc, credential_id, public_key,
                 sign_count, label, enabled_at
sessions         id, user_id, token_hash, created_at, last_seen_at, expires_at, ip_hash, ua_class
groups           id, name
user_groups      user_id, group_id

albums           id, parent_id null, slug (unique within parent), title, description,
                 sort_order int, cover_photo_id null, visibility (public|restricted),
                 manual_order bool, deleted_at
album_grants     album_id, principal_kind (user|group), principal_id
photos           id, short_id (unique), album_id, title, description, taken_at, posted_at,
                 sort_order int, visibility (inherit|public|restricted), original_hash,
                 original_name, original_bytes, mime, kind (image|raw|video), width, height,
                 orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens,
                 focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id null,
                 dominant_colors int[], placeholder (thumbhash bytea), deleted_at
photo_grants     photo_id, principal_kind, principal_id
derivatives      photo_id, variant (thumb|small|medium|large|xlarge|...), format (avif|jxl|jpeg),
                 hdr bool, hash, width, height, bytes
video_renditions photo_id, codec (h264|av1), hash, width, height, bytes, duration_ms, hls bool
tags             id, slug unique, name, source (manual|auto), hidden bool
photo_tags       photo_id, tag_id, confidence null, source
likes            user_id null, share_link_id null, photo_id, created_at   (exactly one of user/link)
flags            id, user_id null, share_link_id null, photo_id, created_at, resolved_at,
                 resolution (replaced|reedited|removed|visibility|dismissed) null
share_links      id, token_hash, target_kind (album|photo), target_id, passphrase_hash null,
                 expires_at null, allow_view, allow_download, allow_like, allow_flag,
                 revoked_at, created_by
share_link_opens share_link_id, opened_at, visitor_hash, country, city
faces            id, photo_id, bbox, embedding vector(512), person_id null, quality
persons          id, name, hidden bool
embeddings       photo_id primary key, model, vector vector(768)
ocr_text         photo_id, text, tsv tsvector
places           id, name, admin1, country, lat, lon
jobs             id, kind, payload jsonb, priority, run_after, attempts, max_attempts,
                 locked_by, locked_at, finished_at, error, created_at
events           id, at, kind, photo_id null, album_id null, share_link_id null,
                 visitor_hash, country, city, device_class, referrer_host, extra jsonb
events_daily     day, kind, target_kind, target_id, count     (rollup, written by worker)
audit_log        id, at, user_id, action, target_kind, target_id, detail jsonb
settings         key primary key, value jsonb, updated_at                (theme tokens etc.)
legacy_redirects id, pattern, target_kind, target_id
```

Indexes that matter: `photos (album_id, sort_order)`, `photos (taken_at)`, `derivatives (photo_id, variant, format, hdr)`, `jobs (kind, run_after) where finished_at is null`, `embeddings using hnsw (vector vector_cosine_ops)`, `faces using hnsw`, `ocr_text using gin (tsv)`, `events (at)`, `share_links (token_hash)`.

Visibility resolution: a photo is visible to a principal if (photo.visibility = public) or (photo.visibility = inherit and album is visible) or (restricted and there is a grant for the user or one of their groups). Admin sees everything. Share links bypass this for their target only. This logic lives in `yhotos-core::access` as pure functions and is tested exhaustively; `yhotos-db` provides the queries that fetch the inputs.

## 5. Storage

Root from config, e.g. `/data`. Layout:

```
/data/originals/<h0h1>/<h2h3>/<hash>            original bytes, immutable
/data/derivatives/<h0h1>/<h2h3>/<hash>.<variant>.<fmt>[.hdr]
/data/video/<h0h1>/<h2h3>/<hash>.<codec>.mp4
/data/tmp/<uuid>                                 uploads in progress, zips being built
```

`hash` is lowercase hex BLAKE3 of the file bytes. `yhotos-storage` exposes `put_stream`, `open`, `exists`, `remove`, and `safe_join`; nothing else touches the tree. `safe_join` rejects `..`, absolute components, symlinks, and anything not matching the hash pattern.

Migration sources use `yhotos-storage::SourceTree`, a read-only root directory
file descriptor, and `open_file(relative) -> SourceFile`. Relative names must be
UTF-8, nonempty, at most 4096 bytes, and contain no NUL, backslash, empty, `.` or
`..` components. Absolute paths are rejected. Linux uses `openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV`;
macOS walks directory descriptors with `openat`, `O_DIRECTORY` and `O_NOFOLLOW`.
Final files are opened `O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK`, then
`fstat` must identify a regular file. Nonblocking open prevents a FIFO from
hanging before rejection. `SourceFile` holds the read-only file and observed
length; callers stream that handle without reopening a pathname. No source
operation creates entries or opens files for writing. The mount remains
read-only during migration; macOS descriptor walking does not prohibit mount
crossings, so Linux is required for final migration.

Serving: derivatives and originals are opened by hash, never by user-supplied path. The server sends them with `ETag: "<hash>"`, `Cache-Control: public, max-age=31536000, immutable`, `Accept-Ranges`, and `Content-Type` from the derivatives row, not from the file. Range requests are honored for originals and video.

Cleanup: a `gc` job removes derivatives not referenced by any row and tmp files older than 24 hours.

## 6. Image pipeline

Upload (admin listener only):
1. Chunked, resumable upload into `/data/tmp/<uuid>` (tus-style protocol, 8 MB chunks, total size cap from config, default 2 GB).
2. On completion: sniff magic bytes (`infer`), reject anything not on the allow list, enforce dimension caps before decode (read headers only), compute BLAKE3 while copying to `originals/`, dedupe by hash.
3. Insert `photos` row with `kind` and `original_*` fields, enqueue `ingest` job.

`ingest` job (worker):
1. Spawn `yhotos-codec-host` with the original path, a variant plan, and an output directory. The child runs under seccomp (no network, no exec), with RLIMIT_AS, CPU time limit, and a wall clock timeout. It writes derivatives and a JSON report (dimensions, ICC, EXIF subset, HDR info, thumbhash, dominant colors) to the output directory. Crashes or timeouts are recorded on the job and the photo is marked `failed`.
2. Worker verifies outputs (file sizes, hashes), moves them into `derivatives/`, writes `derivatives` rows and photo metadata in one transaction, enqueues `ml.embed`, `ml.faces`, `ml.ocr`, `ml.tags`, `geo.reverse` as applicable.

Variant plan (long edge in px, from config, defaults): thumb 320 (square crop also), small 640, medium 1280, large 2048, xlarge 3200, plus 2x of thumb/small/medium. Formats: AVIF and JXL always, JPEG fallback for large and below (large JPEG is the Open Graph image and the fallback for browsers without AVIF or JXL). HDR: if the source has a gain map, HDR variants carry it (JPEG with gain map, AVIF with gain map where encoder supports it) and the SDR variant is the base image. Encoding quality per variant is fixed in config and verified by a fidelity test (SSIM against a reference decode of the original) that must stay above the threshold in `docs/performance.md`.

Color: decode with the embedded ICC to a 16-bit linear working space, resize there, encode with the profile embedded or converted to sRGB only for the JPEG fallback below medium. Never strip the profile from AVIF/JXL. Orientation is applied at decode; derivatives are always orientation 1.

RAW: decode the embedded preview (full size when present) via `rawler`; if none, demosaic with camera defaults. `kind = raw` is shown in the UI; the original is the RAW file.

Video: `ffmpeg` in the same sandboxed child model (separate `yhotos-video-host` entrypoint, same crate). Renditions: H.264 MP4 at 1080p max and AV1 MP4 at source res up to 2160p, plus poster and scrub sheet. HLS only above a configured duration.

## 7. Jobs

Table `jobs`, Postgres only. Enqueue is an insert plus `NOTIFY jobs`. Workers `LISTEN jobs` and also poll every few seconds. Claim:

```sql
update jobs set locked_by = $1, locked_at = now(), attempts = attempts + 1
where id = (select id from jobs where finished_at is null and locked_at is null
            and run_after <= now() and kind = any($2)
            order by priority desc, run_after limit 1 for update skip locked)
returning *;
```

Stale locks (older than the kind's timeout) are released by a sweeper. Failed jobs retry with exponential backoff up to `max_attempts`, then stay visible in the admin job view with their error.

## 8. HTTP

Public router (`yhotos-http::public`):
```
GET  /                         home: top-level albums
GET  /a/{slug...}              album page (nested slugs), paginated grid
GET  /p/{short_id}             photo page (lightbox deep link, OG tags)
GET  /i/{hash}/{variant}.{fmt} derivative
GET  /o/{hash}/{filename}      original (filename is display only, ignored for lookup)
GET  /v/{hash}/{codec}.mp4     video rendition
GET  /s/{token}                share link landing; POST /s/{token}/unlock for passphrase
GET  /search?q=&...            search results
GET  /map, /map/tiles.json     map view data (photos with gps, visibility-filtered)
GET  /calendar/{year}[/{month}]
GET  /api/v1/...               JSON for islands (album pages, search, likes, flags)
POST /api/v1/photos/{id}/like, /flag       (session or share-link scoped)
GET  /u/liked                  personal liked tab
GET  /login, POST /login, POST /logout, /u/security (mfa enrollment)
GET  /zip?album=|photos=        streamed zip, rate limited, visibility-filtered
GET  /healthz                  liveness (no details)
GET  /legacy/*                 Piwigo URL redirects
```
Admin router (feature `admin`, admin listener only): `/admin/...` pages and `/admin/api/...`, plus `/admin/upload` (tus), `/admin/jobs`, `/admin/analytics`, `/admin/theme`, `/metrics` (Prometheus, Tailscale only).

Middleware order: request id, tracing, body limit, timeout, rate limit (per IP bucket, separate budgets for cheap/expensive routes), security headers, CSRF (for state-changing non-API form posts), session loader, CSP nonce.

Security headers on every response: `Content-Security-Policy` (default-src 'none'; img-src 'self'; script-src 'nonce-...'; style-src 'self' 'nonce-...'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'), `Strict-Transport-Security` (preload), `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`, `Permissions-Policy` denying everything unused, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin` (except derivatives, which are `cross-origin` so OG previews and the portfolio site can embed them).

Caching: derivatives/originals immutable as above. HTML for public pages: `Cache-Control: public, max-age=60, stale-while-revalidate=600` with `Cache-Tag: album:<id>` so an admin change purges via Cloudflare API. Anything behind a session: `private, no-store`. `Vary: Accept` on image negotiation endpoints only (format is in the URL, so negotiation happens in the HTML `<picture>` element, not at the image URL; this keeps edge caching simple).

## 9. Frontend

Server-rendered `askama` templates. Islands in `web/src/*.ts`, built by `esbuild` to ES2022, one file per island, loaded with `type=module` and the CSP nonce, total budget in `docs/performance.md`. No framework. Islands: lightbox, upload (admin), like/flag buttons, search box, map (MapLibre, self-hosted tiles or a configurable tile URL), theme editor (admin).

Images in HTML:
```html
<picture>
  <source type="image/jxl"  srcset="/i/h/small.jxl 640w, /i/h/medium.jxl 1280w, ..." sizes="...">
  <source type="image/avif" srcset="...">
  <img src="/i/h/medium.jpeg" srcset="..." sizes="..." width height loading="lazy" decoding="async"
       style="background: thumbhash-derived gradient" alt="...">
</picture>
```
`width`/`height` always set, so layout never shifts. Thumbhash is decoded to a CSS background by a tiny inline-free island, or server-side to a data URI for the first row.

Navigation: same-origin prefetch on hover/touchstart, View Transitions API where supported, otherwise full loads. No client-side router.

## 10. Theming

Layer 1: `settings.theme_tokens` JSON, rendered into `/theme.css` (cached, versioned by hash) as CSS custom properties. Layer 2: `settings.theme_options` consumed by templates. Layer 3: templates in `crates/yhotos-http/templates/`, compiled; a theme is a directory of templates selected at build time via feature or env, defaulting to `default/`. Layer 4: `settings.custom_css` served as `/custom.css` with its own hash, `settings.custom_head_html` restricted to a sanitized allow list (`ammonia`), scripts only from `/static/` with nonces. No remote assets by default; the theme editor warns if a CSS `url()` points off-origin and the CSP blocks it anyway.

## 11. Search

`search?q=` parses into: free text (semantic + OCR + title), and filters (`camera:`, `lens:`, `tag:`, `place:`, `year:`, `color:`, `is:raw`, `is:hdr`, `is:video`). Semantic: embed the query via `yhotos-ml` (HTTP to the ML container on the private network, 20 ms budget, falls back to text-only if unavailable), then `embeddings <=> $1` with pgvector HNSW, joined with visibility filtering. Results merge by reciprocal rank fusion. Person search exists only on the admin router.

## 12. Analytics

`events` rows are written by a lightweight middleware on public page and image responses (sampled for images at the edge, since most image hits never reach origin; Cloudflare's own analytics fill the gap and are not depended on). `visitor_hash = blake3(ip || ua_class || daily_salt)`, salt rotates daily and is not stored. GeoIP from a local MMDB file. Nightly `rollup` job fills `events_daily`; raw events older than 90 days are deleted.

## 13. Migration from Piwigo

`yhotos-migrate-piwigo` reads MariaDB (read-only connection) and the upload directory, writes albums, photos, tags, and legacy redirects, and enqueues ingest for every file. Idempotent: re-running updates rather than duplicates (keyed on original hash). A `--dry-run` reports counts and problems without writing.

## 14. Deployment

`deploy/compose.yml`: `server-public`, `server-admin`, `worker`, `ml`, with Postgres reused from the host. Images are built in CI, signed with cosign, and carry an SBOM. `server-public` runs read-only filesystem, non-root, no capabilities, `/data` mounted read-only. `worker` and `server-admin` mount `/data` read-write. `ml` has `network_mode` restricted to the internal network and the GPU reserved. Cloudflare Tunnel (`cloudflared`) points at `server-public` only.

## 15. Testing

- Unit tests everywhere, no network, no filesystem outside `tempfile`.
- Integration tests in `yhotos-db` and `yhotos-http` run against a real Postgres (`DATABASE_URL` in CI, `testcontainers` locally).
- Fidelity tests in `yhotos-codec` compare derivatives to a reference decode (SSIM, delta E on a color chart fixture).
- Fuzz targets: upload sniffing, EXIF parser wrapper, share token parsing, search query parser, tus protocol, safe_join.
- Benchmarks in `yhotos-bench` (criterion) and load tests (`oha`) driven by `scripts/bench.sh` against a compose stack with fixtures.
