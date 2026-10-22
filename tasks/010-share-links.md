---
id: 010
slug: share-links
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [008, 009]
crates: [yhotos-http, yhotos-db]
---

# 010 Share links

## Goal

Tokenized links to an album or a single photo, with per-link capabilities (view, download, like, flag), optional passphrase, optional expiry, revocation, and open statistics, exactly as `docs/scope.md` section 5.3 describes. The public side (landing, unlock, browsing through the link, file access) is in this task; the admin management UI is task 012, but the database functions and a minimal admin JSON API to create/update/revoke are included here so the feature is testable end to end.

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.3
- `docs/architecture.md` 4 (`share_links`, `share_link_opens`), 8
- `docs/threat-model.md` 4.3 (share link overreach), 4.2 (passphrase brute force), 4.8 (visitor hashing)
- Briefs 005 (tokens), 008 (file access with a `Principal::ShareLink`), 009 (session middleware)

## Scope

`crates/yhotos-http/src/share/`, `crates/yhotos-http/src/middleware/session.rs` (share-link principal resolution), `crates/yhotos-http/src/files/` (honor `allow_download`), `crates/yhotos-http/templates/default/share/*.html`, `crates/yhotos-db/src/share_links.rs`, admin JSON API under `/admin/api/share-links` (feature `admin`).

## Design

### Token handling
`Token::generate()` with `TokenKind::ShareLink`; the URL is `/s/yl_<base64url>`; only `TokenHash` is stored. A landing request decodes, hashes, and looks up the row (moka 30 s). Expired, revoked, or unknown: 404 with the same body as an unknown path (never "expired" vs "wrong"; that distinction is shown only after a valid passphrase, or not at all). Rate limit `/s/*` with the `cheap` bucket and `POST /s/*/unlock` with the `auth` bucket plus a per-token counter: 10 wrong passphrases revokes the link's unlock for 1 hour and writes an audit row.

### Unlock and scoping cookie
A link with a passphrase shows an unlock form; success sets a cookie `yl_<link short id>` holding a `TokenKind::Session`-style token bound to that link (server-side row in `sessions` with `share_link_id` set and `user_id` null; add that nullable column and a check constraint in a migration). A link without a passphrase sets the same cookie on first visit so subsequent requests (images, likes) carry the scope without the token in every URL. The session middleware resolves this cookie into `Principal::ShareLink { target, link_id, caps }`; a request may carry both a user session and a link session; access is the union (a user can also open a link).

### Browsing
- `GET /s/{token}`: album target renders the album page (reuse the album template from task 011 if merged; else a minimal grid listing photos with their derivative URLs) scoped to the target, including descendant albums; photo target renders the photo page.
- Image URLs in the rendered HTML are the normal `/i/{hash}/...` routes; the file handler (task 008) grants access when the `Principal::ShareLink` target covers the photo. `allow_download = false` makes `/o/...` and `?download=1` return 404 for that principal, and the download button is not rendered.
- `allow_like` / `allow_flag` gate `POST /api/v1/photos/{id}/like|flag` for the link principal; rows get `share_link_id` set and `user_id` null.
- Every landing (not every image) records `share_link_opens` with `visitor_hash` (blake3 of ip + ua_class + daily salt; the salt is in memory, rotated at UTC midnight, never persisted), country and city from the GeoIP lookup (task 014 provides the MMDB reader; until then store null, keep the column).
- `Cache-Control: private, no-store` on every `/s/` response and on files served to a link principal.

### Admin JSON API (feature `admin`)
`POST /admin/api/share-links {target_kind, target_id, expires_at?, passphrase?, allow_view, allow_download, allow_like, allow_flag}` returns the full URL once (the token is never retrievable again); `PATCH /admin/api/share-links/{id}` updates flags/expiry/passphrase; `POST /admin/api/share-links/{id}/revoke`; `GET /admin/api/share-links/{id}/stats` returns opens, unique visitors, by country, first/last open.

### Tests
- Unknown, expired, revoked tokens are byte-identical 404s.
- Passphrase flow: wrong passphrase 10 times locks for 1 h (manipulate time), correct one sets the cookie, cookie grants image access, revoking the link makes the next request 404 within the cache TTL (invalidate on revoke).
- Capability gating for download, like, flag, each on and off; likes and flags are attributed to the link.
- Scope: a link to album A cannot read a photo in album B (404 byte-identical), nor a restricted child album above its target; a link to a photo cannot list its album.
- A user session plus a link session: the user sees the union.
- `share_link_opens` written once per landing with a visitor hash that changes across the salt rotation (test by rotating).
- Token never appears in logs: run a request with `tracing` captured and assert the token string is absent.

## Acceptance

- [ ] All tests above pass.
- [ ] A curl walkthrough in the PR: create via admin API, open, unlock, fetch an image, flag, revoke, confirm 404.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model 4.3 share-link bullets are the checklist; map each to a test.
- The raw token exists in memory only during the create response and the landing request; it is zeroized after hashing (type from task 005).
- Passphrase hashing uses `hash_password` (Argon2id) with the same timing-equalization as login.
- Allowed new dependencies: none beyond what 008/009 introduced.

## Performance

- Landing is one query cold, zero warm; image requests through a link add no query beyond task 008's lookup (the principal carries the target).

## Not in this task

- Admin UI pages for links (task 012); API only here.
- GeoIP (task 014); columns written as null until then.
