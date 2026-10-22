---
id: 002
slug: core-config-server
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [001]
crates: [yhotos-core, yhotos-config, yhotos-http, yhotos-server]
---

# 002 Core types, configuration, and the server skeleton

## Goal

`yhotos-server` starts, binds its public listener (and the admin listener when the `admin` feature is on), serves `/healthz`, applies the full middleware stack and security headers from `docs/architecture.md` section 8, renders one askama template for `/` with a CSP nonce, and shuts down cleanly. `yhotos-core` has the id newtypes, short id encoding, slug rules, and the access function with its full test matrix. `yhotos-config` loads and validates configuration from `YHOTOS_*` environment variables.

## Read first

- `AGENTS.md`
- `docs/architecture.md` sections 3, 4 (visibility resolution), 8, 9
- `docs/threat-model.md` sections 4.2 (sessions, only the cookie attributes), 4.3, 4.4
- `docs/performance.md` section 1 (serving budgets) and 3

## Scope

`crates/yhotos-core/`, `crates/yhotos-config/`, `crates/yhotos-http/` (router, middleware, error mapping, templates, static assets wiring), `crates/yhotos-server/`, `crates/yhotos-bench/benches/access.rs`. Workspace `Cargo.toml` only to add dependencies that the design lists.

## Design

### yhotos-core
- `ids.rs`: `macro_rules!` generating `PhotoId`, `AlbumId`, `UserId`, `GroupId`, `ShareLinkId`, `TagId`, `JobId`, `SessionId`, each a `#[repr(transparent)]` newtype over `Uuid`, with `new()` (v7), `Display` as hyphenated UUID, `FromStr`, `serde` as string, `sqlx::Type` (feature `sqlx`, default off).
- `short_id.rs`: `ShortId` is the low 60 bits of a `PhotoId` encoded as 12 chars of lowercase base32 (alphabet `abcdefghijklmnopqrstuvwxyz234567`, no padding). `ShortId::from_photo_id`, `Display`, `FromStr` that rejects wrong length and alphabet. Note in docs that short ids are lookup keys, not secrets, and are not reversible to the full UUID (the database stores the mapping).
- `slug.rs`: `Slug::new(&str) -> Result<Slug, SlugError>`: lowercase ASCII letters, digits, and single hyphens, 1 to 80 chars, no leading or trailing hyphen. `Slug::from_title(&str)` transliterates (`deunicode`), collapses non-alphanumerics to hyphens, truncates at a word boundary under 80, and never returns empty (falls back to `untitled`).
- `access.rs`: pure functions.
  ```rust
  pub enum Visibility { Public, Restricted, Inherit }
  pub enum Principal<'a> { Anonymous, User { id: UserId, groups: &'a [GroupId], is_admin: bool }, ShareLink { target: ShareTarget } }
  pub enum ShareTarget { Album(AlbumId), Photo(PhotoId) }
  pub struct AlbumAccess<'a> { pub id: AlbumId, pub visibility: Visibility, pub grants: &'a [Grant], pub ancestors: &'a [AlbumAccess<'a>] }
  pub struct PhotoAccess<'a> { pub id: PhotoId, pub album: &'a AlbumAccess<'a>, pub visibility: Visibility, pub grants: &'a [Grant] }
  pub enum Grant { User(UserId), Group(GroupId) }
  pub fn can_view_album(p: &Principal, a: &AlbumAccess) -> bool
  pub fn can_view_photo(p: &Principal, ph: &PhotoAccess) -> bool
  ```
  Rules: admin sees all. `Public` is visible to everyone. `Restricted` is visible to a user with a matching user or group grant. `Inherit` on a photo defers to its album; `Inherit` on an album defers to its parent, and a top-level `Inherit` album is `Public`. A restricted ancestor hides every descendant regardless of the descendant's own setting (restriction only narrows). A share link sees exactly its target: the album (and its photos and descendants, subject to the same narrowing by ancestors above the target) or the single photo. Property tests (`proptest`) for the invariants "restricting an ancestor never widens access" and "admin always true", plus a table-driven test covering every combination of (principal kind) x (visibility) x (grant present or not) x (ancestor restricted or not).
- `error.rs`: `CoreError` with variants for slug and short id parsing.
- No IO, no async, no tokio.

### yhotos-config
- `Config` struct, loaded by `Config::from_env()`, using `figment` with the `Env` provider prefixed `YHOTOS_` and nested keys by `__`. Fields (with defaults where sensible):
  - `public_listen: SocketAddr` (default `0.0.0.0:8080`)
  - `admin_listen: Option<SocketAddr>` (required when `admin` feature is on; must not be `0.0.0.0` or `::`, must be a specific address, validated)
  - `database_url: SecretString`
  - `data_dir: PathBuf` (must exist and be a directory at startup)
  - `public_base_url: Url` (https only)
  - `session_key: SecretString` (at least 32 bytes after base64 decode)
  - `trust_proxy: bool` (default false; when true, client IP comes from `CF-Connecting-IP`, else from the socket)
  - `rate_limits: { cheap_per_min: u32 (600), expensive_per_min: u32 (30), auth_per_min: u32 (10) }`
  - `upload: { max_bytes: u64 (2 GiB), chunk_bytes: u32 (8 MiB) }`
  - `log_format: Json | Pretty` (default Json)
- Validation errors list every problem at once, not only the first. `Config` implements `Debug` with secrets redacted (via `secrecy`).

### yhotos-http
- `lib.rs` exposes `pub fn public_router(state: AppState) -> Router` and, under `#[cfg(feature = "admin")]`, `pub fn admin_router(state: AppState) -> Router`. `AppState` holds `&'static Config` and, for now, nothing else (database arrives in task 003 and is added then).
- `middleware/`: `request_id` (ULID or UUIDv7 in `x-request-id`, generated server-side, never taken from the client), `trace` (tower-http `TraceLayer` with the request id, method, path, status, latency; no headers or bodies logged), `body_limit` (1 MiB on public except routes that opt out), `timeout` (10 s public), `rate_limit` (token bucket keyed by client IP with the three budgets from config; in-memory `dashmap` with periodic sweep; returns 429 with `Retry-After`), `security_headers` (exactly the set in architecture section 8, with the CSP nonce per request stored in request extensions), `csrf` (skeleton only: generates and validates a double-submit token for form POSTs; no forms exist yet besides a test one).
- `error.rs`: `HttpError` enum with `NotFound`, `Forbidden` (rendered identically to `NotFound` for public routes), `BadRequest`, `RateLimited`, `Internal(anyhow::Error)`. One `IntoResponse` impl. Internal errors log the full chain with the request id and return a generic page/JSON with only the request id.
- `templates/default/base.html` and `home.html` (askama). `base.html` sets `<meta charset>`, viewport, title, a `<link rel=stylesheet href="/static/app.css">`, and includes the nonce on any `<script>`/`<style>` tag. `home.html` renders a placeholder heading. Static files served from `crates/yhotos-http/assets/` embedded with `rust-embed` (so the binary is self-contained), with `Cache-Control: public, max-age=31536000, immutable` and a content hash in the URL (`/static/app.{hash}.css` with a helper that templates call).
- Routes: `GET /` (home), `GET /healthz` (`200 ok`, text/plain, no caching), `GET /static/*`. Admin router: `GET /admin/healthz` only.
- Every response gets the security headers, including errors and static files (CORP `cross-origin` only on `/static/`, `/i/`, `/o/`, `/v/` paths; the latter three do not exist yet but the middleware takes a path predicate).

### yhotos-server
- `main.rs`: install `mimalloc` as global allocator; init tracing (JSON or pretty from config); load config; build routers; bind listeners with `tokio::net::TcpListener`; graceful shutdown on SIGTERM/SIGINT with a 10 s drain; exit non-zero on config error with every validation message printed once.
- Cargo features: `admin` (enables `yhotos-http/admin` and the admin listener). Default features: none.

### Benchmarks
- `yhotos-bench/benches/access.rs`: `can_view_photo` for anonymous, user with group grant, admin, at album depth 1 and 5. Budget: each call under 100 ns.

## Acceptance

- [ ] `cargo run -p yhotos-server` with a valid env starts and `curl -i localhost:8080/healthz` returns 200 `ok` with every security header from architecture section 8.
- [ ] Starting with an invalid config (missing `database_url`, http base url, short session key, `admin_listen=0.0.0.0:1` with the admin feature) exits non-zero and prints all problems at once.
- [ ] `GET /` renders the template; the CSP header's nonce matches the nonce on the inline-free `<script>`/`<style>` tags, and a test asserts that.
- [ ] Without the `admin` feature, `/admin/healthz` returns 404 on the public listener and the admin listener does not exist (test that `admin_router` symbol is not compiled: a `cfg` test).
- [ ] With the `admin` feature, the admin listener binds only to the configured address; a test asserts the public listener returns 404 for `/admin/healthz`.
- [ ] Rate limiting: a test issues `cheap_per_min + 1` requests from one IP and gets a 429 with `Retry-After` on the last; a different IP is unaffected.
- [ ] Request id is generated server-side; a client-supplied `x-request-id` is ignored (test).
- [ ] Internal errors return a generic body containing the request id and nothing else; the test triggers one via a test-only route behind `#[cfg(test)]`.
- [ ] `yhotos-core::access` table-driven and property tests pass; a coverage check (manual listing in the PR) shows every rule in the Design has a test.
- [ ] `ShortId` and `Slug` have tests for every rejection case.
- [ ] `scripts/check.sh` passes.
- [ ] `cargo bench -p yhotos-bench --bench access` runs and numbers are in the PR description.

## Security notes

- Threat model 4.4: all headers exactly as specified; CSP has no `unsafe-inline` or `unsafe-eval` anywhere; `Server` header removed; `/healthz` returns nothing but `ok`.
- Threat model 4.3: `Forbidden` renders as `NotFound` on public routes. Write the test.
- Threat model 4.2: `trust_proxy` false by default; when true, only `CF-Connecting-IP` is read, never `X-Forwarded-For`.
- Config: `SecretString` for secrets; `Debug` on `Config` must redact; test it by formatting and asserting the secret is absent.
- No `unsafe`. No `build.rs`. Dependencies allowed beyond task 001's list: `figment`, `url`, `rust-embed`, `askama`, `askama_axum` (or hand-written `IntoResponse`), `dashmap`, `mimalloc`, `deunicode`, `base32` (or hand-rolled, 20 lines, preferred), `ulid` (optional), `http-body-util` and `tower-test` for tests.

## Performance

- Startup under 1 s (measure with `time`, record in PR).
- Access benchmark under 100 ns per call.
- Security header middleware must not allocate per request beyond the nonce (16 random bytes, base64); note how in the PR.

## Not in this task

- Database, sessions, login, any real page beyond `/`.
- Static CSS content (a single empty `app.css` is fine).
- The `/i/`, `/o/`, `/v/` routes.
