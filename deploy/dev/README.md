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

`start.sh` verifies a TCP login after the health check. If a container/volume
from an earlier worktree has a different password, it recreates this Compose
project's container **and volume**, discarding its disposable development data.
Other connection errors fail without deleting data. Never use this bootstrap
with production data.
