#!/usr/bin/env bash
# Source for a worktree-scoped disposable stack; caller owns the EXIT cleanup.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
yhotos_bench_hash=$(printf '%s' "$PWD" | cksum | awk '{print $1}')
export COMPOSE_PROJECT_NAME="yhotos-bench-${yhotos_bench_hash}-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}"
export YHOTOS_BENCH_URL="${YHOTOS_BENCH_URL:-http://127.0.0.1:${YHOTOS_BENCH_PORT:-18018}}"
export YHOTOS_LIGHTHOUSE_URL="${YHOTOS_LIGHTHOUSE_URL:-https://localhost:${YHOTOS_LIGHTHOUSE_PORT:-18019}}"
bench_compose() { docker compose -f deploy/dev/compose.bench.yml "$@"; }
bench_stack_start() {
    mkdir -p bench-results
    npm --prefix web ci --no-audit
    npm --prefix web run build
    node deploy/dev/fixtures/generate.mjs
    bench_compose up --build -d --wait --wait-timeout 300
    bench_compose exec -T edge cat /data/caddy/pki/authorities/local/root.crt > bench-results/lighthouse-ca.crt
    # Container-local health can precede readiness of Docker's published ports.
    for url in "$YHOTOS_BENCH_URL" "$YHOTOS_LIGHTHOUSE_URL"; do
        curl --fail --silent --show-error --retry 10 --retry-delay 1 --retry-all-errors \
            --cacert bench-results/lighthouse-ca.crt --max-time 5 "$url/healthz" -o /dev/null
    done
    bench_compose cp server:/fixture/routes.json bench-results/routes.json
    scripts/search-smoke.sh
}
