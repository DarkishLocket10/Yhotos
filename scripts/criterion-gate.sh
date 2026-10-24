#!/usr/bin/env bash
# Run complete bench targets alternately against the merge base and PR head.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
[[ ${1:-} =~ ^[0-9a-f]{40}$ ]] || { echo 'Expected merge-base SHA' >&2; exit 1; }
base_database=${BASE_DATABASE_URL:?Set BASE_DATABASE_URL to a separate disposable database}
[[ "$base_database" != "$DATABASE_URL" ]] || { echo 'Base and head require separate databases' >&2; exit 1; }
root=$PWD
rm -rf bench-results/base
mkdir -p bench-results/base
# Read the base tree without switching branches or carrying PR-generated files.
git archive "$1" | tar -x -C bench-results/base
export SQLX_OFFLINE=true
base_target="$root/bench-results/base-target"
head_target="$root/target"
for tree in "$root/bench-results/base" "$root"; do
    target=$head_target database=$DATABASE_URL
    if [[ "$tree" != "$root" ]]; then target=$base_target; database=$base_database; fi
    (cd "$tree"; DATABASE_URL="$database" cargo sqlx migrate run --source crates/yhotos-db/migrations)
    (cd "$tree"; CARGO_TARGET_DIR="$target" cargo build --locked --release -p yhotos-codec-host)
    (cd "$tree"; CARGO_TARGET_DIR="$target" cargo bench --locked -p yhotos-bench --benches --no-run)
done
# Old/stale result directories must never count as measurements from this run.
rm -rf "$base_target/criterion" "$head_target/criterion"
mapfile -t benches < <(find crates/yhotos-bench/benches bench-results/base/crates/yhotos-bench/benches -name '*.rs' -exec basename '{}' .rs \; | sort -u)
index=0
for bench in "${benches[@]}"; do
    order=(main pr)
    if ((index % 2)); then order=(pr main); fi
    for version in "${order[@]}"; do
        tree=$root target=$head_target database=$DATABASE_URL
        if [[ "$version" == main ]]; then tree="$root/bench-results/base"; target=$base_target; database=$base_database; fi
        [[ -f "$tree/crates/yhotos-bench/benches/$bench.rs" ]] || continue
        (cd "$tree"; DATABASE_URL="$database" CRITERION_HOME="$target/criterion" YHOTOS_WORKER__CODEC_HOST_PATH="$target/release/yhotos-codec-host" CARGO_TARGET_DIR="$target" cargo bench --locked -p yhotos-bench --bench "$bench" -- \
            --save-baseline "$version" --noplot)
    done
    index=$((index + 1))
done
cargo run --locked -p yhotos-bench --bin compare -- "$base_target/criterion" "$head_target/criterion" > bench-results/compare.md
