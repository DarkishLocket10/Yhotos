---
id: 012
slug: upload
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [007, 009]
crates: [yhotos-http, yhotos-db, yhotos-pipeline]
---

# 012 Resumable upload (admin) and ingest trigger

## Goal

The admin listener accepts resumable, chunked uploads (tus 1.0.0 core protocol plus the creation and termination extensions), validates the finished file, stores it as an original, creates the photo row in the chosen album, and enqueues `Ingest`. A minimal admin upload page with a drag-and-drop island drives it. Everything in `docs/threat-model.md` 4.1 for uploads applies.

## Read first

- `AGENTS.md`
- `docs/scope.md` 4, 5.4
- `docs/architecture.md` 6 (upload steps 1 to 3), 8 (admin router)
- `docs/threat-model.md` 4.1 (tus abuse, polyglots, bombs), B4
- Briefs 004 (store), 007 (ingest), 009 (admin guard)

## Scope

`crates/yhotos-http/src/admin/upload/` (feature `admin`), `crates/yhotos-http/templates/default/admin/upload.html`, `web/src/upload.ts`, `crates/yhotos-db/src/uploads.rs` (+ migration for `uploads` table), `crates/yhotos-pipeline/src/validate.rs` (allow list and header checks shared with ingest), `fuzz/fuzz_targets/tus_headers.rs`.

## Design

### tus endpoints (admin router, `/admin/upload`)
- `OPTIONS /admin/upload`: `Tus-Resumable: 1.0.0`, `Tus-Version`, `Tus-Extension: creation,termination,expiration`, `Tus-Max-Size` from config.
- `POST /admin/upload`: `Upload-Length` (required, <= max), `Upload-Metadata` (base64 key/value: `filename`, `album` (AlbumId), `filetype`), creates a row in `uploads` `{id, user_id, album_id, filename, declared_bytes, received_bytes, mime_hint, tmp_path, expires_at, created_at}` and a temp file via `store.tmp_dir()`; returns `201 Location: /admin/upload/{id}` and `Upload-Expires` (24 h).
- `HEAD /admin/upload/{id}`: `Upload-Offset`, `Upload-Length`, no caching.
- `PATCH /admin/upload/{id}`: `Content-Type: application/offset+octet-stream`, `Upload-Offset` must equal the stored offset exactly (409 otherwise), body streamed to the temp file at that offset with a per-request cap (`chunk_bytes` from config, 8 MiB; 413 beyond), `received_bytes` updated in the same transaction as the write completes (write first, fsync, then update; on crash the next HEAD reports the stored offset and the client resends). Concurrent PATCHes on one upload are serialized with a `SELECT ... FOR UPDATE` and the loser gets 409.
- `DELETE /admin/upload/{id}`: termination; removes the temp file and the row.
- Completion (when `received_bytes == declared_bytes`): run `validate::finish(path)`: sniff magic bytes (allow list: JPEG, PNG, TIFF, HEIF/HEIC, AVIF, JXL, WebP, DNG/ARW/RW2/CR2/CR3/NEF/RAF/ORF via their container signatures, MP4/MOV via `ftyp`), header-only dimension and page checks via `yhotos-codec::identify` run in the codec host (not in-process; reuse the host with `want: ["identity"]` and no outputs), reject on mismatch between the sniffed type and `filetype` hint only by ignoring the hint (the hint is advisory). Then `store.put_original` from the temp file (hash while copying), dedupe (if the hash exists: do not create a second photo; return the existing photo and `Upload-Result: duplicate`), insert `photos` row (`status = pending`, `kind` from identify, `original_name` sanitized), enqueue `Ingest`, delete the `uploads` row and temp file. Response to the final PATCH carries `Upload-Result: created|duplicate` and `Location: /admin/api/photos/{short_id}`.
- Expiry: a `gc`-style job (reuse task 007's `Gc` or add `UploadsSweep`) removes expired uploads and their temp files hourly.

### Upload page and island
`/admin/upload`: album picker (tree), drop zone, file list with per-file progress, retry, cancel; uses `fetch` with the tus protocol (write it by hand in `upload.ts`, ~250 lines; no tus-js-client dependency), 4 parallel files, chunk size from a `data-` attribute, resume after reload via `localStorage` of `{fingerprint: upload url}` keyed by name+size+lastModified. Shows the ingest status by polling `/admin/api/photos/{short_id}/status` every 2 s until `ready` or `failed`. No framework; CSP nonce.

### Tests
- Protocol conformance: a Rust client in tests drives create, HEAD, PATCH in 3 chunks, completion; offsets mismatch → 409; oversize declared → 413; chunk over cap → 413; wrong `Tus-Resumable` → 412; missing `Content-Type` on PATCH → 415.
- Resume: kill the test client mid-chunk (drop the body stream), HEAD shows the fsynced offset, resend completes.
- Concurrency: two PATCHes with the same offset, exactly one succeeds.
- Validation: each file in `fixtures/images/malicious/` uploaded via the protocol ends with 422 and no `photos` row and no temp file left; `wrong-ext.png` (a JPEG) is accepted as JPEG with `mime = image/jpeg`.
- Dedupe: uploading the same bytes twice yields one photo, second reports `duplicate`.
- Authorization: the public listener has no `/admin/upload` route (404); an admin session without MFA is redirected by the guard (task 009).
- Filename sanitization: `../../etc/passwd`, unicode, 1000 chars → stored display name is safe and capped at 255.
- Expired uploads are swept and their temp files removed.

### Fuzz
`tus_headers.rs`: `Upload-Metadata` parser and `Upload-Offset`/`Upload-Length` parsing never panic.

## Acceptance

- [ ] All tests above pass.
- [ ] Manual walkthrough in the PR: upload 5 real originals from `fixtures/large/originals` through the page over Tailscale to a dev stack, watch them reach `ready`.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model 4.1 tus bullets and polyglot/bomb bullets are the checklist.
- Validation runs in the codec host, never in the server process.
- Temp files are only ever under `tmp/` from `store.tmp_dir()`; the `uploads.tmp_path` column stores the dir id, not a path.
- Admin-only; body limit middleware exempts only the PATCH route and only up to `chunk_bytes`.
- Allowed new dependencies: `infer` (magic bytes) if not already used by the codec crate, `base64`.

## Performance

- PATCH streams to disk with a 256 KiB buffer; no buffering of the whole chunk.
- Completion does not block the response on ingest; ingest is a job.
- Measure: 30 MB file in 8 MiB chunks over loopback under 300 ms total excluding validation; record.

## Not in this task

- Replacing a photo in place (task 020 admin bulk tools).
- Lightroom or watched-folder ingest (out of scope for v1).
