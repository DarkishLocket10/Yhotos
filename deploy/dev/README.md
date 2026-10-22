# Local database checks

`scripts/check.sh` starts the dev PostgreSQL 16 with required pgvector when
`DATABASE_URL` is unset. Docker Compose must be available. `deploy/dev/start.sh`
generates a random password once in `deploy/dev/.env` (mode 0600, git-ignored),
starts the digest-pinned database, and exports its localhost URL when sourced:

```sh
source deploy/dev/start.sh
cargo sqlx migrate run --source crates/yhotos-db/migrations
cargo sqlx prepare --workspace -- --all-targets --all-features
cargo bench -p yhotos-bench --bench jobs
```

The dev service deliberately runs with `ssl=off`; its exported URL explicitly
sets `sslmode=disable`. Tests and the jobs benchmark pass `DatabaseTls::Disable`
to `Db::connect` for this loopback connection. `DatabaseTls::default()` is
`Require`, which overrides URL `sslmode` with `verify-full`: both the server
certificate and hostname must verify. Set `sslrootcert=/path/to/ca.pem` in the
connection URL for a private certificate authority. A `Require` connection to
the plain dev service fails as expected, and the integration test asserts it.
`Prefer` is an explicit development fallback that can allow plaintext;
production TCP connections must use `Require`. `Disable` is rejected before
dialing unless the effective host is a Unix socket path or a literal loopback
IP address (IPv4 or IPv6); DNS names, including `localhost`, are rejected.

Install the pinned migration/prepare CLI once if needed:

```sh
cargo install sqlx-cli --version 0.9.0 --locked --no-default-features --features postgres,rustls
```

The test login is a disposable local superuser, needed to create isolated test
databases, extensions and NOLOGIN process roles. Never use it for deployments.
Deployment login roles must inherit only their public, admin or worker role;
use a separate privileged administrative migration login for extension/role DDL.
Public and worker `Db` handles explicitly assume their process role. Password
and login-time functions require caller-verified user ids; table roles do not
establish row ownership. SQLx query caches under `.sqlx/` include test and
benchmark queries. CI builds with `SQLX_OFFLINE=true` and runs migrations before
testing against its pgvector service.

The jobs benchmark reports 1,000 individual claim samples (including p50) and a
Criterion `jobs/claim` estimate. Enqueue and completion are outside claim timing.
It uses a unique job kind and leaves completed jobs for normal queue inspection.
Workers retain the `ClaimToken` returned alongside the claimed job and pass it
to completion/failure calls. Updates match the job, worker and attempt; expired
claims return `false` without altering a replacement worker's claim.

Stop the database with `docker compose -f deploy/dev/compose.yml down`. The named
volume preserves it; remove the volume before regenerating an existing password.

## CPU worker tuning

`YHOTOS_WORKER__CONCURRENCY` limits simultaneous codec hosts (default half the
available CPUs, minimum one). `YHOTOS_WORKER__VIPS_CONCURRENCY` controls threads
per host (default 2, range 1..=64). Start with eight hosts and two threads on
tower's sixteen cores; lower the host count when memory or disk bandwidth is the
limit. `YHOTOS_WORKER__CODEC_HOST_PATH` must be an absolute trusted executable
path. `YHOTOS_WORKER__CODEC_TIMEOUT` is a wall-clock limit in seconds (default
300, maximum 600). `YHOTOS_WORKER__VARIANT_PLAN` optionally supplies a structured
`{ variants: [...] }` plan with `name`, `long_edge`, `square`, `formats`
(`avif`, `jxl`, `jpeg`) and `scale2x`; names, uniqueness and the 32-output cap
are checked before consuming jobs. The worker uses `Role::Worker` and verified
database TLS, with an explicit loopback-only development opt-in
`YHOTOS_WORKER__LOCAL_PLAINTEXT_DATABASE=true`. SIGTERM/SIGINT drains for up to
29 seconds before cancelling and releasing remaining claims. Unimplemented ML, geo and video handlers defer for one hour
without exhausting their attempt allowance.

Queue facade metrics are `yhotos_jobs_claimed_total`,
`yhotos_jobs_completed_total`, `yhotos_jobs_failed_total` (all labelled `kind`),
`yhotos_jobs_duration_seconds` (histogram) and `yhotos_jobs_queue_depth` (gauge,
refreshed every ten seconds). The worker installs a Prometheus recorder and
serves `/metrics` at `YHOTOS_WORKER__METRICS_LISTEN` (default
`127.0.0.1:9101`; only loopback addresses are accepted). Duration observations
export as histogram buckets, sum and count.

`cargo test -p yhotos-codec-host --all-features --test pipeline` runs the actual
host against the dev database. The host-package harness gives Cargo access to
`CARGO_BIN_EXE_yhotos-codec-host`; the test bodies live in the pipeline crate.
CI runs these tests inside `Dockerfile.codec` with its pgvector service.
`test-hooks` enables an explicit bounded child-delay environment variable and
an in-process parent verification barrier; production builds omit both hooks.

Build the host in release mode, then run
`cargo bench -p yhotos-bench --bench pipeline`. It reports single-photo and
eight-photo parallel full ingests, resetting publication outside the measured
region. It skips explicitly unless a fixture of at least 24 MP exists in
`fixtures/large/`. The architecture's default sizes, 2x variants and encoder
rules expand to 23 files; the performance document's older 13-file count does
not match that plan.
