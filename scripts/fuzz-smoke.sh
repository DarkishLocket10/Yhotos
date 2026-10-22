#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
seconds=${FUZZ_SECONDS:-60}
[[ "$seconds" =~ ^[1-9][0-9]*$ ]] || { echo 'Invalid FUZZ_SECONDS' >&2; exit 1; }
changed=all
if [[ ${1:-} == --changed ]]; then
    [[ ${2:-} =~ ^[0-9a-f]{40}$ ]] || { echo 'Expected merge-base SHA' >&2; exit 1; }
    changed=$(git diff --name-only "$2" HEAD)
fi
failed=0
for target in identify token_parse safe_join range_header tus_headers; do
    case "$target" in
        identify) paths='^crates/(yhotos-codec|yhotos-core)/';;
        token_parse) paths='^crates/(yhotos-auth|yhotos-core)/';;
        safe_join) paths='^crates/(yhotos-storage|yhotos-core)/';;
        range_header) paths='^crates/(yhotos-http|yhotos-storage)/';;
        tus_headers) paths='^crates/(yhotos-http|yhotos-config|yhotos-core)/';;
    esac
    if [[ "$changed" != all ]] && ! grep -Eq "$paths|^fuzz/|^Cargo\.(toml|lock)$|^rust-toolchain.toml$|^scripts/fuzz-smoke.sh$|^\.github/workflows/fuzz.yml$" <<< "$changed"; then
        continue
    fi
    mkdir -p "fuzz/corpus/$target" "fuzz/artifacts/$target"
    echo "Fuzzing $target for $seconds seconds"
    if ! cargo +"${FUZZ_TOOLCHAIN:-nightly}" fuzz run "$target" -- -max_total_time="$seconds" -timeout=30 -rss_limit_mb=4096 -max_len=1048576; then
        failed=1
    elif [[ ${FUZZ_MINIMIZE:-false} == true ]]; then
        timeout 600 cargo +"${FUZZ_TOOLCHAIN:-nightly}" fuzz cmin "$target" -- -max_total_time=540 || failed=1
    fi
done
exit "$failed"
