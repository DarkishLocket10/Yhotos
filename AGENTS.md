# Rules for agents working on Yhotos

You are working on a public, internet-facing photo gallery written in Rust. Read this file fully before touching anything. Then read the brief you were given in `tasks/`, and the docs it points to. Do not start from assumptions about what the project is.

## Where things are

- `docs/scope.md`: what the product is and is not. The source of truth for product decisions.
- `docs/architecture.md`: crates, boundaries, data model, URL scheme, conventions. Follow it; do not invent parallel structures.
- `docs/threat-model.md`: what we defend against and how. Every change touching input, auth, files, or the network must be checked against it.
- `docs/performance.md`: budgets and how they are measured. A change that regresses a budget is not done.
- `docs/process.md`: how tasks flow from brief to merged PR.
- `tasks/`: one brief per task. Your brief defines your scope. Nothing outside it.

## Hard rules

Security
- `#![forbid(unsafe_code)]` in every crate except `yhotos-codec`. Do not add `unsafe` anywhere else, and do not add it to `yhotos-codec` without a comment explaining the invariant and a test that exercises it.
- Every byte from outside the process is untrusted: HTTP bodies, headers, query strings, uploaded files, filenames, EXIF, database rows written by other components, environment variables. Validate at the boundary, then use typed values inside.
- SQL only through `sqlx` query macros with bound parameters. Never build SQL from strings.
- No secrets in the repo, in tests, in fixtures, in logs, or in error messages. Configuration comes from the environment. If you need a value for a test, generate it in the test.
- New dependencies need a one-line justification in the PR description and must pass `cargo deny check`. Prefer crates that are widely used, actively maintained, and small. Pin exact versions in `Cargo.toml`.
- Never log tokens, passwords, session ids, or full request bodies. Log ids and outcomes.
- Constant-time comparison for any secret (`subtle` crate). Hash tokens before storing them.
- Any file path derived from untrusted input (request data, uploads, metadata, database text) goes through the storage layer's safe-join; never concatenate such paths yourself. Paths handed to you by another trusted component (a job payload built by the pipeline, a command line argument from the worker) are validated by canonicalizing and prefix-checking, as the brief describes.
- Error responses to the public never reveal internals: no stack traces, no SQL, no file paths, no dependency names.

Performance
- Nothing expensive in the request path. Image work, ML, zips, and anything over a few milliseconds goes to the job queue.
- Every PR that touches a hot path (request handling, derivative serving, search, pipeline) includes or updates a `criterion` benchmark and reports before/after numbers in the PR description.
- Allocations in hot paths are deliberate. Use borrowed types, `Bytes`, and streaming where the architecture doc says so.
- Do not add runtime templating, reflection, or dynamic dispatch where a compile-time construct exists.

Code
- `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` must pass before you open a PR. Run `scripts/check.sh`; it runs everything CI runs.
- No `unwrap()`, `expect()`, or `panic!()` outside tests and `main` startup. Use `Result` with typed errors (`thiserror`) in library crates and `anyhow` only in binaries.
- Public types and functions get doc comments that say what they guarantee, not what they do line by line.
- Tests are part of the task, not an extra. Unit tests next to the code, integration tests under `tests/`, fixtures from `fixtures/` (never from the internet).
- Match the surrounding style. Do not reformat or rename code outside your brief.
- Do not edit `docs/` to match your implementation. If the docs are wrong or incomplete, say so in the PR description and implement what the docs say, or stop and report if that is impossible.

Scope
- Touch only what the brief lists, plus the supporting files that any task may always change when its brief requires them: `Cargo.lock`, the workspace `Cargo.toml` (dependency pins), the manifests of the crates you work in, `crates/yhotos-bench/Cargo.toml` and `crates/yhotos-bench/benches/` (benchmark targets), `fuzz/` (the cargo-fuzz harness crate and its manifest, with `libfuzzer-sys` and `arbitrary` as its only dependencies), `deploy/dev/`, and `.github/workflows/ci.yml` when an acceptance item needs a CI service or job. List each such change in the PR under "What changed".
- `deny.toml` at the repository root is maintainer-only: never edit it. If a dependency needs a licence or advisory exception, say so under "Out of scope, needs a decision" and the maintainer adds it. The fuzz harness has its own `fuzz/deny.exceptions.toml`, which is in scope for fuzz work.
- A brief's Scope names areas, not an exhaustive file list. Everything needed to make the feature compile, be reachable and be tested is in scope by definition: module declarations, router and state wiring, `main`/CLI dispatch, middleware integration, migrations and `.sqlx/` metadata, templates and static assets the feature renders, integration tests, and scripts the brief names. Do not stop for these; do them and list them under "What changed".
- If you believe something else outside the brief must change, write that in the PR description under "Out of scope, needs a decision" and do not do it.
- `tasks/<id>.blocked.md` is for a brief that cannot be satisfied at all (a real contradiction, a missing prerequisite crate API). It is not for a missing manifest line, a lockfile update, or a benchmark target; do those. If one requirement of the brief is impossible, implement the rest, and describe the impossible part in the PR instead of stopping everything.
- If the brief is contradictory or impossible as written, stop, write what you found in `tasks/<id>.blocked.md`, and finish; the run script opens the PR so a human sees it.
- Do not install system packages, change CI secrets, or touch anything outside the repository worktree.

## Git

- Branch names: `y/<task-id>-<short-slug>`, for example `y/003-db-schema`.
- Commit messages: a short title, then plain point form of what changed. No em or en dashes. No marketing words. Example:

  ```
  Add storage safe-join and content hashing

  - BLAKE3 hashing of uploads with streaming reader
  - safe_join rejects traversal, absolute paths, and symlinks
  - unit tests for every rejection case
  ```

- One PR per task. The PR description follows `.github/pull_request_template.md`.
- Inside the Codex sandbox the `.git` directory is read only, so you cannot commit. Do not try. The run script commits your working tree and opens the PR when you finish; your final message becomes the PR description.
- Never force push. Never rewrite history on a shared branch. Never push to `main`.

## Definition of done

A task is done when all of these are true:
1. Everything in the brief's acceptance list passes, with tests that prove it.
2. `scripts/check.sh` passes locally.
3. Security notes in the brief are addressed and the PR description says how.
4. Benchmarks required by the brief exist and numbers are in the PR description.
5. The PR description lists every new dependency with a reason.
6. Nothing outside the brief's scope changed.
