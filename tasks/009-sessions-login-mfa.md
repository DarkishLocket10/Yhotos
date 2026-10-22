---
id: 009
slug: sessions-login-mfa
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [002, 003, 005]
crates: [yhotos-http, yhotos-db, yhotos-auth]
---

# 009 Sessions, login, forced password change, and MFA enrollment

## Goal

Users can log in with username and password, are forced to change a temporary password before doing anything else, can enroll TOTP and passkeys and are challenged for them at login, can see and revoke their sessions, and can log out. The admin account must have at least one MFA method enrolled before any admin route works. Sessions live in Postgres and are loaded by middleware into a `Principal` for every request. Everything follows `docs/threat-model.md` 4.2.

## Read first

- `AGENTS.md`
- `docs/architecture.md` sections 4 (`users`, `user_mfa`, `sessions`), 8 (middleware order)
- `docs/threat-model.md` 4.2 in full, 4.4 (CSRF, open redirect)
- Brief 005 (the primitives you call) and 002 (middleware and templates)

## Scope

Areas: `crates/yhotos-http/` (new `auth/` module, `middleware/session.rs`, router and `AppState` wiring, CSRF and rate-limit integration, error mapping additions, templates under `templates/default/auth/`, static asset wiring for `passkey.js`), `crates/yhotos-server/` (startup wiring, the `create-admin` CLI subcommand), `crates/yhotos-db/` (`users`, `sessions`, `mfa`, `audit` repository additions, migrations for `pending_webauthn`, `pending_login`, `sessions.reauth_at`, regenerated `.sqlx/` metadata), `crates/yhotos-auth/` only if a primitive is missing (say so in the PR), `web/src/passkey.ts`, `crates/yhotos-bench/` (`session` bench target), `scripts/auth-smoke.sh`, `deploy/dev/` if the dev database bootstrap needs fixing, plus the always-in-scope supporting files from `AGENTS.md`.

## Design

### Session middleware
Reads cookie `ys` (name from config; `__Host-ys` in production when served on https root), decodes with `Token::decode`, hashes, looks up `sessions` by `token_hash` (moka cache 30 s, invalidated on logout/revoke by deleting the key), checks `session_state` against the policy (admin policy if `is_admin`), updates `last_seen_at` at most once per 5 minutes, and inserts `Principal::User { id, groups, is_admin }` into request extensions. Missing or invalid cookie: `Principal::Anonymous`, and the invalid cookie is cleared. A user with `must_change_password` is redirected to `/u/password` on every HTML route except `/u/password`, `/logout`, `/static`; API routes return 403 `{"error":"password_change_required"}`.

Cookie attributes: `Secure; HttpOnly; SameSite=Lax; Path=/` for users; `SameSite=Strict` for admin sessions (the admin listener issues its own cookie named `ya` so a public-site session never carries admin rights, and vice versa).

### Routes (public router)
- `GET /login`: form with CSRF token. If already logged in, redirect to `/`.
- `POST /login`: fields `username`, `password`, `next` (validated: must start with `/` and not `//`, else dropped). Flow: lockout check per username and per IP; `verify_password` or `dummy_verify`; on failure record and show the same message either way; on success, if the user has MFA, store a short-lived `pending_login` (server-side, 5 min, keyed by a `yc` token in a cookie) and redirect to `/login/mfa`; else create the session (rotate: delete any session the request carried), set the cookie, redirect to `next` or `/`.
- `GET|POST /login/mfa`: TOTP code or passkey assertion; 5 attempts then the pending login is destroyed. On success create the session as above.
- `POST /logout`: CSRF-protected; deletes the session row and clears the cookie.
- `GET|POST /u/password`: current password required unless `must_change_password`; policy from task 005; on success rotate the session and clear the flag; all other sessions of the user are revoked.
- `GET /u/security`: list sessions (created, last seen, ip_hash prefix, ua_class, current marker), MFA methods, enrollment buttons.
- `POST /u/security/sessions/{id}/revoke`.
- `GET|POST /u/security/totp`: show secret as `otpauth://` URI and QR (render QR server-side as SVG with the `qrcode` crate; no JS), confirm with a code, store the secret sealed with the `Keyring` (aad = `user_mfa.id`).
- `POST /u/security/passkey/start|finish`: WebAuthn registration via `yhotos-auth::Passkeys`; state kept server-side in a `pending_webauthn` table or an in-memory map with 5 min TTL (table preferred for multi-process correctness; add a migration).
- `POST /u/security/mfa/{id}/remove`: requires a fresh re-authentication (password within the last 5 minutes, tracked on the session as `reauth_at`).
- Rate limits: all `/login*` and `/u/security*` POSTs use the `auth` bucket.

### Admin listener
Same handlers mounted under `/admin/login` etc. with the admin cookie and policy, plus a guard middleware: any `/admin/*` route except login requires `is_admin` and at least one enrolled MFA; otherwise redirect to `/admin/setup-mfa` which only allows enrollment. The first admin user is created by a CLI subcommand `yhotos-server create-admin --username` that prints a generated temporary password once (this is the only place a password is printed, and it is the user's own terminal).

### Audit
Every login success/failure (username, ip_hash, outcome), MFA enrollment/removal, password change, session revoke writes an `audit_log` row (user_id may be null for failures).

### Templates
Plain forms, no JS except `passkey.ts` for WebAuthn, which is loaded only on pages that need it with the CSP nonce. Error messages are generic ("Sign in failed").

### Tests
Integration tests with a real Postgres and `tower::ServiceExt::oneshot`:
- login success sets a cookie with the exact attributes; failure for unknown user and wrong password return identical status, body, and timing within 20 % (5 samples, `#[ignore]` if flaky);
- lockout kicks in after 5 failures and the `Retry-After`/message matches the policy;
- forced password change redirects everywhere and clears after change; other sessions are revoked;
- session rotation on login; logout invalidates immediately even with the cache (test that a request right after logout is anonymous);
- idle and absolute expiry (manipulate timestamps directly in the DB);
- CSRF: POST without token is 403;
- `next` parameter: `//evil`, `https://evil`, `/\evil`, and `javascript:` are dropped;
- TOTP enrollment and login with a code generated by `yhotos-auth::totp` in the test; replay rejected;
- passkey enrollment and login with the software authenticator from task 005's dev-dependency;
- admin guard: admin without MFA is redirected to setup; non-admin user on the admin listener gets 404;
- public listener never issues or accepts the admin cookie;
- audit rows written for each event.

### Bench
`session.rs`: session middleware on a warm cache (under 20 µs) and cold (one query).

## Acceptance

- [ ] All tests above pass in CI.
- [ ] `yhotos-server create-admin` works and the printed password satisfies the policy.
- [ ] A manual walkthrough script `scripts/auth-smoke.sh` (curl) logs in, is forced to change the password, changes it, enrolls TOTP (using `oathtool` if present, else skipped), and logs out; the PR includes its output.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model 4.2 is the checklist; map every bullet to code and test in the PR.
- Never put the WebAuthn or pending-login state in a cookie; server-side only.
- The pending-login cookie and the session cookie are different tokens of different `TokenKind`s.
- Password change invalidates other sessions; MFA removal requires re-auth; both tested.
- `Referrer-Policy` and `Cache-Control: no-store` on every auth page.
- Allowed new dependencies: `qrcode` (SVG renderer), `moka`, `axum-extra` (cookies, typed headers), `tower-cookies` or `axum-extra` cookie jar (pick one), `time` or `jiff` (whichever task 002 chose).

## Performance

- Session lookup warm under 20 µs; `last_seen_at` write coalesced to once per 5 min; prove with the query-count pattern.

## Not in this task

- Email MFA (code path reserved; return `Unsupported`).
- Share-link principals (task 010).
- Admin user management pages (task 012); only the CLI `create-admin` here.
