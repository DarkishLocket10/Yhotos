# Piwigo 16 migration runbook

Rehearse **twice** against a snapshot on a disposable staging Yhotos stack before
cutover. Use Linux for the final run. Never point a rehearsal at the live Yhotos
database. The operator supplies all credentials through environment variables;
do not put them in command arguments, logs, reports, shell history or this repo.

## Prepare the snapshot and source user

1. Back up both databases and verify a restore. Record the Piwigo container name,
   database name, upload mount, snapshot timestamp and counts. The measured
   acceptance baseline is **4,925 source images and 33 categories**, including
   **12 admin-only photos**. Scope section 10's older 4,966 file figure differs
   from the measured section 2 baseline; investigate filesystem extras rather
   than importing files absent from the image table.
2. For every rehearsal and the final snapshot, stop Piwigo and **all other
   database and upload writers** (including scheduled jobs and external upload
   tools) before starting either snapshot. Keep every writer stopped until both
   the database dump and upload copy/snapshot finish. Piwigo 16 tables may use
   [MyISAM](https://raw.githubusercontent.com/Piwigo/Piwigo/16.4.0/install/piwigo_structure-mysql.sql);
   [`mariadb-dump --single-transaction`](https://mariadb.com/docs/server/clients-and-utilities/backup-restore-and-import-clients/mariadb-dump#--single-transaction)
   does not make those tables consistent.
   With writers stopped, dump the database and copy/snapshot the upload directory,
   then restore both into separate staging storage and MariaDB 11.4. Record the
   stopped writers and snapshot start/end times; verify neither source changed
   during that interval before restarting writers after a rehearsal. Keep them
   stopped through final cutover. Use a defaults file or `MYSQL_PWD` for the
   operator's dump/restore credentials. The tool never dumps/restores the source.
3. On the snapshot (and later the final source), run these exact statements as
   the MariaDB administrator, replacing `piwigo` with the source database name and
   the password placeholder with a newly generated password. This example assumes
   MariaDB receives the migration connection from `127.0.0.1`; use the actual
   single source host if a container bridge changes that address. Do not grant
   writes, FILE, administration privileges or access to other tables.

   ```sql
   CREATE USER 'yhotos_migrate'@'127.0.0.1' IDENTIFIED BY '<operator-generated-password>';
   GRANT SELECT ON piwigo.piwigo_categories TO 'yhotos_migrate'@'127.0.0.1';
   GRANT SELECT ON piwigo.piwigo_images TO 'yhotos_migrate'@'127.0.0.1';
   GRANT SELECT ON piwigo.piwigo_image_category TO 'yhotos_migrate'@'127.0.0.1';
   GRANT SELECT ON piwigo.piwigo_tags TO 'yhotos_migrate'@'127.0.0.1';
   GRANT SELECT ON piwigo.piwigo_image_tag TO 'yhotos_migrate'@'127.0.0.1';
   GRANT SELECT ON piwigo.piwigo_config TO 'yhotos_migrate'@'127.0.0.1';
   SHOW GRANTS FOR 'yhotos_migrate'@'127.0.0.1';
   ```

4. Mount the source upload directory read-only. On tower this is
   `/mnt/user/data/media/gallery/upload`; the tool's root is the **upload directory**,
   not its parent. Only the known `./upload/` or `upload/` prefix is removed from
   Piwigo's stored path before `SourceTree` validates it. Symlinks, alternate
   galleries paths, traversal, non-regular entries and missing files are reported
   and skipped. Piwigo derivatives are never read. Mount the Yhotos data root
   read-write at a separate, non-overlapping path; it must already exist.
5. Apply Yhotos migrations with the administrative deployment login. The importer
   never runs DDL automatically. Stop other administrative writers during import.
   The source transaction is repeatable-read and read-only even during real runs.

## Connect, dry-run and import

For a remote source, start a loopback-only SSH tunnel (no password on the command
line):

```sh
ssh -N -L 127.0.0.1:13306:127.0.0.1:3306 tower
```

Export `PIWIGO_DATABASE_URL` using the SELECT-only user, for example with host
`127.0.0.1:13306` and the source snapshot database name. Supply the percent-encoded
password from your secret environment. Remote direct connections require verified
MariaDB TLS; local/tunneled connections allow local transport. Export
`DATABASE_URL` for the staging/final Yhotos administrative login. PostgreSQL TLS
is verified by default. For a disposable localhost database or a loopback SSH
tunnel only, set `YHOTOS_MIGRATION_PG_TLS=disable`; remote plaintext is rejected.
The tool disables source statement logging and never prints connection errors'
underlying details.

Build with `SQLX_OFFLINE=true cargo build --release -p yhotos-migrate-piwigo`.
Use a **new report filename for every invocation**. Reports must be outside the
source tree, must not exist already, and must not be symlinks. The output file is
reserved before import so an unwritable report fails before destination changes.

```sh
./target/release/yhotos-migrate-piwigo \
  --source-root /mnt/piwigo-upload \
  --dry-run --concurrency 4 --report ./rehearsal-1-dry.json

./target/release/yhotos-migrate-piwigo \
  --source-root /mnt/piwigo-upload --storage-root /data \
  --concurrency 4 --report ./rehearsal-1-import.json
```

Dry-run reads and hashes source originals but never connects to PostgreSQL, opens
the destination store or writes any source entry. Its only write is the requested
report. It prints source album/image counts, membership rows, bytes, per-album
counts, slug/legacy collisions, skipped image ids with fixed reasons and metadata
warnings. Attach the real-snapshot dry-run JSON/stdout to the PR. Resolve every
skip and warning before cutover. An incomplete import reports skips in JSON and
can exit successfully; **success exit status is not verification**.

Piwigo dates have no timezone. UTC is the default interpretation. If the source
installation used a known fixed offset, pass `--source-offset-seconds` (for
example `-18000`) consistently on every rehearsal and final run. Capture wall
clock is retained separately. A fixed offset cannot model historical DST; verify
dates against the source and resolve timezone ambiguity before cutover.

Albums preserve numeric global-rank ordering and nesting. Names/entities are
decoded, HTML/comments become plain text, and sibling slug collisions receive
`-2`, `-3`. Retained destination albums/tags reserve their slugs when their source
identities disappear, so remaining collision members cannot take those URLs.
Unknown album statuses fail closed as private. Private albums and
`level > 0` photos are restricted with **no grants**. Inherited public photo
visibility respects private ancestors. Curated source capture dates/GPS take
priority over EXIF during ingest; missing values may come from EXIF. Author is
stored on `photos.author`. Invalid GPS is reported and omitted.

Each image's first category by global rank is the bare-photo redirect target.
Additional categories produce additional photo rows sharing the same original
hash, with independent category order and one ingest job per new row. Distinct
Piwigo image ids with identical bytes remain distinct. Counts of Yhotos photo
rows can therefore exceed 4,925. For verification, join `migration_sources` to
`photos` on `photo_id = photos.id` and filter `photos.deleted_at IS NULL` before
counting distinct `image_id` for the source count or rows for memberships. Retired
source identities are excluded from these counts. Uncategorised photos are
reported, not silently put in a synthetic album.

Reruns preserve photo URLs using `(image_id, category_id)` in `migration_sources`
and compare its original hash. Unchanged hashes skip original copies and do not
queue another ingest unless a source override was removed or an unfinished photo
is being restored. Removed source capture dates/GPS clear the previous published
metadata and derived place, then enqueue a new generation to re-extract EXIF;
absent overrides can use actual EXIF again. Changed hashes also replace the
original with a new generation and enqueue ingest. Album identity uses
`migration_albums`. Source metadata and
manual tag memberships are authoritative on rerun; do not edit imported content
in Yhotos until final migration completes. Auto tags remain independent. Reruns
retire every migration-owned photo copy absent from the successfully imported
memberships, including source-deleted images and skipped files. Retirement sets
restricted visibility, removes grants and trashes the obsolete copy in the same
transaction as current metadata updates. Returning memberships restore their
identities. Restored pending/processing/failed or unpublished photos enqueue a
fresh ingest generation; already published photos retain their derivatives.
Empty source-deleted albums remain; use a fresh rehearsal destination
for exact album counts.

Keep the destination private during reruns. Before serving revised visibility,
restart public serving processes to discard their file metadata caches, and purge
previously public HTML, derivative and original responses from the proxy/CDN.
Reopen public traffic only after the purge succeeds. This follows the serving
cache invalidation requirement for visibility changes.

Legacy rows include numeric album paths, numeric/url-name paths, name/slug aliases
and both photo URL forms. Piwigo's nullable category `permalink` supplies its
url-name when available. Ambiguous name aliases are reported and omitted rather
than redirected to an arbitrary album; numeric links remain. Each import
transaction replaces its owned redirect set, removing previously unique aliases
that have become ambiguous or disappeared. Independently managed redirects and
old import mappings manually retargeted to different targets are retained.
Bare album aliases (including encoded names) resolve through the same authorized
HTTP lookup as `/legacy/` and PHP paths; restricted targets return 404. Names
conflicting with application paths (for example `/map`) retain prefixed aliases
and are reported; unreachable bare/PHP variants are not stored. Old IDs were reset,
so test name-based fallbacks with representative historical links. Users,
comments, ratings and Piwigo derivatives are intentionally excluded.

## Drain, verify, repeat and cut over

1. Start the worker and watch the admin **Jobs** view until ingest and follow-up
   ML/geo jobs drain. Investigate failed/retrying jobs; wait for every imported
   photo to be ready. The full migration including ML must finish within six
   hours on tower. Keep reports and wall-clock measurements for both rehearsals.
2. Compare counts: 33 albums, 4,925 distinct source images, correct membership
   row count, 12 admin-only source images. All skipped items must be resolved.
3. Verify nesting, numeric order, descriptions, authors, dates, GPS and manual tags.
   Check **every album has the expected cover** after ingest. Confirm restricted
   photos/albums are invisible anonymously and have no grants.
4. Visually compare **20 randomly chosen photos** against Piwigo, including the
   PNG, a large JPEG, geotagged photos and admin-only content as admin. Check
   original hashes/byte lengths and derivative fidelity.
5. Test **10 old links**, including album names from before the ID reset, nested
   albums, numeric and url-name album forms, photo URLs with and without a
   category. Restricted targets must not redirect anonymously.
6. Run the importer again; identities and counts must be unchanged and existing
   originals reused. Repeat the entire rehearsal on a **fresh staging stack**.
7. For final cutover, stop the Piwigo container first, take a final consistent DB
   snapshot and read-only upload snapshot/mount, repeat dry-run, import, drain and
   the verification checklist. Keep Piwigo stopped to prevent drift. Switch DNS/
   proxy only after owner sign-off. Retain DB/upload backups for rollback; if
   verification fails, keep Yhotos private and restart the previous Piwigo stack.
8. Revoke/drop the temporary MariaDB user and close the SSH tunnel after sign-off.

## Automated integration fixture

`YHOTOS_TEST_MARIADB_URL` must point only at a disposable MariaDB 11.4 database
named `piwigo_fixture`. The integration test resets it from
`crates/yhotos-migrate-piwigo/tests/fixtures/piwigo.sql`. It contains synthetic
nested/private albums, entities, duplicate memberships and hashes, missing/unsafe
files and tags. PostgreSQL tests create isolated databases from `DATABASE_URL`.
CI exposes the MariaDB URL only to this crate's test step. For local full checks,
provide both test URLs, run `npm ci --prefix web`, then `scripts/check.sh`.
