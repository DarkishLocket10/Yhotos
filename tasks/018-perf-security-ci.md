---
id: 018
slug: perf-security-ci
model: gpt-6.1-sol
effort: high
review_effort: high
depends_on: [011]
crates: [yhotos-bench]
---

# 018 Benchmark gates, Lighthouse CI, load tests, fuzzing CI, security smoke

## Goal

The measurement promises in `docs/performance.md` section 2 and `docs/threat-model.md` section 6 are enforced automatically: criterion benchmarks compared against `main` on every PR with a regression gate, Lighthouse CI against a running stack, nightly load tests, continuous fuzzing, and a security smoke script that asserts headers and route exposure against a live server.

## Read first

- `AGENTS.md`
- `docs/performance.md` all
- `docs/threat-model.md` 6
- `docs/process.md` (CI, self-hosted runner)

## Scope

Areas: `.github/workflows/{bench,lighthouse,fuzz,nightly}.yml`, `scripts/bench.sh`, `scripts/security-smoke.sh`, `scripts/fuzz-smoke.sh`, `deploy/dev/compose.bench.yml`, `crates/yhotos-bench/` (shared fixture loader, baseline comparison tool), `lighthouserc.json`, plus supporting files. This task edits `.github/`, so it must run on a machine whose GitHub token has the `workflow` scope (the Mac).

## Design

### Benchmark gate (`bench.yml`, on PR)
- Run all criterion benches on the PR head and on the merge base in the same job (same runner, interleaved to reduce noise), using `--save-baseline`. A small Rust tool in `yhotos-bench` (`compare` binary) reads both criterion outputs and fails if any benchmark's median regressed by more than 5 % and the confidence interval excludes zero; it prints a Markdown table posted as a sticky PR comment (via `gh` with `pull-requests: write` scoped to this job only).
- Mutation benchmarks must time real state changes: `crates/yhotos-bench/benches/interactions.rs` (task 013) currently repeats one photo and actor, so after warm-up every like conflicts, every unlike deletes nothing and every flag is skipped. Reset state outside each timed iteration (or rotate fresh photos/actors) and bench real writes separately from idempotent retries, so the like/flag p99 < 10 ms budget in `docs/performance.md` is actually measured.
- Benchmarks needing Postgres use the CI service container; those needing large fixtures skip on hosted runners and run nightly on the self-hosted runner.

### Lighthouse CI (`lighthouse.yml`, on PR touching `crates/yhotos-http/`, `web/`, templates)
- Builds the server, starts `deploy/dev/compose.bench.yml` (Postgres + server + a fixture loader that ingests 60 distinct small photos generated deterministically), runs `@lhci/cli` against home, an album, a photo page and search, mobile and desktop presets, three runs each, asserting the budgets in `docs/performance.md` (performance >= 95, LCP, CLS 0, total JS and CSS bytes). Reports uploaded as artifacts, not committed.

### Load tests (`nightly.yml`)
- `scripts/bench.sh` drives `oha` against album, photo, derivative, search, mixed; records p50/p99 and RSS; compares to the stored previous nightly result (artifact) and opens an issue on regression beyond budget.

### Fuzzing
- `fuzz.yml`: on PRs, 60 s per target for targets whose crate changed; nightly, 30 min per target on the self-hosted runner, corpus cached and minimized weekly; crashes upload artifacts and fail the job.

### Security smoke (`scripts/security-smoke.sh`, run in `lighthouse.yml` against the same stack and nightly)
Asserts with `curl` and `jq`: every public response has the exact security header set; CSP contains no `unsafe-inline` or `unsafe-eval`; `/admin/*` returns 404 on the public listener; `Server` header absent; restricted and missing photos return byte-identical 404s; rate limits return 429 with `Retry-After`; `/healthz` body is exactly `ok`; no cookies on anonymous GETs; HSTS present with preload.

### Self-hosted runner
Document (in `docs/process.md`, a short section the maintainer applies) how to register an ephemeral runner on tower in a container with no access to production data, labelled `self-hosted,tower`, used only by nightly jobs. Do not register it in this task.

## Acceptance

- [ ] On a PR that deliberately slows a benchmark by 20 % (a throwaway commit in this PR, reverted before merge), the bench gate fails with a clear table; after revert it passes.
- [ ] Lighthouse workflow passes on this PR with the budgets enforced.
- [ ] `scripts/security-smoke.sh` passes against the dev stack and fails when one header is removed (demonstrate in the PR).
- [ ] All workflow actions pinned by SHA, least `permissions`.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.6: workflows pinned, minimal permissions, the PR comment job is the only one with `pull-requests: write`, no secrets exposed to fork PRs (`pull_request` not `pull_request_target`).

## Not in this task

- Registering the self-hosted runner; external penetration test.
