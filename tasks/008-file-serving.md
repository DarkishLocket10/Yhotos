---
id: 008
slug: file-serving
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [002, 003, 004]
crates: [yhotos-http]
---

# 008 Serving derivatives, originals, and video

## Goal

The `/i/{hash}/{variant}.{fmt}`, `/o/{hash}/{filename}`, and `/v/{hash}/{codec}.mp4` routes exist on the public router and serve files from the store with the caching, access control, and streaming semantics in `docs/architecture.md` sections 5 and 8. This is the hottest path in the system and the one the edge caches; it must be correct about authorization before it is fast.

## Read first

- `AGENTS.md`
- `docs/architecture.md` sections 4 (visibility), 5, 8
- `docs/threat-model.md` 4.3 (derivatives are not authorized by hash alone), 4.4 (DoS, information leakage)
- `docs/performance.md` section 1 (derivative budget: p99 < 5 ms before first byte)

## Scope

`crates/yhotos-http/src/files/` (new module), `crates/yhotos-http/src/state.rs` (add `Store` and `PgPool` to `AppState` if task 002/003 did not), `crates/yhotos-db/src/photos.rs` (one new query, see Design), `crates/yhotos-bench/benches/files.rs`.

## Design

### Lookup and authorization
One query, `photos::owner_of_hash(hash) -> Option<OwnerRow { photo_id, photo_visibility, album_id, album_visibility, album_ancestors_visibility: Vec<Visibility>, grants (user ids, group ids) for photo and album chain, kind, mime, bytes, width, height }>` for originals, and `derivatives::lookup(hash, variant, fmt, hdr) -> Option<DerivativeRow + the same owner info>` for derivatives, using a single SQL statement with a recursive CTE for ancestors. Cache the result in-process with `moka` (TTL 60 s, max 50k entries, keyed by the full path) because the edge will still miss on the long tail; invalidate on admin changes via a `cache_version` setting bump (simplest: TTL only, documented).

Then `yhotos-core::access::can_view_photo(principal, ...)`. Principal comes from the session loader (task 009 provides real sessions; until then the loader yields `Anonymous`, and the share-link principal is `None`; design the handler against the `Principal` enum so no change is needed later).

Decision table:
- Not found in DB: 404.
- Visible to anonymous (public): serve with `Cache-Control: public, max-age=31536000, immutable`, `Cross-Origin-Resource-Policy: cross-origin`.
- Restricted and the principal may view: serve with `Cache-Control: private, no-store`, `Vary: Cookie`.
- Restricted and may not view: 404, identical body and headers to the not-found case (test byte-equality).

### Response
- `ETag: "<hash>"` (strong), `Last-Modified` omitted (immutable content), `If-None-Match` honored with 304.
- `Content-Type` from the row (`image/avif`, `image/jxl`, `image/jpeg`, `video/mp4`, originals from `photos.mime`). Never sniffed.
- `Content-Length` always; `Accept-Ranges: bytes`; single-range `Range` requests honored (206, `Content-Range`), multi-range rejected with 416 to keep it simple; invalid ranges 416.
- `Content-Disposition: inline; filename="<safe>"` for originals using the stored original name sanitized to `[A-Za-z0-9._-]`, with `?download=1` switching to `attachment`.
- Body: `tokio::fs::File` streamed via `tokio_util::io::ReaderStream` with a 256 KiB buffer; no read of the whole file. Use `sendfile`-equivalent if `axum`/`hyper` offer a zero-copy path; otherwise document the measured throughput.
- `HEAD` supported.
- `X-Content-Type-Options: nosniff` already from middleware.

### Rate limits
Originals and video use the `expensive` bucket; derivatives use `cheap`. Add a per-IP concurrent-stream cap for originals (default 4) with 429 beyond it.

### Video
Same as originals with `video/mp4` and range support (required for seeking). HLS is not in this task.

### Bench
`files.rs`: in-process request to `/i/...` for a public derivative, store on tmpfs, DB replaced by the moka-cached row (warm). Budget: p99 < 5 ms to first byte; also measure the cold path with a real Postgres.

## Acceptance

- [ ] Public derivative: 200, correct headers, byte-exact body, 304 on `If-None-Match`.
- [ ] Restricted derivative: anonymous gets a 404 byte-identical to a missing hash; a principal with a grant gets 200 with `private, no-store`.
- [ ] Original with `Range: bytes=0-99` returns 206 with 100 bytes and correct `Content-Range`; `bytes=5-2` and `bytes=0-0,2-3` return 416; `HEAD` returns headers without a body.
- [ ] `Content-Type` comes from the database (test by storing a wrong extension in the path and asserting the row's type wins), and the file is never sniffed.
- [ ] A hash that is syntactically invalid (uppercase, 63 chars, path chars) returns 404 without touching the filesystem (test with a store rooted at a nonexistent path).
- [ ] A derivative row whose file is missing on disk returns 404 and logs at `error` with the request id (test).
- [ ] Concurrent-stream cap for originals returns 429 on the 5th simultaneous stream from one IP.
- [ ] Benchmarks and numbers in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model 4.3: the hash is not an authorization. The PR must point to the access check in the handler and the byte-equality 404 test.
- Threat model 4.4: streaming only; range math is fuzzed (add `fuzz/fuzz_targets/range_header.rs`), `Content-Disposition` filename is sanitized (test with quotes, newlines, unicode, path separators).
- No directory listing, no fallback to filesystem paths, no `Last-Modified` from the filesystem (would leak timing).
- Allowed new dependencies: `moka`, `tokio-util`, `http-range-header` or hand-written range parsing (hand-written, with the fuzz target, preferred).

## Performance

- One query per cold request, zero per warm request; prove with the query-count test pattern from task 003.
- No allocation of the file body; `ReaderStream` chunks only.

## Not in this task

- HTML pages that reference these URLs.
- Share-link principal resolution (task 010) and sessions (task 009); design the handler against `Principal` so they plug in.
- Zip downloads.
