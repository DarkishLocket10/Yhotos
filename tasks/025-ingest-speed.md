---
id: 025
slug: ingest-speed
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [007, 019]
crates: [yhotos-pipeline, yhotos-codec, yhotos-codec-host, yhotos-bench]
---

# 025 Ingest speed: all derivatives within budget on Linux, fidelity unchanged

## Goal

Generating every derivative for one photo currently takes about 28 s for a 24 MP source on yesktop (Linux, release build, task 019's measurement: 29 outputs, SDR outputs 26.5 s, HDR 0.8 s). The budget in `docs/performance.md` is under 4 s wall clock per photo with 8 photos in parallel on tower, written when the plan had 13 files. The Piwigo migration alone is 4,925 photos (average about 31 MP), so this decides whether migration takes hours or days. Make ingest fast without loosening a single fidelity gate.

## Read first

- `AGENTS.md`, `docs/performance.md` (ingest budget, measurement rules), `docs/architecture.md` (pipeline, derivative plan)
- Briefs 006, 007, 019; `crates/yhotos-pipeline/src/plan.rs` (the plan and per-format quality settings)

## Scope

Areas: `crates/yhotos-pipeline/`, `crates/yhotos-codec/`, `crates/yhotos-codec-host/`, `crates/yhotos-bench/` (a per-output timing benchmark), `docs/performance.md`, plus supporting files. Run and measure on Linux with the production codec stack (this host, and the codec Docker image); macOS numbers do not count.

## Design

1. **Measure first.** Add `cargo bench -p yhotos-bench --bench pipeline_breakdown` that ingests a real-photo 24 MP fixture (generate deterministically from the repository's photo fixtures; no network) and reports wall time per stage: identify, decode, colour conversion, each resize, each encode per variant x format, HDR, thumbhash, hashing and storage writes. Record the breakdown table in the PR before changing anything.
2. **Then fix the biggest costs, measuring after each change.** Candidates to evaluate (adopt only what measures faster with gates still passing):
   - Decode once and derive every size from the decoded image (or a resize cascade from the next larger output) instead of re-decoding or re-resizing from the original; use shrink-on-load where the format supports it.
   - Encode outputs in parallel within one photo, sized to the host (respecting `vips_concurrency` and the 8-photos-in-parallel budget so total threads stay bounded).
   - Encoder speed settings: AVIF/libaom `speed`/`cpu-used`, JXL `effort`. Pick the fastest setting that still passes every fidelity gate at the same quality targets; record file-size changes too (bandwidth matters, so a faster setting that grows files more than 5 % needs justification).
   - Avoid redundant colour conversions and ICC transforms; avoid copying pixel buffers between stages.
3. **Fidelity gates are fixed:** SSIM >= 0.98 for large and up (>= 0.97 below), mean deltaE < 1.0, max deltaE < 2.5, as tuned against the Linux codec image. No gate, quality target or output may be dropped to win time.
4. **Reconcile the budget.** Update `docs/performance.md` with the current output set (29 files including 2x and HDR variants) and a budget expressed for it, measured on this Linux host and stated as a target for tower (8 photos in parallel). Keep the 4 s figure unless the measurements prove it impossible with the gates; if so, state the achieved number and why.

## Tests

- Fidelity gate tests still pass for every output, unchanged thresholds.
- Byte-identical or gate-passing outputs before and after for the fixture set (record which).
- The new benchmark runs in `cargo bench` and prints the breakdown.
- Concurrency stays bounded: a test that ingest of one photo never runs more encoder threads than configured.

## Acceptance

- [ ] Breakdown table before and after, per output, in the PR.
- [ ] 24 MP photo wall time on this Linux host within budget, or a measured explanation of the remaining gap and what would close it.
- [ ] `scripts/check.sh` and the codec CI job pass.

## Security notes

- 4.1: all decoding stays inside the sandboxed codec host; parallelism must not weaken rlimits, seccomp or Landlock (threads only, no new processes).

## Not in this task

- Changing which variants or formats exist, or the quality targets themselves.
