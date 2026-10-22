---
id: 006
slug: codec-sandbox
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [001]
crates: [yhotos-codec, yhotos-codec-host]
---

# 006 Codec crate and sandboxed codec host

## Goal

`yhotos-codec` wraps libvips (and, behind features, libheif and RAW decoding) to identify an image from its header, decode it color-managed into a 16-bit linear working image, resize with Lanczos3, and encode AVIF, JPEG XL, and JPEG with the ICC profile preserved and orientation applied. `yhotos-codec-host` is a separate binary that runs one job per process under resource limits and a seccomp filter, reads a JSON job from stdin, writes outputs to a given directory, and prints a JSON report. The worker (later task) talks only to the host. This task makes the pipeline's correctness and fidelity testable on real files.

## Read first

- `AGENTS.md`
- `docs/scope.md` section 4
- `docs/architecture.md` section 6
- `docs/threat-model.md` sections 4.1 (decoders, bombs, polyglots, EXIF) and B5
- `docs/performance.md` sections 1 (pipeline and fidelity gates) and 3

## Scope

`crates/yhotos-codec/`, `crates/yhotos-codec-host/`, `fixtures/images/` (small files only, see below), `crates/yhotos-bench/benches/codec.rs`, `fuzz/fuzz_targets/identify.rs`, `deploy/dev/Dockerfile.codec` (build image with libvips, libheif, libjxl, libaom/rav1e, for CI and for machines without libvips). Workspace `Cargo.toml` for new dependencies only.

## Design

### yhotos-codec
The only crate allowed `unsafe`. `#![deny(unsafe_op_in_unsafe_fn)]`, and every `unsafe` block has a `// SAFETY:` comment. Use the `libvips` crate if it supports the needed operations with a current libvips (8.15+); otherwise write a minimal `vips-sys` module with `bindgen` at build time (a `build.rs` is allowed in this crate only, and it must only link, never download). Decide in the first hour and record the reason in the PR.

```rust
pub struct Limits { pub max_pixels: u64 (default 250 M), pub max_bytes: u64, pub max_dim: u32 (default 65_535) }
pub enum Kind { Jpeg, Png, Tiff, Heif, Avif, Jxl, Webp, Raw(RawMake), Unknown }
pub struct Identity { pub kind: Kind, pub width: u32, pub height: u32, pub orientation: u8, pub has_icc: bool, pub icc_desc: Option<String>, pub bits: u8, pub has_alpha: bool, pub has_gain_map: bool, pub pages: u32 }
pub fn identify(path: &Path, limits: &Limits) -> Result<Identity, CodecError>;   // headers only, no pixel decode; sniff magic bytes first and require agreement with the decoder
pub struct Working { /* vips image, 16-bit, linear scRGB or XYZ-ish working space, orientation applied, alpha preserved */ }
pub fn decode(path: &Path, id: &Identity, limits: &Limits) -> Result<Working, CodecError>;
pub struct ResizeSpec { pub long_edge: u32, pub square: bool }
pub fn resize(w: &Working, spec: ResizeSpec) -> Result<Working, CodecError>;   // Lanczos3, never upscales, square = center crop
pub enum Encode { Avif { quality: u8, speed: u8 }, Jxl { distance: f32, effort: u8 }, Jpeg { quality: u8, progressive: bool, chroma_subsampling: bool } }
pub struct EncodeOpts { pub keep_icc: bool /* false converts to sRGB */, pub strip_metadata: bool /* always true for derivatives; GPS never survives */ }
pub fn encode(w: &Working, enc: Encode, opts: EncodeOpts, out: &Path) -> Result<Encoded { bytes, width, height }, CodecError>;
pub fn thumbhash(w: &Working) -> Result<Vec<u8>, CodecError>;    // 100x100 max, per the thumbhash spec; implement in Rust, ~150 lines, with the reference vectors
pub fn dominant_colors(w: &Working, n: u8) -> Result<Vec<[u8; 3]>, CodecError>;  // simple median cut on a 64x64 downsample
pub struct Metadata { exif subset: make, model, lens, focal_mm, aperture, shutter, iso, taken_at, gps (lat, lon) option, orientation; xmp: rating option, title option; icc_desc }
pub fn metadata(path: &Path) -> Result<Metadata, CodecError>;    // via libvips' exif/xmp access or `nom-exif`; all strings length-capped at 256, lossy UTF-8
```
HDR: detect gain maps (ISO 21496-1 / Adobe / Apple variants) at identify time; expose `has_gain_map`. Encoding HDR variants is a later task; this task must not drop the gain map silently: `Encode` has a `hdr: bool` reserved field that returns `Unsupported` when true.
RAW: feature `raw` using `rawler` to extract the embedded preview JPEG (largest), falling back to a demosaic to 16-bit; return it as a `Working`. Identify reports `Kind::Raw(make)`.
HEIC: feature `heif` via libvips' heif loader (libheif linked).

### yhotos-codec-host
Binary. Reads one `Job` JSON from stdin:
```json
{ "input": "/in/original", "out_dir": "/out", "limits": {...},
  "outputs": [ { "name": "medium.avif", "resize": {"long_edge":1280,"square":false}, "encode": {"Avif":{"quality":60,"speed":6}}, "keep_icc": true } ],
  "want": ["identity","metadata","thumbhash","dominant_colors"] }
```
Writes each output to `out_dir/<name>` and prints a `Report` JSON to stdout: identity, metadata, thumbhash (base64), dominant colors, per-output sizes and dimensions and the BLAKE3 hash of each output, and timings. Exits 0 on success, 2 on a rejected input (bad magic, over limits), 3 on a decode failure, 4 on timeout, with a one-line error on stderr and nothing else.

Sandboxing, applied by the host to itself at startup, before reading the job:
- `setrlimit`: `RLIMIT_AS` from `limits.max_memory` (default 3 GiB), `RLIMIT_CPU` (default 120 s), `RLIMIT_FSIZE` (default 2 GiB), `RLIMIT_NOFILE` 64, `RLIMIT_NPROC` 0 where the platform allows.
- Linux: `seccomp` filter via `seccompiler` allowing only the syscalls libvips needs (enumerate by running the test suite under `strace -f -c` and allow-listing; record the list in `src/seccomp.rs` with a comment per group). Deny `socket`, `connect`, `execve`, `ptrace`, `mount`, `clone` with `CLONE_NEWUSER`, and everything else. `prctl(PR_SET_NO_NEW_PRIVS)` first.
- macOS: `sandbox_init` is deprecated; apply rlimits only and document that the production sandbox is Linux. Tests of the seccomp layer are `#[cfg(target_os = "linux")]`.
- Wall clock: the parent (worker) enforces it; the host also installs an `alarm` as a backstop.
- Threads: `VIPS_CONCURRENCY` set to 2 by default; the worker runs several hosts in parallel instead.

### Fixtures (`fixtures/images/`)
Generated by `scripts/gen-fixtures.sh` using libvips' CLI so nothing binary is hand-made, each under 200 KB, committed:
- `chart-srgb.png`, `chart-p3.png`, `chart-adobergb.jpg`: a synthetic 24-patch color chart with known Lab values embedded as a JSON sidecar, with the respective ICC profiles (ICC files come from the `icc-profiles` directory of libvips' test data or are generated with `lcms2`'s `tifficc`; if neither is available offline, use the sRGB and Display P3 profiles that macOS and libvips ship).
- `orientation-{1..8}.jpg`: the same arrow image with every EXIF orientation.
- `alpha.png`, `16bit.png`, `16bit.tif`, `cmyk.jpg`, `gray.jpg`, `progressive.jpg`, `tiny-1x1.jpg`, `wide-8000x100.jpg` (to test max_dim logic without a huge file).
- `exif-heavy.jpg` with GPS and lens data written by `exiftool` if available, else by libvips' `vips copy ... [exif-...]` setters.
- `malicious/`: `bomb-png.png` (1 KB declaring 60000x60000), `truncated.jpg`, `polyglot-jpg-html.jpg`, `zero.jpg`, `wrong-ext.png` (a JPEG), `huge-dims-tiff.tif` (header only).
- HEIC, JXL, AVIF samples: encode from the chart with libvips (`.heic` needs libheif with x265 or aom; if the build cannot encode HEIC, commit a sample encoded on a machine that can and say so in the PR).
- RAW: not synthesizable. Leave a `README` listing the files expected in `fixtures/large/raw/` (synced from tower) and make RAW tests skip when absent.

### Tests
- Identify on every fixture: kind, dims, orientation, icc, alpha, bits.
- Malicious: every file in `malicious/` is rejected by `identify` or `decode` with the expected error variant; none panics; none takes more than 2 s.
- Orientation: all 8 decode to pixel-identical arrows (compare to orientation 1).
- Fidelity: for each chart, decode, resize to 50 %, encode to each format, decode the result (with libvips as oracle), convert to Lab, mean delta E 2000 over patches < 1.0 and max < 2.5 (budgets in `docs/performance.md`); ICC present in AVIF and JXL output (parse the container or re-identify); JPEG below `medium` converted to sRGB has `icc_desc` sRGB or none.
- SSIM: resize of a photographic fixture compared against `vips thumbnail` reference at the same size >= 0.98.
- Metadata: `exif-heavy.jpg` fields parse; strings over 256 bytes are truncated; GPS present in metadata but absent from every encoded output (re-read with `identify`+`metadata` on the outputs).
- Host: end-to-end run of the binary on `chart-srgb.png` with three outputs; report parses; hashes match the files; exit codes for a malicious input (2), a missing file (2), and a timeout (4, simulate with a tiny `RLIMIT_CPU` and a large synthetic resize loop under `#[cfg(target_os = "linux")]`).
- Seccomp (linux only): the host, after applying the filter, cannot `connect()` (test by a debug flag `--self-test-network` that attempts a UDP socket and must be killed or get EPERM).

### Bench
`codec.rs`: identify (under 1 ms), decode+resize+encode medium AVIF/JXL/JPEG for a 24 MP fixture from `fixtures/large/` (skip if absent; record on a machine that has it).

### Fuzz
`identify.rs`: arbitrary bytes written to a temp file, `identify` must return `Err` or a sane `Identity`, never panic, never exceed limits.

## Acceptance

- [ ] All tests above pass on Linux in `deploy/dev/Dockerfile.codec` (CI runs the codec tests inside that image; add the job to `ci.yml` in this task, pinned image digest).
- [ ] Tests pass on macOS with Homebrew libvips except seccomp tests (documented).
- [ ] `fixtures/images/` total under 3 MB; `scripts/gen-fixtures.sh` regenerates them deterministically (run twice, `git status` clean, or document which encoders are nondeterministic).
- [ ] No `unsafe` outside `yhotos-codec`; every `unsafe` block has a `SAFETY:` comment; `cargo geiger` or a `rg` count in the PR.
- [ ] Fuzz target runs 60 s without a crash.
- [ ] Benchmarks and numbers in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model 4.1 is the checklist: sandbox (rlimits, seccomp, no network, no exec), header-only size checks before decode, `max_pixels`, magic-byte agreement with the decoder, metadata length caps, GPS stripped from derivatives. Map each to code and test in the PR.
- The host must not read any path other than `input` and must not write outside `out_dir`; enforce by canonicalizing and checking prefixes, and by the seccomp `openat` policy being read-only outside `out_dir` where feasible (if path-based filtering is impractical in seccomp, document that the worker runs the host with `out_dir` as its only writable mount in production).
- libvips `VIPS_WARNING` and other env-driven behavior: clear the environment in the host except a fixed allow list.
- Allowed new dependencies: `libvips` (or `bindgen` + `pkg-config`), `rawler`, `nom-exif` (if libvips' metadata access is insufficient), `seccompiler`, `nix` (rlimit, prctl), `serde`, `serde_json`, `blake3`, `base64`, `image` (dev-dependency only, for pixel comparisons), `kolor` or hand-written Lab/delta E (hand-written, 60 lines, preferred).

## Performance

- Budget: 30 MP JPEG to all 13 variants under 4 s wall clock with `VIPS_CONCURRENCY=2`; this task measures medium only; record it.
- Do not decode twice: `decode` once, `resize` per variant from the largest-needed intermediate (resize chain: decode -> xlarge -> large -> ... is acceptable only if each step is from the original working image, not a chained downscale of a downscale; chained downscales reduce quality. Resize each variant from the working image.)

## Not in this task

- The worker loop, job claiming, moving outputs into the store (task 007).
- HDR gain-map encoding (task later; `Unsupported` for now).
- Video.
