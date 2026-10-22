---
id: 020
slug: admin-library
model: gpt-6.1-sol
effort: high
review_effort: xhigh
depends_on: [009, 010, 011, 012]
crates: [yhotos-http, yhotos-db]
---

# 020 Admin library: albums, photos, bulk tools, flags, likes, users, groups, links, jobs, audit

## Goal

Everything the admin needs to run the gallery day to day, on the admin listener only: the album tree editor, photo grid with multi-select bulk actions, replace-in-place, trash, the flag queue, likes overview, users and groups with grants, share-link management UI, the job queue view, and the audit log. Fast, keyboard-friendly, and safe: every mutation is CSRF-protected, audited, and checked against the admin guard.

## Read first

- `AGENTS.md`
- `docs/scope.md` 3, 5.2, 5.3, 5.4
- `docs/architecture.md` 4, 8 (admin router)
- `docs/threat-model.md` 4.2, 4.3, 4.4, 4.7
- Briefs 009 (admin guard, sessions), 010 (share-link API), 012 (upload)

## Scope

Areas: `crates/yhotos-http/src/admin/` (pages and `/admin/api/*` JSON), templates under `templates/default/admin/`, `web/src/admin/*.ts` islands, `crates/yhotos-db/` repository additions and migrations, plus supporting files.

## Design

### Pages
- `/admin` dashboard: counts (photos, albums, pending ingests, failed ingests, open flags, links expiring soon), recent activity, links to everything below.
- `/admin/albums`: tree with drag-and-drop reorder and re-parent (keyboard alternative: move up/down/indent/outdent), create, rename (slug regenerates with a redirect row from the old path), description (Markdown subset rendered with `pulldown-cmark` and sanitized), cover picker, visibility (public, restricted, inherit) with user/group grant editor, manual photo order toggle.
- `/admin/albums/{id}`: photo grid of the album (all photos including restricted, with badges for restricted, RAW, HDR, video, failed), multi-select (click, shift-click range, select all), bulk actions: move, copy (adds to another album; a photo belongs to one album, so copy creates a second photo row pointing at the same original hash and derivatives, documented), reorder (drag in manual mode), add/remove tags, set visibility and grants, set title/description pattern (e.g. `{album} {n}`), delete to trash, download originals as zip (task 021).
- `/admin/photos/{short_id}`: metadata editor (title, description, tags, taken_at override, place override, visibility, grants), derivative list with sizes, ingest status and failure reason, retry ingest, **replace original**: upload a new file (tus, task 012) that takes over this photo row: new hash and derivatives, same id/short id/URL/likes/flags/analytics; old original kept in trash for 30 days.
- `/admin/trash`: soft-deleted albums and photos with restore and purge (purge requires a typed confirmation and runs `gc`).
- `/admin/flags`: open flags grouped by photo, showing flagger (user or share link), time; actions per photo: replace (jumps to replace flow), re-edit (downloads original), remove (trash), change visibility, dismiss; resolving records `resolution` and audit.
- `/admin/likes`: most liked photos, likes per user, per-photo likers.
- `/admin/users`: list, create (generates temporary password shown once), disable/enable, reset temporary password, group membership, sessions with revoke, MFA status (cannot view secrets), delete (soft, with confirmation).
- `/admin/groups`: create, rename, members.
- `/admin/links`: share links table (target, flags, expiry, passphrase yes/no, opens, unique visitors, last open), create from any album or photo page (button there too), edit toggles and expiry, revoke, copy URL (only at creation).
- `/admin/jobs`: queue depth by kind, running jobs, failed jobs with error and retry/delete, recent completions.
- `/admin/audit`: filterable audit log.

### API and islands
Each page is server-rendered; islands handle drag-and-drop, multi-select, and inline edits through `/admin/api/*` JSON endpoints that require the admin session, MFA-complete session (`mfa_at`), CSRF header, and same-origin. Bulk endpoints take up to 1,000 ids per request and run in one transaction. Every mutation writes `audit_log` with before/after for changed fields (secrets never logged).

### Cache invalidation
Mutations that affect public pages purge the relevant cache tags: in-process caches immediately, and the edge via a `CachePurger` trait (Cloudflare implementation reads an API token from config; no-op implementation when unset; tested with a mock).

### Tests
- Every admin route returns 404 on the public listener (generated test over the router table).
- Every mutation without CSRF or from a non-MFA session is rejected.
- Bulk move/visibility over 1,000 photos is one transaction and one audit row per action with the id list.
- Replace-in-place keeps id, short id, likes, flags, events; old original goes to trash; derivatives regenerate.
- Album rename creates a legacy redirect and the old URL 301s.
- Purge removes originals only when no other photo row references the hash.
- Flag resolution flows.
- Accessibility: keyboard-only reorder test (via the API the keyboard actions call).

## Acceptance

- [ ] All tests above pass.
- [ ] Walkthrough screenshots of each page in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.2/4.3: admin guard, MFA-complete session, CSRF, audit on everything. Markdown descriptions sanitized.
- Admin-only routes compiled only with the `admin` feature.
- Allowed new dependencies: `pulldown-cmark`, `ammonia` (if not present).

## Performance

- Admin pages for an album with 1,000 photos render under 100 ms server-side; bulk operations on 1,000 photos under 1 s.

## Not in this task

- Face/person management (task 014 admin UI), theme editor (016), analytics dashboard (017).
