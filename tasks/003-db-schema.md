---
id: 003
slug: db-schema
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [001]
crates: [yhotos-db, yhotos-core]
---

# 003 Database schema, migrations, and repositories

## Goal

`yhotos-db` owns the PostgreSQL schema from `docs/architecture.md` section 4 as `sqlx` migrations, provides a `Db` handle with connection pooling and three roles' worth of connection strings (public, admin, worker), and exposes typed repository functions for albums, photos, users, groups, grants, share links, tags, likes, flags, settings, legacy redirects, and the jobs table primitives. Every query is a compile-time checked `sqlx` macro. Integration tests run against a real Postgres.

## Read first

- `AGENTS.md`
- `docs/architecture.md` sections 3, 4, 7
- `docs/threat-model.md` sections 3 (B3, B4), 4.3, 4.5 (database compromise)
- `docs/performance.md` section 3 (one round trip per page; indexes)

## Scope

`crates/yhotos-db/` entirely, `crates/yhotos-core/` only to add `sqlx` feature support on ids if not already present from task 002 (coordinate: if 002 is merged, reuse; if not, add the `sqlx` feature gate yourself and keep it minimal). `deploy/dev/compose.yml` for a local Postgres with pgvector. `scripts/check.sh` to start that Postgres if `DATABASE_URL` is unset (document it).

## Design

### Migrations
`crates/yhotos-db/migrations/` with `sqlx migrate` conventions, timestamped, forward-only. First migration enables `pgcrypto` (not needed for uuid v7, which the app generates), `citext`, `vector` (pgvector; skip gracefully in tests if unavailable by having a separate `0002_vectors.sql` that is applied only when the extension exists? No: require pgvector; the dev compose uses `pgvector/pgvector:pg16`). Tables exactly as in architecture section 4, with:
- `uuid` primary keys, `timestamptz` everywhere, `text` not `varchar`, `citext` for usernames and slugs.
- Check constraints: exactly one of `user_id`/`share_link_id` on `likes` and `flags`; `visibility` enums as Postgres enums; `album.slug` unique per `parent_id` (use a unique index on `(coalesce(parent_id, '00000000-0000-0000-0000-000000000000'::uuid), slug)`); `photos.short_id` unique; `share_links.token_hash` unique.
- Every index listed in architecture section 4, plus a partial index on `jobs` for the claim query.
- Triggers: `updated_at` maintenance via a single function.
- Roles: a migration creates roles `yhotos_public`, `yhotos_admin`, `yhotos_worker` (NOLOGIN; the deployment creates login roles that inherit) and grants: public gets `SELECT` on everything except `user_mfa`, `sessions` (`SELECT` only via a security-definer function? keep it simple: public gets `SELECT` on `sessions` and `users`, nothing on `user_mfa`), `INSERT` on `events`, `likes`, `flags`, `share_link_opens`, `sessions`, `UPDATE` on `sessions.last_seen_at`, `users.password_hash`/`must_change_password` (for the forced change flow); worker gets everything except `users`, `user_mfa`, `settings`; admin gets all. Put the grant matrix in a comment at the top of that migration and make the integration test assert it by attempting forbidden statements as each role and expecting permission errors.

### Db handle
```rust
pub struct Db { pool: PgPool }
impl Db {
  pub async fn connect(url: &SecretString, role: Role, max_conns: u32) -> Result<Db, DbError>;
  pub async fn migrate(&self) -> Result<(), DbError>;  // admin role only
  pub fn pool(&self) -> &PgPool;
}
pub enum Role { Public, Admin, Worker }
```
`connect` sets `application_name`, `statement_timeout` (5 s public, 60 s worker, 30 s admin), and `idle_in_transaction_session_timeout`.

### Repositories
Modules `albums`, `photos`, `users`, `groups`, `grants`, `share_links`, `tags`, `likes`, `flags`, `settings`, `redirects`, `jobs`. Functions take `&mut PgConnection` or `&PgPool` (not `&Db`), return domain structs defined in this crate (`AlbumRow`, `PhotoRow`...) that implement `From` into `yhotos-core` types where those exist. Minimum set:
- albums: `create`, `get_by_id`, `get_by_path(&[Slug])`, `children(parent)`, `ancestors(id)` (recursive CTE), `update`, `reorder(parent, &[AlbumId])`, `set_cover`, `soft_delete`, `restore`, `list_tree()` (all albums, one query).
- photos: `create`, `get_by_short_id`, `get_by_hash`, `list_in_album(album, order, page, per_page)` returning photos joined with their derivative hashes as a `jsonb` array (one round trip), `update_metadata`, `move_to_album(&[PhotoId], AlbumId)`, `set_visibility(&[PhotoId], Visibility)`, `soft_delete`, `restore`, `purge_deleted_before(ts)`, `replace_original(id, new_hash, ...)`.
- users: `create(username, password_hash, must_change)`, `get_by_username`, `get_by_id`, `set_password`, `disable`, `enable`, `touch_login`.
- groups: `create`, `rename`, `add_member`, `remove_member`, `members`, `groups_of(user)`.
- grants: `grant_album`, `revoke_album`, `grant_photo`, `revoke_photo`, `grants_for_album`, `grants_for_photo`.
- share_links: `create`, `get_by_token_hash` (only non-revoked, returns expiry for the caller to check), `update_flags`, `set_expiry`, `revoke`, `record_open`, `stats(id)`.
- tags: `upsert_by_slug`, `attach(photo, tag, source, confidence)`, `detach`, `tags_of(photo)`, `photos_with_tag(tag, page)`.
- likes/flags: `like`, `unlike`, `liked_by_user(user, page)`, `likes_of_photo`, `flag`, `open_flags(page)`, `resolve_flag`.
- settings: `get(key) -> Option<serde_json::Value>`, `set(key, value)`.
- redirects: `insert`, `lookup(path) -> Option<target>`.
- jobs: `enqueue(kind, payload, priority, run_after)` with `NOTIFY jobs`, `claim(worker_id, &[kind]) -> Option<JobRow>` (the SKIP LOCKED query from architecture section 7), `complete(id)`, `fail(id, error, retry_in)`, `release_stale(older_than)`, `list(status, page)`.

### Tests
`tests/` using `sqlx::test` or a manual pool from `DATABASE_URL`, each test in its own transaction or schema. Cover: migrations apply from empty; every repository function; the role grant matrix; the album slug uniqueness per parent; the likes/flags check constraint; jobs claim concurrency (two claimers, 10 jobs, no double claim; use `tokio::join!`); `list_in_album` is one query (assert with `pg_stat_statements` is overkill; instead wrap the pool in a counting `sqlx` log hook or count via `tracing` events and assert 1).

## Acceptance

- [ ] `sqlx migrate run` from empty succeeds; re-running is a no-op.
- [ ] `cargo sqlx prepare --workspace` output (`.sqlx/`) is committed so CI builds offline; CI's `test` job also runs migrations against the service Postgres and runs the integration tests.
- [ ] Role grant matrix test passes (forbidden statements fail with a permission error for each role).
- [ ] Jobs concurrency test: no job claimed twice.
- [ ] Every repository function has at least one test; the PR lists them.
- [ ] `list_in_album` test asserts a single query.
- [ ] `deploy/dev/compose.yml` brings up `pgvector/pgvector:pg16` with a generated password written to `.env` (git-ignored), and `scripts/check.sh` uses it when `DATABASE_URL` is unset.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model B3/B4/4.5: least-privilege roles are part of this task, not later. Statement timeouts set per role.
- Threat model 4.3: nothing in this crate decides visibility; it only fetches inputs for `yhotos-core::access`. Do not add "visible to" filters in SQL except the explicit `visibility` column pass-through. (Visibility filtering in SQL for pagination correctness is a later task and will be designed with the access function.)
- All queries are `sqlx::query!`/`query_as!` macros. A reviewer will grep for `format!` near `query`.
- No secrets in tests: the compose file generates the password.

## Performance

- Indexes from architecture section 4 exist in the first migration; `EXPLAIN` in a test for `list_in_album` and `claim` shows index use (assert the plan contains `Index` and not `Seq Scan` on `photos`/`jobs`).
- `claim` benchmark in `yhotos-bench/benches/jobs.rs` against the dev Postgres: under 1 ms p50.

## Not in this task

- Sessions and MFA tables are created here but their logic lives in task 004.
- Vector and face tables are created here; search logic is later.
- Events rollup logic (table only).
