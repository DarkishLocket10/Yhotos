# Threat model

Yhotos is public on the internet, its source is public, and it holds a photographer's complete original library plus the identities and viewing habits of their friends, family and clients. This document lists what we protect, who attacks it, where the trust boundaries are, and which control stops each attack. Every PR that touches input handling, auth, files, the network, or dependencies is checked against the relevant section. Reviewers cite section numbers.

## 1. Assets

A1. Original photo files (150 GB today). Loss or unauthorized disclosure is the worst outcome.
A2. Restricted photos and albums: content meant for specific people only.
A3. Admin account: controls everything.
A4. User accounts and their likes/flags (reveal who the owner's contacts are and what they look at).
A5. Share links: a leaked token is a leaked album.
A6. Analytics data: visitor behavior, coarse location.
A7. The server itself (tower) and the home network behind it.
A8. The supply chain: the repo, CI, container images, dependencies.
A9. Availability: the gallery being up, and the home connection not being saturated.

## 2. Actors

T1. Anonymous internet attacker: scanners, bots, opportunists, targeted individuals.
T2. Holder of a share link (legitimately or leaked) trying to go beyond it.
T3. A logged-in user trying to see restricted content they were not granted, or to impersonate another.
T4. A malicious file: crafted JPEG/HEIC/RAW/video/EXIF designed to exploit a decoder.
T5. A compromised dependency or CI step.
T6. Someone who learns the home IP and bypasses Cloudflare.
T7. A malicious theme or custom CSS/HTML entered through the admin UI (admin is trusted, but a compromised admin session is not).
T8. Insider on the LAN or tailnet (lower likelihood; still, admin is authenticated).

## 3. Trust boundaries

B1. Internet to Cloudflare edge: TLS, WAF, rate limits, caching. Not under our control; treat Cloudflare as a filter, not a guarantee.
B2. Cloudflare to `server-public` through the tunnel: only Cloudflare can reach it; verify the `CF-Connecting-IP` header only from the tunnel, nothing else.
B3. `server-public` to Postgres and the filesystem: the public server has a database role with no direct write access to `users`, `user_mfa`, `settings`, or `jobs`. It may insert events, likes, flags, share-link opens, sessions and audit rows, maintain its own sessions, and change a user's own password only through a `SECURITY DEFINER` function (`user_set_own_password`) so the grant surface on `users` is one function, not a table. The filesystem is mounted read-only.
B4. Tailscale to `server-admin`: only tailnet devices reach it; the admin role in Postgres is broader.
B5. Worker to `yhotos-codec-host`: the child process is sandboxed; the worker trusts only the structured report it returns and re-validates files.
B6. Worker/server to `yhotos-ml`: internal network only; ML results are untrusted data (tags, text) and are escaped like any input.
B7. Repo to production: CI builds signed images from reviewed commits only.

## 4. Threats and controls

Each entry: threat, then the control, then where it lives in code so reviewers can check it.

### 4.1 Input and parsing

- **Malicious image, RAW, HEIC, or video exploits a decoder (T4).**
  Control: decoders run only inside `yhotos-codec-host`, a child process with seccomp (no network, no exec, no ptrace), RLIMIT_AS, CPU time limit, wall timeout, and an empty filesystem view except the input and output directories. A crash loses one job, not the server. Header-only size checks before decode. Dimension and pixel-count caps. libvips, libheif, libraw/rawler, ffmpeg pinned and updated through Dependabot; `cargo audit` on every PR. Fuzz targets for the wrapper. Code: `yhotos-codec-host`, `yhotos-codec::limits`.
- **Decompression bomb / pixel flood.** Control: `max_pixels` cap read from headers before any decode; AVIF/JXL/PNG bombs stopped by the same cap plus RLIMIT_AS.
- **Polyglot or mislabeled uploads.** Control: magic-byte sniffing, allow list, and the decoder's own identification must agree; MIME on serving comes from the database, never from the request or file name.
- **Path traversal via filenames or hashes.** Control: `yhotos-storage::safe_join` is the only path constructor; hashes are validated against `^[0-9a-f]{64}$`; filenames from uploads are stored as display text only.
- **EXIF/XMP/IPTC injection** (scripts in metadata, oversized fields, SSRF URLs in XMP). Control: metadata is parsed in the sandbox, extracted into a fixed typed subset, length-capped, and HTML-escaped by `askama` on output. No URL in metadata is ever fetched.
- **Query parser abuse** (`search?q=`). Control: hand-written parser with bounded length and term count, fuzzed; parameters bound in SQL; semantic search has a timeout.
- **tus upload protocol abuse** (overlapping offsets, huge declared sizes, slowloris). Control: per-upload state in the database, offsets validated, declared size capped, idle uploads expire, admin-only listener.

### 4.2 Authentication and sessions

- **Credential stuffing / brute force (T1).** Control: Argon2id (m=64 MiB, t=3, p=1), per-IP and per-username rate limits with exponential lockout, constant-time compare, identical responses for unknown user and wrong password. Admin requires TOTP or passkey.
- **Session theft.** Control: 256-bit random session tokens stored hashed (BLAKE3), `Secure; HttpOnly; SameSite=Lax` cookies (`Strict` for admin), session rotation on login and privilege change, absolute and idle expiry, `ip_hash`/`ua_class` change logged, revocation list in the user's security page.
- **CSRF.** Control: `SameSite` plus double-submit token on every state-changing form; JSON API requires a custom header and same-origin check (`Sec-Fetch-Site`).
- **Passkey / TOTP weaknesses.** Control: `webauthn-rs` with attestation not required, user verification required for admin; TOTP with 30 s step, 1-step drift, replay prevention (last used counter), secrets encrypted at rest with a key from config.
- **Temporary passwords.** Control: `must_change_password` enforced by middleware on every authenticated route except the change form and logout.
- **Privilege escalation (T3).** Control: `is_admin` checked server-side per request from the session's user row, never from a cookie or a form field. Admin routes do not exist in the public binary at all.

### 4.3 Authorization

- **Seeing restricted content (T2, T3).** Control: a single access function in `yhotos-core::access` evaluated for every photo and album read, including derivative and original fetches by hash (the hash alone is not an authorization: `/i/` and `/o/` look up the owning photo and check visibility; public photos' derivatives are cacheable, restricted ones are served `private, no-store` and only to authorized sessions or valid share links). Exhaustive tests of the access matrix.
- **Share link overreach.** Control: links authorize exactly one target; capabilities (view/download/like/flag) are stored on the link and checked per action; passphrase hashed with Argon2id and rate limited per token; tokens 256-bit, hashed at rest, never logged; expiry and revocation checked on every request; enumeration impossible by construction.
- **IDOR via short ids.** Control: short ids are not secrets; every lookup by short id goes through the access function. Restricted photos return 404 (not 403) to anonymous requests so existence is not revealed.

### 4.4 Web

- **XSS.** Control: `askama` auto-escaping, CSP with nonces and `default-src 'none'`, no inline scripts, custom HTML sanitized by `ammonia` with a tight allow list, custom CSS served as a separate file and limited by CSP (`style-src 'self'` plus nonce; no `unsafe-inline`).
- **Clickjacking, MIME sniffing, referrer leakage.** Control: `frame-ancestors 'none'`, `nosniff`, `Referrer-Policy`.
- **Open redirect.** Control: redirects only to a fixed allow list of internal paths; `legacy` redirects come from the `legacy_redirects` table, not the request.
- **SSRF.** Control: the public server makes no outbound requests at all. The ML client talks to a fixed internal address from config. Map tiles are loaded by the browser, not the server.
- **Information leakage.** Control: generic error pages; `Server` header removed; no version strings; `/healthz` returns only `ok`; 404 and 403 indistinguishable for restricted content.
- **Denial of service on expensive routes (A9).** Control: zip, originals, search have their own token buckets per IP and global concurrency limits; zips stream with no buffering and a size cap; Cloudflare rate rules on top. Image derivatives are static files served with `sendfile`-style streaming.

### 4.5 Infrastructure

- **Untrusted Piwigo migration paths.** MariaDB rows, including `piwigo_images.path`,
  are untrusted. Migration reads every original through storage's `SourceTree`
  directory capability, never by joining database text onto a path. It rejects
  traversal, absolute paths, empty/dot components, NULs, backslashes, symlinks and
  non-regular files. Linux `openat2` additionally forbids mount crossings and magic
  links; macOS uses a descriptor walk with `O_NOFOLLOW` for development. Every
  descriptor is read-only, and migration reads from a read-only source mount.
  The known `./upload/` or `upload/` prefix is removed from a Piwigo path before
  passing the relative name to `SourceTree`; the file handle is streamed without
  reopening a path. No source API creates entries or opens files for writing.

- **Bypassing Cloudflare (T6).** Control: Cloudflare Tunnel; router ports 80/443 closed; `server-public` listens only on the container network reached by `cloudflared`. Nothing on the host is port-forwarded for Yhotos.
- **Admin exposed (A3).** Control: admin container bound to the Tailscale IP only, `admin` feature absent from the public binary, separate Postgres role.
- **Container breakout / lateral movement (A7).** Control: non-root, read-only root filesystem, dropped capabilities, `no-new-privileges`, seccomp default profile, `/data` read-only in public server, ML container with no internet egress, GPU device only in `ml`.
- **Database compromise.** Control: least-privilege roles per process, TLS or Unix socket to Postgres, secrets encrypted at rest (MFA secrets, nothing else sensitive at rest except hashes), nightly backups with restore drills.
- **Home connection saturation (A9).** Control: edge caching of all derivatives (content-hashed, immutable), originals cached too, HTML cached briefly; originals and zip downloads rate limited per IP.

### 4.6 Supply chain (T5)

- **Malicious or vulnerable dependency.** Control: `cargo deny` (advisories, licenses, duplicate versions, source allow list), `cargo audit`, Dependabot weekly, exact version pins, `Cargo.lock` committed, new deps justified in PRs and reviewed.
- **CI compromise.** Control: GitHub Actions pinned by SHA, minimal `permissions:` per job, no secrets in PR builds from forks, OIDC for image signing, branch protection on `main`, required checks, signed images verified at deploy.
- **Secrets in repo.** Control: push protection and secret scanning on; `gitleaks` in CI and pre-commit; `.env` ignored; config only from environment.
- **Malicious contribution.** Control: every PR reviewed by a human-directed review plus automated review; agents cannot merge; `CODEOWNERS` on security-critical paths.

### 4.7 Theme and admin content (T7)

- **Theme as backdoor.** Control: templates are compiled into the binary at build time, so a theme change is a reviewed PR, not a runtime upload. Custom CSS cannot load remote resources (CSP). Custom HTML is sanitized and cannot contain scripts. Admin actions are in the audit log.

### 4.8 Privacy

- **Analytics becoming tracking.** Control: no cookies for analytics, daily salted visitor hash, salt never stored, GeoIP to city at most, raw events deleted after 90 days, no third-party scripts anywhere, face names, embeddings and person search admin-only. Non-admin responses carry face data only for people the admin opted in (off by default): normalized boxes, crops served under the photo's own visibility check, and a random, resettable public id, on photos the viewer can already see; `/f/` pages are `noindex` and return the standard 404 for unknown, opted-out, hidden or empty ids (task 024).
- **EXIF leaking location.** Control: GPS is stored in the database for map/search but stripped from every derivative; originals keep it and are only downloadable by people who can see the photo (the owner decides what goes public). An admin setting can strip GPS from originals on download for public photos.

## 5. Residual risks, accepted

- Cloudflare sees traffic in the clear and could be compromised or compelled; accepted for the bandwidth and DDoS benefits.
- A full admin device compromise (with Tailscale and passkeys) defeats admin protections; mitigated by audit log and session revocation, not prevented.
- Zero-days in libvips/libheif/libraw/ffmpeg; mitigated by the sandbox so the blast radius is one job.
- RAW decoding through `rawler`. RAW files reach the decoder only through the admin upload path, which is reachable only over Tailscale, so a hostile RAW requires an admin to upload it. `yhotos-codec` validates the container structures it can check cheaply (TIFF IFD entries, MakerNote layouts, embedded TIFF blobs, JPEG SOF dimensions, preview extents) before calling `rawler`. Allocation behaviour inside `rawler`'s own parsers beyond those structures is not chased further: it is contained by the codec host sandbox (`RLIMIT_AS`, CPU limit, seccomp, separate process), and the worker records a child abort as a failed photo. Reviews should flag a RAW parsing gap only when it escapes the sandbox, affects the server or worker process, or is reachable without admin upload.
- Legitimate share-link holders can screenshot or re-share; no DRM is attempted.

## 6. Verification

- Unit and integration tests for every control that is code (access matrix, safe_join, token handling, rate limits, headers).
- Fuzzing: continuous in CI for the parsers in 4.1.
- `scripts/security-smoke.sh`: hits a running stack and asserts headers, 404 behavior, rate limits, CSP, and that admin routes do not exist publicly.
- External penetration test before launch, scope: everything reachable through Cloudflare plus the share-link and account tiers. A second test if the account tier ships later than the first.
- This document is reviewed at every milestone and whenever a new input type, route, or dependency category is added.
