# Yhotos

A self-hosted photo gallery built from scratch in Rust. It is the successor to a Piwigo install and is designed around three things that are measured, not promised: security, speed, and image fidelity.

Status: scoping complete, no code yet. See [docs/scope.md](docs/scope.md) for the full scope and architecture.

## What it will do

- Public portfolio, logged-in users and groups with per-album and per-photo visibility, and tokenized share links with per-link permissions and expiry
- Color-managed image pipeline: ICC aware, wide gamut, gain-map HDR, RAW previews, AVIF and JPEG XL derivatives generated once at upload
- Server-rendered pages with small TypeScript islands, cached at the edge
- Search with embeddings, auto tags, OCR, EXIF and place filters, run by a GPU worker outside the request path
- First-party, cookie-free analytics
- Themeable in layers without runtime cost
- Admin reachable only over a private network

## Development

Planning documents and task briefs live in this repo. Implementation is delegated to coding agents working from those briefs, with automated review and CI gates on every pull request. See the process section of the scope document.

Prerequisites: Rust stable (the exact version and rustfmt/clippy components are
pinned in `rust-toolchain.toml`), Docker for PostgreSQL 16 tests, and Node.js 22 or
newer with npm for the TypeScript islands. The codec skeleton needs no system
libraries; libvips will be required when the codec implementation is added.

Install the dependency checks once:

```sh
cargo install --locked cargo-deny --version 0.20.2
cargo install --locked cargo-audit --version 0.22.2
```

Install [Gitleaks](https://github.com/gitleaks/gitleaks/releases/tag/v8.30.1) on
`PATH` for local secret scanning. CI requires it; the local check script warns
and continues if it is missing.

```sh
cargo build --workspace
scripts/check.sh
cd web
npm ci
npm run build
```

`scripts/check.sh` runs the same Rust and secret checks as CI: formatting, Clippy,
workspace tests, cargo-deny, cargo-audit, and Gitleaks. It stops at the first
failure and names the failed step. CI also builds the TypeScript islands.
The web build bundles `web/src/*.ts` into `crates/yhotos-http/assets/js/` as
minified ES2022 modules without source maps. Esbuild strips TypeScript types;
it does not type-check them. `web/tsconfig.json` sets strict options for editors.

The skeleton tests do not need a database yet. To run tests that need PostgreSQL,
start a local disposable database from the repository root (requires `openssl`
to generate a fresh password):

```sh
export YHOTOS_TEST_PASSWORD="$(openssl rand -hex 24)"
docker run --detach --rm --name yhotos-postgres \
  --publish 127.0.0.1:5432:5432 \
  --env POSTGRES_USER=yhotos --env POSTGRES_DB=yhotos_test \
  --env POSTGRES_PASSWORD="$YHOTOS_TEST_PASSWORD" postgres:16
until docker exec yhotos-postgres pg_isready --username yhotos --dbname yhotos_test; do
  sleep 1
done
export DATABASE_URL="postgres://yhotos:${YHOTOS_TEST_PASSWORD}@127.0.0.1:5432/yhotos_test"
cargo test --workspace --all-features
docker stop yhotos-postgres
unset DATABASE_URL YHOTOS_TEST_PASSWORD
```

## Security

Internet facing and open source, so it is built as if every line will be attacked. Please read [SECURITY.md](SECURITY.md) before reporting a vulnerability.

## License

AGPL-3.0. See [LICENSE](LICENSE).
