#!/usr/bin/env bash
# Source for a worktree-scoped disposable stack; caller owns the EXIT cleanup.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
yhotos_bench_hash=$(printf '%s' "$PWD" | cksum | awk '{print $1}')
export COMPOSE_PROJECT_NAME="yhotos-bench-${yhotos_bench_hash}-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}"
export YHOTOS_BENCH_URL="${YHOTOS_BENCH_URL:-http://127.0.0.1:${YHOTOS_BENCH_PORT:-18018}}"
bench_compose() { docker compose -f deploy/dev/compose.bench.yml "$@"; }
bench_stack_start() {
    mkdir -p bench-results
    npm --prefix web ci --no-audit
    npm --prefix web run build
    node deploy/dev/fixtures/generate.mjs
    bench_compose up --build -d --wait --wait-timeout 300
    bench_compose cp server:/fixture/routes.json bench-results/routes.json
}
