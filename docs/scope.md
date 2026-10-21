# Yhotos: scope and architecture

Status: scoping complete, 2026-10-21. This document is the source of truth for scope and architecture.

## 1. Vision and non-negotiables

Yhotos replaces Piwigo as the home of Yash's photography at `photos.yashnilay.ca`. Built from scratch in Rust, with no PHP anywhere.

Non-negotiables, each one measurable:

1. **Security.** Internet-facing, public source code, so it is built as if every line will be attacked. Threat model in the repo, defense in depth, memory-safe code, no `unsafe` outside the codec wrapper, continuous dependency auditing, fuzzing of every untrusted-input parser, external penetration test before launch.
2. **Speed.** Performance budgets enforced in CI. A change that regresses a budget does not merge. Every image derivative is generated once at upload and served as a static file from Cloudflare's edge.
3. **Fidelity.** Rendered images look exactly as the uploaded file intends: ICC-managed pipeline, wide gamut preserved, gain-map HDR delivered to HDR-capable browsers with correct SDR fallback, high-quality resampling only.
4. **Customization.** Themeable down to the detail, without runtime cost and without theme code becoming an attack vector.
5. **UI/UX.** Snappy, pretty, easy. Instant-feeling navigation, zero layout shift, works on phones and slow connections.

Launch model: **big bang**. Piwigo keeps running until Yhotos is complete, pen-tested, and signed off. Yhotos runs on tower internally throughout development with the real library.

## 2. Current state (Piwigo, measured 2026-10-21)

- 4,925 images, 150 GB of originals at `/mnt/user/data/media/gallery/upload` on tower's array. All JPEG plus one PNG. No videos.
- Average 31 MB per file, max 142 MB. Average ~6800 x 4500 px, widest 13,895 px.
- 33 albums named `Title | Year`, flat except `Japan | 2024/25` (3 children). All public. 12 photos admin-only.
- Metadata: titles on all photos (filenames), 1 description, 1 tag, 16 geotagged, no ratings, no comments.
- 2 users (admin, guest). No private albums, no per-album permissions. Self-registration off.
- Features in use: justified grid (GThumb), EXIF panel, download with size picker, slideshow, favorites, sort order, date views, Open Graph previews, language switcher (English only in practice).
- Infra: Cloudflare DNS proxy (orange cloud) in front of a router port-forward to `swag` (nginx, wildcard cert for `*.darkishlocket10.me`). No Cloudflare Tunnel. `yashnilay.ca` is GitHub Pages and stays separate.
- Old deep links to albums currently fall through to the homepage (IDs were reset at some point). Redirects will map by album name, not ID.

## 3. Access tiers

| Tier | Who | Can |
|---|---|---|
| Public | anyone | browse public albums and photos, search (non-face), view EXIF, download originals and generated sizes, bulk download |
| Account | people Yash creates accounts for (friends, family, clients), optionally in groups | everything public can, plus albums and photos whose visibility is restricted to them or their group, like, flag, personal Liked tab, optional TOTP or passkey |
| Share link | anyone holding a tokenized link | an album or a single photo, with per-link toggles for view / download / like / flag, optional passphrase, per-link expiry, revocable, open count and coarse location recorded |
| Admin | Yash only | everything, reachable only over Tailscale |

Accounts: created by admin with a temporary password, forced change on first login. No self sign-up. No email sending in v1, so no password reset by email; admin issues a new temporary password. Email MFA exists in code but is switched off until an SMTP sender is configured.

Admin auth: password plus at least one of TOTP or passkey, required. Admin UI and upload endpoints bind to the Tailscale interface only; the public listener never exposes them.

Visibility is set per album and per photo, to users and/or groups.

## 4. Content and image pipeline

Accepted uploads: JPEG, PNG, TIFF, HEIC/HEIF, RAW (Sony ARW, Panasonic RW2, Canon CR2/CR3, iPhone DNG, Android DNG and others via the RAW decoder), gain-map HDR (JPEG and HEIC), and video (see 5.9).

Pipeline at upload, run by a worker pool, never in the request path:

1. Validate: magic bytes, decoder in a sandboxed process, size limits, decompression-bomb guard.
2. Store original as-is, content-addressed (BLAKE3 hash), on the array.
3. Extract metadata: EXIF, IPTC, XMP, GPS, ICC profile, HDR gain map presence, RAW flag.
4. Decode to 16-bit linear working space with the embedded ICC; RAW via embedded preview or default demosaic (labeled RAW in the UI, no "intended look" claimed).
5. Generate derivatives: square thumb, small, medium, large, xlarge, 2x variants, in AVIF and JPEG XL with JPEG fallback; HDR variants with gain map where the source has one; blurhash/thumbhash placeholder; dominant colors. Lanczos3 resampling, encoder settings tuned per size for quality, verified by SSIM/butteraugli in tests.
6. Queue ML jobs (section 5.7).

Derivatives are immutable files named by content hash plus variant, served with `Cache-Control: immutable` for a year. Changing a photo produces new hashes; old URLs keep working until garbage-collected.

Tooling: libvips (via Rust bindings) for decode, color management (lcms2), resize and encode; rawler/libraw for RAW; libheif for HEIC; ffmpeg for video.

## 5. Features

### 5.1 Viewer
- Justified grid as default layout, with masonry and uniform grid as theme options.
- Lightbox: keyboard, swipe, pinch zoom to original resolution tiles, EXIF panel, download menu (original and each size), like, flag, share, slideshow with configurable timing.
- Responsive images via `srcset` and `sizes`; format negotiation by `Accept` header; lazy loading; placeholders; prefetch on hover; view transitions.
- Sorting: date taken, date posted, name, manual order (admin-defined).
- Map view (photos by location, clustered) and calendar view (by year, month, day).
- Bulk download: whole album or selected photos as a zip, streamed, with size choice.
- Open Graph and Twitter cards for every album and photo.
- Translation-ready UI, English shipped.

### 5.2 Logged-in extras
- Restricted albums and photos per user or group.
- Like (heart) per photo; personal Liked tab.
- Flag per photo: binary, with flagger identity; appears in the admin flag queue.
- Optional TOTP or passkey enrollment, password change, active session list and revocation.

### 5.3 Share links
- Target: album or single photo.
- Token: 256-bit random, URL-safe, stored hashed. No enumeration possible.
- Per link: expiry (date or never), optional passphrase (any string), toggles for view / download / like / flag, editable after creation, revoke at any time.
- Likes and flags via a link are attributed to the link, not a person.
- Stats per link: opens, unique visitors (hashed), countries, first and last open.
- Rate limited and brute-force protected on the passphrase.

### 5.4 Admin (Tailscale-only)
- Web upload UI: drag and drop, batch, resumable chunked uploads, progress, assign to album, pre-fill from EXIF.
- Album tree: create, nest, reorder (drag), cover image, description, visibility.
- Bulk tools: multi-select to move, copy, reorder, tag, change visibility, replace a photo in place (keeps URL, likes, flags, analytics), delete to trash, restore, empty trash.
- Flag queue with one-click actions: replace, re-edit (download original), remove, change visibility, dismiss.
- Likes overview: who liked what, most liked.
- Users and groups: create, disable, reset temporary password, assign groups, see sessions.
- Share link management.
- Face management (admin only): clusters, name people, merge, hide; person search.
- Theme editor (section 5.8).
- Analytics dashboard (section 5.6).
- Job queue view: derivative generation, ML, video transcodes, with retry.
- Audit log of every admin action.

### 5.5 Search
- Semantic search (CLIP/SigLIP embeddings via pgvector), public.
- Tags: auto-generated object and scene tags, editable, plus manual tags. Tags cover the "collections" concept (e.g. `best-of-2025`).
- OCR text search.
- Structured filters: camera, lens, focal length, aperture, ISO, date range, place (reverse-geocoded offline), dominant color, orientation, file type, RAW/HDR flags.
- "More like this" and duplicate detection.
- Faces: detection and clustering for all photos, but naming and person search are admin-only. Public and account users never see face data.

### 5.6 Analytics (first-party, server-side)
- Collected from day one: page views, photo views, lightbox opens, downloads by size, zip downloads, search queries, referrers, countries and cities, device class, share-link opens.
- Privacy: no third-party scripts, no tracking cookies, IPs hashed with a daily rotating salt, GeoIP from a local database, nothing that triggers consent banners or tracking blockers.
- Dashboard: per photo, per album, trends, top referrers, top searches, share link stats.

### 5.7 ML worker
- Separate service on tower, GPU (GTX 1080 Ti, pinned to a CUDA runtime that still supports Pascal), no internet access, consumes a job queue, writes to Postgres.
- Models (candidates, final choice by benchmark): SigLIP or CLIP ViT-L/14 for embeddings; InsightFace (ArcFace) for faces; RapidOCR/PaddleOCR for text; zero-shot tagging from the embedding model or RAM++; offline reverse geocoding from GeoNames.
- Never in the request path. Query embedding at search time is the one online call, and it is small.

### 5.8 Theming
Layered, zero runtime cost:
1. Design tokens (colors, type, spacing, radii, grid density, motion) edited live in admin, emitted as CSS variables.
2. Layout and component options as settings.
3. Templates compiled at build time (not interpreted per request).
4. Custom CSS and small script hooks, under a strict Content Security Policy with nonces, so a theme can never read data or make requests it should not.
Dark and light modes, system-aware.

### 5.9 Video
- Accepted: MP4/MOV (H.264, HEVC), iPhone and Android formats.
- Pipeline: ffmpeg transcode to H.264 MP4 (universal) and AV1 (modern), poster frame, scrub thumbnails; HLS only if a video exceeds a length threshold.
- Plays inline in the grid and lightbox; download original.

## 6. Architecture

Components (each its own container on tower):
- `yhotos-web`: Rust, axum, tokio, sqlx, tower middleware. Serves public HTML, API, auth, share links. Public listener via Cloudflare Tunnel.
- `yhotos-admin`: same binary, admin routes enabled, bound to the Tailscale interface.
- `yhotos-worker`: derivatives, video transcodes, zip streaming, cleanup. CPU-bound.
- `yhotos-ml`: GPU worker (Rust with ONNX Runtime, or Python where a model only ships that way), network-isolated.
- PostgreSQL 16 with pgvector (reuse tower's instance, separate database and role).
- Redis or Postgres `LISTEN/NOTIFY` for the job queue (decide by benchmark; Postgres-only is simpler).
- Object layout on the array: `/originals/<hash-prefix>/<hash>`, `/derivatives/<hash>/<variant>.<ext>`, `/video/...`.

Frontend: server-rendered HTML from compile-time templates (askama or maud), islands in TypeScript (no framework), total JS under budget, WASM only where a benchmark earns it (candidate: JXL decoding on browsers without support).

URL scheme:
- `/` home, `/a/<album-slug>`, `/a/<album-slug>/<child-slug>`, `/p/<photo-id>-<slug>`
- `/i/<hash>/<variant>.<ext>` immutable derivatives, `/o/<hash>/<filename>` originals
- `/s/<token>` share links, `/map`, `/calendar/<year>/<month>`, `/search?q=`
- `/u/liked` personal tab, `/admin/...` Tailscale only
- Redirect table for legacy Piwigo URLs (`/index.php?/category/<id>` and friends) matched by album name.

Caching: Cloudflare edge caches `/i/` and `/o/` for a year (content-hashed), HTML cached briefly for public pages with cache-tag purge on change. Origin sends ETags and `304`s. Zip downloads and search are never cached.

## 7. Security

Threat model (full version to live in `docs/threat-model.md`): untrusted uploads, token guessing, auth attacks, injection, SSRF via metadata, path traversal, DoS on expensive endpoints, dependency compromise, theme-as-backdoor, admin exposure, data exfiltration via analytics.

Controls:
- Decoders run in a separate sandboxed process (seccomp, no network, memory and time limits). A codec crash cannot take the server down.
- `#![forbid(unsafe_code)]` everywhere except the codec wrapper crate, which gets fuzzing and extra review.
- Argon2id passwords, WebAuthn passkeys, TOTP, session tokens hashed at rest, strict same-site cookies, CSRF tokens on state changes.
- Share tokens 256-bit, stored hashed, constant-time comparison, rate limits per IP and per token.
- Content Security Policy with nonces, no inline scripts, HSTS preload, frame-ancestors none, Referrer-Policy, Permissions-Policy.
- Rate limiting at Cloudflare and at origin; expensive endpoints (search, zip, originals) have per-IP budgets.
- Admin and upload only over Tailscale; public listener has no admin routes compiled in.
- Secrets via environment only; gitleaks pre-commit; GitHub push protection on.
- CI: `cargo audit`, `cargo deny` (licenses and advisories), CodeQL, Dependabot, `cargo clippy -D warnings`, fuzz corpus run per PR, full fuzz nightly.
- Signed container images and releases, SBOM published.
- `SECURITY.md` with a disclosure process.
- External penetration test before launch; a second after accounts and share links go live if they ship later than the first test.
- Cloudflare: Tunnel replaces the router port-forward; router ports 80/443 closed; WAF managed rules on.

## 8. Performance

Budgets (proposed, enforce in CI; tune after first measurements):
- Origin TTFB for a cached-miss album page: p50 under 20 ms, p99 under 50 ms.
- Album HTML under 30 KB compressed; total JS under 50 KB compressed; CSS under 30 KB.
- LCP under 1.5 s on simulated 4G, under 1.0 s on broadband; CLS zero; INP under 100 ms.
- Derivative generation: a 30 MP JPEG to all variants in under 4 s on tower's CPU; migration of 4,925 photos in under 6 hours wall clock.
- Search p99 under 50 ms at 10k photos; 100 concurrent album views without exceeding 1 core.
- Memory: web process under 200 MB steady state.

Measurement:
- `criterion` micro-benchmarks for the pipeline and hot paths, tracked over time.
- `oha`/`k6` load tests against a staging container in CI, with regression gates.
- Lighthouse CI on representative pages per PR.
- Real-user metrics (Web Vitals) collected into the first-party analytics, so the budget is checked against real phones, not only the lab.
- A `perf/` dashboard page in admin showing trends per commit.

## 9. Infrastructure

- Tower runs everything. Originals and derivatives on the array (parity-protected), never on the cache drive (only ~100 GB free).
- GPU: one GTX 1080 Ti reserved for `yhotos-ml`.
- Cloudflare Tunnel from tower to `photos.yashnilay.ca`; cache rules for `/i/` and `/o/`; WAF on.
- Tailscale for admin; Yhotos admin listener bound to the Tailscale IP.
- Backups: nightly `pg_dump` to the array, weekly copy off-box (location to decide); derivatives are reproducible and not backed up; originals rely on array parity plus whatever off-site copy Yash already keeps (to confirm).
- Staging: a second stack on tower at a Tailscale-only address for pen testing and load tests.

## 10. Migration

1. Read Piwigo's MariaDB: albums (tree, order, covers, descriptions), images (title, description, author, dates, GPS, level), tags, image-tag links.
2. Ingest all 4,966 original files through the normal pipeline (hash, derivatives, ML), mapped to their Piwigo albums. Piwigo's own derivatives are ignored.
3. Carry over the 12 admin-only photos as restricted visibility.
4. Build the legacy redirect table from old album names.
5. Verify: counts match, every album has its cover, spot-check fidelity against originals, run the full ML pass.
6. Users are not migrated; Yash re-creates them.
7. Rehearse the migration at least twice against a snapshot before the real cutover.

## 11. Development process

- Repo: `DarkishLocket10/Yhotos`, public, AGPL-3.0.
- Planning lives in the repo: `docs/architecture.md`, `docs/threat-model.md`, `docs/performance.md`, `docs/theming.md`, `tasks/` with one brief per task (goal, files, acceptance tests, security notes, benchmark expectations, suggested model and effort).
- Claude (Fable) owns planning, task briefs, final review and merge decisions.
- Codex `gpt-6.1-sol` runs tasks non-interactively (`codex exec`) in isolated worktrees on yesktop (primary, ~44 GB free) or yac (one or two at a time), opening a PR each. Sandboxed mode on both machines.
- Every PR gets an automated `gpt-6-astra` review with a fixed brief (security, performance, tests, fidelity) plus full CI.
- Model tiering: high effort for architecture, auth, pipeline, security-sensitive code; medium for ordinary features; low effort or smaller models (Claude Haiku/Sonnet, lighter Codex) for fully specified mechanical work such as fixtures, docs from code, lint passes, translation scaffolding.
- Commit and PR style: short plain point form, no em or en dashes, branch prefix `y/`.
- Test fixtures: a curated sample set on tower (a few hundred of Yash's originals plus synthetic RAW/HEIC/HDR/TIFF/PNG/video samples and malicious files for fuzzing), served to agents over Tailscale.
- Prerequisite: `gh auth login` on yesktop.

## 12. Internal milestones (single public launch at the end)

1. Foundation: repo, CI, security scaffolding, benchmark harness, Postgres schema, auth core, admin over Tailscale.
2. Pipeline: upload, validation sandbox, derivatives, fidelity tests, HDR, RAW, video.
3. Public viewer: albums, grid, lightbox, downloads, EXIF, OG cards, map, calendar, theming layers 1 to 4.
4. Accounts, groups, visibility, likes, flags, share links.
5. ML worker and search; faces admin-only.
6. Analytics collection and dashboard; Web Vitals.
7. Migration tooling and rehearsals.
8. Hardening: fuzzing campaign, load tests, Lighthouse, pen test, fixes.
9. Cutover: Cloudflare Tunnel, DNS, redirects, Piwigo retired.

## 13. Decisions proposed, awaiting a yes
- AGPL-3.0 licence.
- Postgres `LISTEN/NOTIFY` as the job queue rather than Redis, unless benchmarks disagree.
- Performance budgets in section 8 as the starting values.
- Who performs the penetration test, and budget.
- Off-site backup target for originals and the database.

## 14. Out of scope for v1
Self sign-up, email sending, notifications, comments, ratings, Lightroom publish plugin, watched-folder ingest, public face search, multi-tenant or multi-gallery hosting. Yash will request more features after launch.
