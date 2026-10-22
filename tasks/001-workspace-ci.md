---
id: 001
slug: workspace-ci
model: gpt-6.1-sol
effort: high
review_effort: high
depends_on: []
crates: [all]
---

# 001 Workspace, tooling and CI

## Goal

A Cargo workspace with every crate from `docs/architecture.md` section 2 present as a compiling skeleton, the shared lints and release profile set, `scripts/check.sh` that runs exactly what CI runs, and GitHub Actions workflows for CI, CodeQL, and secret scanning. After this task any later task can add code to its crate and get fmt, clippy, tests, deny, audit, and gitleaks on its PR.

## Read first

- `AGENTS.md`
- `docs/architecture.md` sections 2 and 3
- `docs/threat-model.md` section 4.6
- `docs/performance.md` section 3 (release profile)

## Scope

May create or change: `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml`, `.gitleaks.toml`, `crates/*` (skeletons only), `scripts/check.sh`, `.github/workflows/*`, `.github/CODEOWNERS`, `web/` (package.json and esbuild config only), `README.md` (a "Development" section with the commands).

## Design

Workspace `Cargo.toml`:
- `[workspace] members = ["crates/*"] resolver = "3"`.
- `[workspace.package]`: `edition = "2024"`, `license = "AGPL-3.0-only"`, `rust-version` matching `rust-toolchain.toml` (current stable).
- `[workspace.lints.rust]`: `unsafe_code = "forbid"` is NOT set workspace-wide (codec needs it); instead every crate except `yhotos-codec` has `#![forbid(unsafe_code)]` in `lib.rs`/`main.rs`, and `yhotos-codec` has `#![deny(unsafe_op_in_unsafe_fn)]` with a module comment explaining the rule. Workspace lints: `missing_docs = "warn"`, `unused_must_use = "deny"`.
- `[workspace.lints.clippy]`: `all = "deny"`, `pedantic = "warn"`, `unwrap_used = "deny"`, `expect_used = "deny"`, `panic = "deny"`, `indexing_slicing = "warn"`, `dbg_macro = "deny"`, `print_stdout = "deny"` (allowed per-crate in binaries' `main.rs`), `todo = "deny"`, `unimplemented = "deny"`. Tests may `#![allow(clippy::unwrap_used, clippy::expect_used)]` at module level.
- `[profile.release]`: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`, `opt-level = 3`. `[profile.bench]` inherits release with `debug = 1`.
- `[workspace.dependencies]` with exact versions (`=x.y.z`) for the crates every task will need: `tokio`, `axum`, `tower`, `tower-http`, `sqlx` (postgres, runtime-tokio, uuid, time/jiff, json), `uuid` (v7), `serde`, `serde_json`, `thiserror`, `anyhow`, `tracing`, `tracing-subscriber`, `secrecy`, `blake3`, `subtle`, `criterion`, `proptest`, `tempfile`. Pick the current stable versions at task time and record them in the PR.

Crates: every crate in architecture section 2 exists with a `Cargo.toml`, a `lib.rs` (or `main.rs` for binaries) containing the forbid line, a crate-level doc comment stating its responsibility (copy from the architecture doc), and one trivial test so `cargo test` exercises it. No real code. Binaries print nothing and exit 0.

`rust-toolchain.toml`: channel = the current stable version, components `rustfmt`, `clippy`.

`scripts/check.sh` (bash, `set -euo pipefail`): `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `cargo deny check`, `cargo audit`, `gitleaks detect --no-banner --redact` if `gitleaks` is on PATH (warn and continue if not). Exits non-zero on the first failure and prints which step failed.

`deny.toml`: advisories deny unmaintained and yanked; licenses allow list `MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, MPL-2.0, AGPL-3.0-only`, everything else denied; `sources` allow only crates.io; `bans` multiple-versions = warn.

`.github/workflows/ci.yml`: triggers `pull_request` and `push` to `main`. Top-level `permissions: contents: read`. Jobs: `check` (fmt, clippy), `test` (with a `postgres:16` service, `DATABASE_URL` env, `sqlx` offline mode acceptable), `deny` (`EmbarkStudios/cargo-deny-action`), `audit` (`rustsec/audit-check` or `cargo audit`), `gitleaks` (`gitleaks/gitleaks-action`). Every action pinned to a full commit SHA with a version comment. Cache with `Swatinem/rust-cache`. Concurrency group per ref with cancel-in-progress.

`.github/workflows/codeql.yml`: CodeQL for `rust` and `javascript-typescript`, on PR, push to main, and weekly. Pinned SHAs. `permissions: security-events: write, contents: read`.

`.github/CODEOWNERS`: `* @DarkishLocket10`, and explicit lines for `crates/yhotos-auth/`, `crates/yhotos-codec/`, `crates/yhotos-codec-host/`, `crates/yhotos-storage/`, `docs/`, `.github/`.

`web/`: `package.json` with `esbuild` as the only dependency, exact version, a `build` script that bundles `web/src/*.ts` to `crates/yhotos-http/assets/js/` as ES2022 modules, minified, with sourcemaps off. `tsconfig.json` strict. One placeholder `web/src/noop.ts`. `package-lock.json` committed. No framework.

`README.md`: add a Development section: prerequisites (Rust stable, Docker for Postgres, libvips for the codec crate later), `scripts/check.sh`, how to run tests with Postgres.

## Acceptance

- [ ] `cargo build --workspace` succeeds on a clean checkout with no system libraries beyond the Rust toolchain (codec crate is a skeleton without libvips for now).
- [ ] `scripts/check.sh` passes locally.
- [ ] Every non-codec crate has `#![forbid(unsafe_code)]`; `rg -L "forbid\(unsafe_code\)" crates/*/src/lib.rs crates/*/src/main.rs` lists only `yhotos-codec`.
- [ ] CI workflow runs on the PR and all jobs are green.
- [ ] CodeQL workflow is valid (runs on the PR, even if it finds nothing).
- [ ] All Actions pinned by SHA (`rg "uses: .*@v" .github` returns nothing).
- [ ] `cargo deny check` passes with the allow list above.
- [ ] `npm ci && npm run build` in `web/` produces `crates/yhotos-http/assets/js/noop.js`.

## Security notes

- Threat model 4.6: SHA-pinned actions, least `permissions`, no secrets used, `cargo deny` sources restricted to crates.io.
- Do not add any dependency not listed in Design. Do not add a `build.rs` anywhere.

## Performance

- Release profile as specified. No benchmarks yet; `yhotos-bench` exists with an empty `benches/` and criterion as a dev dependency.

## Not in this task

- Any real implementation in any crate.
- Dockerfiles, compose, deploy (later task).
- Lighthouse and benchmark workflows (later task, once there is something to measure).
- Self-hosted runner setup.
