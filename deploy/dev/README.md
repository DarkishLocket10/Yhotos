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

`start.sh` derives and exports `COMPOSE_PROJECT_NAME` from a short SHA-256 hash
of the canonical worktree path. Each worktree has an independent container and
volume. Set `POSTGRES_PORT` to a free localhost port when running worktrees at
the same time. Source the script before running Compose commands so they use
that same project name:

```sh
source deploy/dev/start.sh
docker compose -f deploy/dev/compose.yml down
```

Stopping preserves the volume. `start.sh` verifies a TCP login after the health
check and exits on any error. A credential mismatch preserves all data and prints
the exact project-specific `down --volumes` command for an explicit reset. Run
that command only when you intend to discard this worktree's development data.
`bash deploy/dev/test-start.sh` verifies isolation and failure behavior using a
fake Docker CLI.

Server configuration uses separate restricted deployment logins:
`YHOTOS_DATABASE_URL_PUBLIC` for the public listener, and
`YHOTOS_DATABASE_URL_ADMIN` for the admin listener and `create-admin` CLI. A public
build needs only the public URL. An admin build serves both listeners and needs
both URLs; never grant its public login membership in the admin role.
`YHOTOS_ADMIN_BASE_URL` is required with the admin feature and selects the admin
CSRF/WebAuthn origin independently of `YHOTOS_PUBLIC_BASE_URL`. Both URLs must use
HTTPS without credentials, queries or fragments.
