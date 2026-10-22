---
id: 023
slug: deploy
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [007, 009, 011, 014, 018]
crates: [deploy]
---

# 023 Deployment: images, compose, Tailscale and Cloudflare Tunnel, backups

## Goal

A reproducible production deployment on tower: signed container images built in CI, a compose stack with the public server, admin server, worker and ML service, least-privilege database roles, the admin listener bound to Tailscale only, the public listener reachable only through Cloudflare Tunnel, backups with a tested restore, and a staging stack for rehearsals and the penetration test. Nothing in this task flips DNS; that is the cutover.

## Read first

- `AGENTS.md`
- `docs/architecture.md` 1, 14
- `docs/threat-model.md` 3, 4.5, 4.6
- `docs/scope.md` 9 (infrastructure)

## Scope

Areas: `deploy/` (Dockerfiles, `compose.yml`, `compose.staging.yml`, `.env.example`, `cloudflared/`, `postgres/roles.sql`, `backup/`), `.github/workflows/release.yml`, `docs/operations.md` (runbook), plus supporting files. Edits `.github/`; both the Mac and yesktop GitHub tokens have the `workflow` scope.

## Design

### Images
Multi-stage Dockerfiles: build with the pinned Rust toolchain, with an explicit feature list per image (never `--all-features`: the `test-util` features select deliberately weak test-only parameters, such as minimum-cost Argon2); add a release-workflow check that `cargo tree -e features` for each shipped binary does not contain `test-util`; runtime on a minimal distroless or `debian:stable-slim` base with only libvips/libheif/libjxl/libaom/libultrahdr/ffmpeg where needed (worker image only; the public server image contains no codec libraries at all). Non-root user, read-only root filesystem, no shell in the public server image. `release.yml` on tags: build, SBOM (syft), sign with cosign keyless (OIDC), push to GHCR; images referenced by digest in compose.

### Compose (tower)
- `server-public`: public feature set, `/data` read-only, connects as the public DB login role, listens on an internal network only; `cloudflared` sidecar points the tunnel at it.
- `server-admin`: `admin` feature, `/data` read-write, admin DB role, published only on the Tailscale IP of tower (`100.87.41.52:<port>`), never `0.0.0.0`.
- `worker`: `/data` read-write, worker DB role, CPU and memory limits, the codec host and ffmpeg inside, Landlock and seccomp active.
- `ml`: GPU reservation for one card, `/data` read-only, internal network only (`internal: true`), worker DB role.
- Postgres: reuse tower's `postgresql16` with a new database `yhotos` and login roles created by `deploy/postgres/roles.sql` (password from env, TLS required where not on the local socket); pgvector extension installed (tower's image lacks it: document switching that container to `pgvector/pgvector:pg16` or running a dedicated Yhotos Postgres; recommend dedicated, to keep Immich and others isolated).
- Healthchecks, restart policies, log rotation, `security_opt: no-new-privileges`, `cap_drop: [ALL]`.
- Data paths on the array: `/mnt/user/data/yhotos/{originals,derivatives,video,tmp}`; Postgres data on the cache drive.

### Network edge
- Cloudflare Tunnel config (`cloudflared/config.yml.example`) mapping `photos.yashnilay.ca` to `server-public`; cache rules and WAF settings documented as a checklist in `docs/operations.md` (immutable caching for `/i/`, `/o/`, `/v/`, `/static/`; bypass for `/s/`, `/api/`, `/u/`; WAF managed rules on; rate limiting rule for `/zip` and `/o/`).
- After cutover the router port-forward for 80/443 to swag can be removed for Yhotos; document the swag Cloudflare-IP allow list as an interim hardening for the other services.

### Backups
Nightly `pg_dump` (custom format, compressed) to the array with 30 daily and 12 monthly retention; weekly off-box copy target configurable (rclone remote; owner chooses provider). Originals are on the parity array; document that an off-site copy of originals is the owner's existing responsibility and recommend one. A monthly restore drill job restores the latest dump into a scratch database and runs a verification query set.

### Staging
`compose.staging.yml`: same images, separate database and data dir with a copy of the fixture library, admin and public both bound to the Tailscale IP on different ports, used for migration rehearsals, load tests, and the external pen test.

### Runbook (`docs/operations.md`)
First install, upgrades (pull new digest, migrate, restart in order worker, admin, public), rollback, rotating secrets, creating the first admin (`create-admin`), checking health, restoring from backup, responding to a security report.

## Acceptance

- [ ] `docker compose -f deploy/compose.staging.yml up` on yesktop (Docker available there) brings up a working stack with fixtures, admin reachable only on the bound address, public only on its internal network (verified with `curl` from inside and outside the network).
- [ ] Release workflow builds, signs and verifies images on a test tag (delete the tag afterwards).
- [ ] Restore drill succeeds against a dump taken from the staging stack.
- [ ] `scripts/security-smoke.sh` (task 018) passes against staging.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.5 is the checklist: tunnel only, no port forward for Yhotos, admin bound to Tailscale, read-only data for public, dropped capabilities, separate DB roles.
- 4.6: signed images, SBOM, pinned base images by digest.
- No secrets in the repo: `.env.example` only; real `.env` lives on tower with mode 600.

## Not in this task

- The DNS cutover and Piwigo retirement (done by the maintainer and the owner together after sign-off).
