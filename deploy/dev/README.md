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

Stop the database with `docker compose -f deploy/dev/compose.yml down`. The named
volume preserves it; remove the volume before regenerating an existing password.
