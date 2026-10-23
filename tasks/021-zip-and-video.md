---
id: 021
slug: zip-and-video
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [007, 008, 011]
crates: [yhotos-http, yhotos-worker, yhotos-codec-host, yhotos-pipeline]
---

# 021 Streaming zip downloads and the video pipeline

## Goal

Two remaining file features. (1) Bulk download: a whole album or a selection of photos as a zip, streamed with no temp file and no buffering, choosing original or a generated size, respecting visibility and share-link capabilities. (2) Video: uploaded videos are transcoded in the sandbox to H.264 MP4 and AV1 MP4 with a poster frame and scrub thumbnails, and play inline in the grid and lightbox.

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.1 (bulk download), 5.9 (video)
- `docs/architecture.md` 6 (video), 8 (`/zip`, `/v/`)
- `docs/threat-model.md` 4.1 (video parsing), 4.4 (DoS on expensive routes)
- `docs/performance.md` 1, 3 (streaming)

## Scope

Areas: `crates/yhotos-http/src/zip/`, `crates/yhotos-codec-host/` (video entry point), `crates/yhotos-pipeline/src/video.rs`, `crates/yhotos-worker/` (VideoTranscode handler), templates and islands for video playback, `deploy/dev/Dockerfile.codec` (ffmpeg), fixtures (short videos under 300 KB), plus supporting files.

## Design

### Zip
- `GET /zip?album={id}&size={original|large|medium}` and `POST /zip` with a JSON id list (max 500) for selections, both resolving visibility per photo with the same access function and share-link caps (`allow_download`).
- Stream with `async_zip` (or a hand-written STORE-only writer: images are already compressed, so no deflate; ZIP64 for archives over 4 GB). Entries are added as each file is opened; nothing is buffered beyond the copy buffer. `Content-Disposition: attachment; filename="<album-slug>.zip"`. No `Content-Length` (chunked).
- Entry names: `NNN-<sanitized title or short id>.<ext>`, unique within the archive, no path separators, no traversal.
- Limits: per-IP concurrency 1 for zips, global concurrency from config (default 4), max archive size from config (default 20 GB), uses the `expensive` rate bucket and the body-lifetime permits from task 008 (a stalled client releases the slot via the write timeout).
- Records `zip_download` analytics events (task 017 consumes them).

### Video
- Host: `yhotos-codec-host --video` supervises direct ffprobe/ffmpeg execs. Before exec each child sets AS/CPU/FSIZE/NOFILE/CORE rlimits, no-new-privileges, parent-death SIGKILL, its own process group, and closes non-stdio descriptors. Landlock ABI v3 is mandatory: input read-only, output directory read/write, EXECUTE on exactly the configured ffmpeg/ffprobe files, static ELF ffmpeg/ffprobe binaries are required and checked for absence of PT_INTERP at startup, so they need no loader/library-directory grants. Dynamically linked parsers are refused because Landlock would require an additional execute grant on their loader. Missing confinement fails closed with a recorded reason. Seccomp permits execve/execveat and inspected pthread clone flags, returns ENOSYS for clone3, and denies fork/vfork, networking, ptrace, mount and other non-allowlisted calls. The supervisor kills the child process group on timeout. Probe first with strict limits (duration <= 30 min from config, resolution <= 8K, streams count <= 4, known codecs only); reject anything else.
- Renditions: H.264 High profile MP4, max 1080p long edge, CRF 21, `+faststart`; AV1 (SVT-AV1) MP4, source resolution up to 2160p, CRF 30, `+faststart`; audio AAC 160k in both. Poster JPEG at 10 % into the video (and AVIF/JXL variants through the normal image path). Scrub sheet: 10x10 sprite at 160 px plus a WebVTT index.
- Pipeline: `VideoTranscode` job runs the host with a longer timeout (config, default 30 min) and lower priority than image ingest; writes `video_renditions` rows; marks the photo ready.
- Playback: grid shows the poster with a play badge and duration; lightbox uses `<video controls preload="metadata" poster=...>` with `<source>` AV1 then H.264; scrub preview uses the sprite. `/v/` routes from task 008 serve renditions with range support.
- HDR video is out of scope (transcoded to SDR BT.709 with tone mapping `zscale`+`tonemap=hable`; document).

### Tests
- Zip: visibility (restricted photo absent for anonymous), share-link `allow_download = false` yields 404, entry names safe, ZIP64 correctness for a synthetic large archive (sparse files), concurrency limits and slot release on client stall, archive opens with the system `unzip` and checksums match originals.
- Video: fixture MP4/MOV (H.264, HEVC), iPhone-style MOV with rotation metadata, malformed and oversize-duration samples rejected, renditions playable (`ffprobe` checks), poster generated, job retry on timeout, seccomp/Landlock still enforced in video mode (linux-only test).

## Acceptance

- [ ] All tests above pass; codec CI job runs video tests with ffmpeg in the image.
- [ ] Manual (staging task 023, pending): a 1-minute 4K iPhone video uploads, transcodes, and plays in the lightbox on desktop and mobile (record timings).
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.1: ffmpeg is the largest parser in the system; it runs only inside the sandboxed host with probe limits, no network, filesystem confinement, and resource limits. Pin the ffmpeg version in the Dockerfile and list enabled demuxers/decoders (build with `--disable-everything` plus an explicit allow list if building from source; otherwise restrict via `-f`/codec allow lists on the command line).
- 4.4: zip and video endpoints are expensive and are rate limited and concurrency capped.
- Allowed new dependencies: `async_zip` (or hand-written writer, preferred if under 400 lines with tests), `crc32fast`.

## Performance

- Zip streaming throughput at least 200 MB/s from local disk; CPU under 10 % of a core per stream (STORE only).

## Not in this task

- HLS adaptive streaming (only if a later measurement shows need).
