---
id: 007
slug: jobs-worker-pipeline
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [003, 004, 006]
crates: [yhotos-jobs, yhotos-pipeline, yhotos-worker]
---

# 007 Job queue client, worker loop, and the ingest pipeline

## Goal

`yhotos-jobs` turns the `jobs` table primitives from task 003 into a typed queue: enqueue typed job payloads, a worker loop that claims with `SKIP LOCKED`, listens on `NOTIFY jobs`, retries with backoff, releases stale locks, and reports metrics. `yhotos-pipeline` implements the `ingest` job: run `yhotos-codec-host` on an original with the variant plan, verify outputs, place them in the store, write `derivatives` rows and photo metadata in one transaction, and enqueue the ML and geo follow-up jobs. `yhotos-worker` is the binary that runs it. After this task, inserting a photo row and enqueueing `ingest` produces every derivative on disk and in the database.

## Read first

- `AGENTS.md`
- `docs/architecture.md` sections 6, 7
- `docs/threat-model.md` B5, 4.1
- `docs/performance.md` section 1 (pipeline budgets)
- Briefs 003, 004, 006 (the APIs you consume)

## Scope

`crates/yhotos-jobs/`, `crates/yhotos-pipeline/`, `crates/yhotos-worker/`, `crates/yhotos-bench/benches/pipeline.rs`. `yhotos-db::jobs` may gain small additions if a primitive is missing; list them in the PR.

## Design

### yhotos-jobs
```rust
#[derive(Serialize, Deserialize)] #[serde(tag = "kind", content = "payload")]
pub enum Job { Ingest { photo_id }, MlEmbed { photo_id }, MlFaces { photo_id }, MlOcr { photo_id }, MlTags { photo_id }, GeoReverse { photo_id }, VideoTranscode { photo_id }, Zip { request_id }, Gc, Rollup { day }, LegacyRedirectRebuild }
pub struct Queue { pool: PgPool }
impl Queue {
  pub async fn enqueue(&self, job: Job, priority: i16, run_after: Option<Timestamp>) -> Result<JobId, JobsError>;   // dedupes: an identical (kind, payload) that is not finished is not inserted again; returns the existing id
  pub async fn run_worker(&self, worker_id: &str, kinds: &[JobKind], handler: impl JobHandler, shutdown: CancellationToken) -> Result<(), JobsError>;
}
#[async_trait] pub trait JobHandler { async fn handle(&self, job: &JobRow, ctx: &JobCtx) -> Result<(), JobError>; }
pub enum JobError { Retry { after: Duration, reason: String }, Fatal(String) }
```
Loop: `LISTEN jobs`; on notify or every `poll_interval` (3 s), claim up to `concurrency` jobs (config, default `nproc/2` for ingest, 1 for gc/rollup) into a `JoinSet`; each job has a per-kind timeout (ingest 10 min, ml 5 min, zip 30 min, gc 1 h); on timeout, the job is failed with `Retry`. Backoff: `30 s * 2^attempts` capped at 1 h, `max_attempts` per kind (ingest 3). Stale lock sweeper every minute releases locks older than the kind's timeout plus 1 min. Graceful shutdown: stop claiming, wait for running jobs up to 30 s, release what is still running back to the queue (clear `locked_by`), exit.

Metrics via `metrics` crate facade: jobs claimed/completed/failed by kind, duration histogram, queue depth gauge (polled every 10 s).

### yhotos-pipeline
```rust
pub struct Ingest { store: Store, db: PgPool, codec_host: PathBuf, plan: VariantPlan, limits: Limits, concurrency_per_host: u8 }
pub struct VariantPlan { pub variants: Vec<Variant> }   // from config; defaults per architecture section 6
pub struct Variant { pub name: &'static str, pub long_edge: u32, pub square: bool, pub formats: Vec<Format>, pub scale2x: bool }
```
`ingest(photo_id)`:
1. Load the photo row; if `kind == video`, enqueue `VideoTranscode` and finish. If derivatives already exist for this hash and plan version, finish (idempotent).
2. Create a temp dir via `store.tmp_dir()`. Build the host `Job` JSON with outputs for every variant/format (and 2x where `scale2x`), `want` everything.
3. Spawn `yhotos-codec-host` with `tokio::process::Command`: `stdin` = job JSON, `stdout` captured (cap 1 MiB), `stderr` captured (cap 64 KiB), env cleared except `PATH`, `VIPS_CONCURRENCY`, and a fixed allow list; `kill_on_drop`; wall-clock timeout from config (default 5 min) then SIGKILL.
4. Parse the report. Verify each declared output exists, its size matches, and its BLAKE3 matches by re-hashing while moving it into the store (`put_derivative` takes the hash from the key; compute it in the pipeline, do not trust the host's hash alone).
5. In one transaction: upsert `derivatives` rows, update photo `width/height/orientation/is_hdr/has_gain_map/icc_name/camera_*/lens/focal_mm/aperture/shutter/iso/gps_lat/gps_lon/taken_at/dominant_colors/placeholder`, set `kind` (`raw` if identify says so), set `status = ready`.
6. Enqueue `MlEmbed`, `MlFaces`, `MlOcr`, `MlTags`, and `GeoReverse` if gps present, all `priority` lower than ingest.
7. Exit codes: 2 (rejected input) is `Fatal` and marks the photo `failed` with the stderr line as the reason (length-capped, no paths); 3 is `Retry` once then `Fatal`; 4 (timeout) is `Retry`.

Add a `photos.status` enum (`pending|processing|ready|failed`) and `photos.failure_reason` in a migration in this task (coordinate with task 003's migration numbering by using a later timestamp).

### yhotos-worker
Binary. Loads config (reuse `yhotos-config`; add `worker: { concurrency, codec_host_path, codec_timeout, vips_concurrency }`), connects as `Role::Worker`, installs the `Ingest` handler and stub handlers for the ML and geo kinds that immediately `Retry { after: 1 h }` with reason "not implemented" (so queued jobs are not lost before those tasks land), runs `run_worker` until SIGTERM.

### Tests
- `yhotos-jobs`: dedupe on enqueue; claim respects `run_after` and priority; retry backoff schedule; stale lock release; shutdown releases running jobs; two workers never double-claim (reuse the pattern from task 003 at the queue level); `NOTIFY` wakes a waiting worker within 100 ms (test with a timeout).
- `yhotos-pipeline`: end-to-end on `fixtures/images/chart-srgb.png` and the orientation fixtures with a real `yhotos-codec-host` binary (build it in the test via `CARGO_BIN_EXE_yhotos-codec-host`), against the dev Postgres: all expected `derivatives` rows exist, files exist in the store, photo metadata is filled, follow-up jobs are enqueued, and re-running is a no-op. A malicious fixture yields `status = failed` with a reason and no derivative rows. A host that exceeds the timeout (simulate with an env var the host honors only under `cfg(test)`-like feature `test-hooks`) yields a retry.
- Verify that the host's reported hash is not trusted: a test replaces an output file after the host exits (using the `test-hooks` feature to pause) and asserts ingest fails.

### Bench
`pipeline.rs`: full ingest of one 24 MP fixture from `fixtures/large/` (skip if absent). Budget 4 s per photo with 8 in parallel on tower; record the local single-photo number.

## Acceptance

- [ ] All tests above pass; the end-to-end pipeline test runs in CI inside `deploy/dev/Dockerfile.codec` with the Postgres service.
- [ ] Idempotency: running `ingest` twice on the same photo produces no duplicate rows or files.
- [ ] A photo with `kind = video` is routed to `VideoTranscode` without running the codec host.
- [ ] Worker shuts down cleanly on SIGTERM within 30 s with a running job released.
- [ ] Metrics exported (names listed in the PR).
- [ ] Benchmark and number in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model B5: the host's report is untrusted. Re-hash outputs; cap stdout/stderr; clear the environment; kill on timeout; never pass user-controlled strings into the command line (only paths the pipeline constructed and JSON on stdin).
- Failure reasons stored in the database must not contain filesystem paths or stderr beyond the first line, capped at 200 chars.
- Temp directories are removed on every exit path (use `TempDir` drop semantics; test that a failure leaves `tmp/` empty).

## Performance

- Parallelism is per-host-process, `VIPS_CONCURRENCY=2`, `concurrency = nproc/2` hosts at once by default; document how to tune.
- The ingest transaction is one round trip for all derivative rows (batch insert with `UNNEST`), not one per row.

## Not in this task

- ML and geo handlers (stubs only).
- Video transcoding (handler stub only).
- Upload endpoint; this task assumes the photo row and original already exist (tests insert them directly).
