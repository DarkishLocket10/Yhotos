#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

run_step() {
    local step="$1"
    shift
    printf '\nChecking %s\n' "$step"
    if "$@"; then
        return 0
    else
        local status=$?
        printf 'FAILED: %s (exit %d)\n' "$step" "$status" >&2
        return "$status"
    fi
}

# A disposable pgvector service is started automatically unless the caller
# supplies DATABASE_URL. See deploy/dev/README.md for credentials and CLI setup.
if [[ -z "${DATABASE_URL:-}" ]]; then
    source deploy/dev/start.sh
fi
run_step migrations cargo sqlx migrate run --source crates/yhotos-db/migrations
export SQLX_OFFLINE=true

run_step fmt cargo fmt --all --check
run_step clippy cargo clippy --workspace --all-targets --all-features -- -D warnings
run_step test cargo test --workspace --all-features
run_step deny cargo deny check
run_step audit cargo audit

if command -v gitleaks >/dev/null 2>&1; then
    run_step gitleaks gitleaks detect --no-banner --redact
else
    printf 'WARNING: gitleaks is not on PATH; skipping local secret scan (required in CI).\n' >&2
fi
