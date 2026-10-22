---
id: 019
slug: hdr-gain-map-encoding
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [007]
crates: [yhotos-codec, yhotos-codec-host, yhotos-pipeline]
---

# 019 HDR derivatives with gain maps

## Goal

Photos whose originals carry an HDR gain map (iPhone/Android JPEG and HEIC, Lightroom-exported JPEG/AVIF/JXL with ISO 21496-1 or Adobe/Apple gain maps) get HDR derivatives that preserve the gain map, so HDR-capable browsers show the photo as shot while SDR browsers show the SDR base. Task 006 detects gain maps and returns `Unsupported` for `Encode { hdr: true }`; this task implements it.

## Read first

- `AGENTS.md`
- `docs/scope.md` 4 (HDR fidelity)
- `docs/architecture.md` 6 (HDR variants), 9 (`<source media="(dynamic-range: high)">`)
- `docs/performance.md` 1 (gain map present in HDR variants when source has one)
- `crates/yhotos-codec/README.md` and the gain-map detection code from task 006

## Scope

Areas: `crates/yhotos-codec/` (gain map extraction, resize, encode), `crates/yhotos-codec-host/` (job protocol `hdr` outputs), `crates/yhotos-pipeline/` (variant plan adds HDR outputs when `has_gain_map`), fixtures for each gain-map container type, plus supporting files.

## Design

- Extraction: for each supported container, read the base image and the gain map image plus its metadata (min/max boost, gamma, offsets, base and alternate headroom, per-channel or single), normalized to the ISO 21496-1 model. Containers: JPEG with MPF + ISO 21496-1 or Adobe hdrgm XMP; Apple HEIC with the auxiliary HDR gain map (`urn:com:apple:photo:2020:aux:hdrgainmap`) and its metadata; AVIF with `tmap`; JXL with a `jhgm` box. Unknown or inconsistent metadata: skip HDR output for that photo (SDR only) and record `hdr_skipped_reason` in the report, never fail the ingest.
- Resizing: resize base and gain map to the same target geometry with the same orientation handling; the gain map may be lower resolution than the base, keep its relative scale (typically 1/2 or 1/4).
- Encoding targets, per variant marked `hdr` in the plan (default: medium, large, xlarge):
  - JPEG + ISO 21496-1 gain map (UltraHDR-compatible MPF layout with both ISO and Adobe hdrgm metadata for broad support); this is the most widely displayed HDR format in browsers today.
  - AVIF with `tmap` when the encoder (libavif via libvips or libheif) supports gain maps; otherwise skip AVIF HDR.
  - JXL with a `jhgm` box when libjxl supports it; otherwise skip.
  Use `libultrahdr` through a minimal FFI wrapper in `yhotos-codec` if libvips does not expose gain-map encoding; it is the reference implementation, Apache-2.0. Same `unsafe` and `SAFETY:` rules as the rest of the crate; fuzz the wrapper.
- Color: base image in its own profile (sRGB or P3, ICC embedded), gain map metadata copied with headroom values recomputed only if resizing changes nothing about them (it does not; values carry over).
- Pipeline: when `identity.has_gain_map`, add HDR outputs for the plan's HDR variants; derivatives rows with `hdr = true`; file serving and pages (tasks 008, 011) already handle them.
- Verification: decode each HDR output with libultrahdr (or the container's reference decoder) and check that the recovered HDR rendition's luminance at a few sample points matches the original's HDR rendition within tolerance (document the tolerance), and that the SDR base is bit-identical in color to the SDR derivative at the same size (same encoder settings).

### Fixtures
Small (< 300 KB each) samples for JPEG ISO 21496-1, JPEG Adobe hdrgm, Apple HEIC gain map, AVIF tmap; generated with libultrahdr/avifenc where possible (generator in `scripts/gen-fixtures.sh`), else one real iPhone HEIC from the owner's library downscaled and stripped of metadata other than the gain map (taken from `fixtures/large` on tower; record which).

## Acceptance

- [ ] Each fixture type produces HDR derivatives at the planned variants; round-trip checks pass.
- [ ] Photos without gain maps produce no HDR outputs and no errors.
- [ ] Malformed gain-map metadata leads to SDR-only output with a recorded reason, never a failed ingest (tests per malformed case).
- [ ] Fuzz target for gain-map metadata parsing runs 60 s clean.
- [ ] Benchmarks: HDR encode time per variant recorded; total per-photo pipeline still within the 4 s budget for a 24 MP HDR photo or the PR explains the delta.
- [ ] `scripts/check.sh` passes; codec CI job passes.

## Security notes

- 4.1: new parsers (MPF, XMP hdrgm, HEIC aux, tmap, jhgm) are bounded and fuzzed; they run only inside the codec host.
- Allowed new dependencies: `libultrahdr` (system library, linked; add to `Dockerfile.codec`), `quick-xml` for XMP if not present (bounded reader).

## Not in this task

- HDR video; tone-mapping HDR to SDR when no gain map exists (sources without a gain map are treated as SDR).
