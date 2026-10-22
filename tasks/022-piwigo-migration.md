---
id: 022
slug: piwigo-migration
model: gpt-6.1-sol
effort: high
review_effort: xhigh
depends_on: [007, 011]
crates: [yhotos-migrate-piwigo, yhotos-db]
---

# 022 Piwigo migration tool

## Goal

`yhotos-migrate-piwigo` moves the existing Piwigo gallery into Yhotos: album tree (nesting, order, covers, descriptions), every photo with its Piwigo metadata, admin-only photos as restricted, tags, and a legacy redirect table so old album and photo links keep working. It reads Piwigo read-only, is idempotent, has a dry run, and is rehearsed against a snapshot before the real cutover.

## Read first

- `AGENTS.md`
- `docs/scope.md` 2 (measured Piwigo facts), 10 (migration plan)
- `docs/architecture.md` 13
- `docs/threat-model.md` 4.5 (database roles)

## Scope

Areas: `crates/yhotos-migrate-piwigo/`, `crates/yhotos-db/` additions (bulk upserts keyed on original hash, `legacy_redirects` population), `docs/migration.md` (runbook), plus supporting files.

## Design

- Source: Piwigo MariaDB (schema version 16, tables `piwigo_categories`, `piwigo_images`, `piwigo_image_category`, `piwigo_tags`, `piwigo_image_tag`, `piwigo_config`) via a read-only MariaDB user the operator creates (runbook gives the exact `CREATE USER ... GRANT SELECT` statement), and the upload directory (`/mnt/user/data/media/gallery/upload` on tower) mounted read-only. Use `sqlx` with the `mysql` feature only in this binary.
- Albums: preserve tree (`id_uppercat`), order (`global_rank`), names (Piwigo stores HTML entities; decode), descriptions (`comment`, sanitize to plain text or the Markdown subset), covers (`representative_picture_id`), status (`public` vs `private` mapped to visibility). Slugs from names; collisions within a parent get `-2`, `-3`.
- Photos: for each `piwigo_images` row, hash the file (BLAKE3) and `put_original` through the store; insert the photo in its album (first category by rank if in several; additional categories create additional photo rows sharing the hash, documented), with title (`name`, falling back to filename stem), description (`comment`), author, `date_creation` as `taken_at` when present, `date_available` as `posted_at`, GPS, and `level` > 0 mapped to restricted with no grants (admin only). Enqueue `Ingest` for each new photo (the pipeline generates derivatives; Piwigo's are ignored).
- Tags: copy `piwigo_tags` and links as manual tags.
- Redirects: one row per album for `index.php?/category/{id}`, `index.php?/category/{id}-{url_name}`, `{url_name}`, and per photo `picture.php?/{image_id}/category/{cat_id}` and `picture.php?/{image_id}`; plus album name-based rows since old IDs were reset at some point (scope section 2): map by the album's current name and slug as well.
- Idempotency: keyed on original hash plus Piwigo image id stored in a `migration_sources` table; re-running updates metadata, never duplicates; `--dry-run` prints counts, collisions, missing files, and unsupported fields without writing.
- Progress: prints per-album progress, a final report (albums, photos, bytes, skipped with reasons), writes the report to a JSON file.
- Concurrency: hashes and copies files with bounded parallelism (default 4) to avoid saturating the array; resumable (skips files already present by hash).

### Runbook (`docs/migration.md`)
Snapshot Piwigo DB and stop the Piwigo container during the final run; create the read-only DB user; dry run; real run; wait for ingest to drain (job view); verification checklist (counts match: 4,925 photos, 33 albums; every album has its cover; 20 random photos visually compared against Piwigo; 10 old links tested); rehearse twice on a staging stack before cutover.

### Tests
Integration tests against a MariaDB container seeded with a fixture dump that mirrors the real schema and shapes (nested album, private level, HTML entities in names, duplicate category membership, missing file, tags), asserting the resulting Yhotos state and redirects; idempotency (run twice); dry run writes nothing.

## Acceptance

- [ ] All tests pass in CI (MariaDB service added to the `test` job for this crate only).
- [ ] Dry run against a copy of the real Piwigo database on tower (run by the maintainer; the tool must support connecting through an SSH tunnel) reports 4,925 photos, 33 albums, and lists any problems; output attached to the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- Read-only source access; the tool never writes to MariaDB or the Piwigo upload directory (enforced by the DB user and a read-only mount; also the code opens files read-only).
- Credentials only from environment; never logged.
- Allowed new dependencies: `sqlx` `mysql` feature (this crate only), `html-escape`.

## Not in this task

- User accounts (re-created by the owner), comments and ratings (not used).
