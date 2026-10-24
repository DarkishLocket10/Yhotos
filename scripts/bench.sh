#!/usr/bin/env bash
# Local/nightly origin measurements against synthetic data only.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
source scripts/bench-stack.sh
monitor_pid=
cleanup() {
    [[ -z "$monitor_pid" ]] || { kill "$monitor_pid" 2>/dev/null || true; wait "$monitor_pid" 2>/dev/null || true; }
    bench_compose down --volumes --remove-orphans
}
trap cleanup EXIT
bench_stack_start
scripts/security-smoke.sh
base=${YHOTOS_BENCH_URL:-http://127.0.0.1:${YHOTOS_BENCH_PORT:-18018}}
for scenario in album photo derivative search; do
    route=$(jq -er --arg key "$scenario" '.[$key]' bench-results/routes.json)
    # Warm origin caches; reject missing routes rather than timing fast failures.
    for ((i=0; i<10; i++)); do curl --fail --silent --show-error --max-time 15 "$base$route" -o /dev/null; done
    printf '%s\n' "$base$route" > "bench-results/$scenario.urls"
done
cat bench-results/{album,photo,derivative,search}.urls > bench-results/mixed.urls
# Read the server's /proc, not container memory usage (which includes file cache).
# Sampling inside its PID namespace works with rootless/nested Docker as well.
# shellcheck disable=SC2016 # Expansion belongs to the container shell.
bench_compose exec -T server bash -ceu '
while true; do
  awk "/^VmRSS:/ {print \$2}" /proc/1/status
  sleep 0.2
done' > bench-results/rss-kib.txt &
monitor_pid=$!

# shellcheck disable=SC2016 # awk evaluates fields inside the container.
cpu_usage() { bench_compose exec -T server awk '$1 == "usage_usec" {print $2}' /sys/fs/cgroup/cpu.stat; }
for scenario in album photo derivative search mixed; do
    # The 100-view concurrency budget applies to albums. Keep search below its
    # production 16-request admission cap so this measures work rather than 429s.
    concurrency=100
    case "$scenario" in search|mixed) concurrency=4;; esac
    cpu_before=$(cpu_usage)
    time_before=$(date +%s%N)
    oha --no-tui --output-format json -z "${YHOTOS_LOAD_DURATION:-30s}" -w -c "$concurrency" \
        -H 'Cache-Control: no-cache' --urls-from-file "bench-results/$scenario.urls" > "bench-results/$scenario.json"
    time_after=$(date +%s%N)
    cpu_after=$(cpu_usage)
    jq -n --argjson before "$cpu_before" --argjson after "$cpu_after" --argjson elapsed "$((time_after-time_before))" \
        '{cores: (($after-$before) * 1000 / $elapsed)}' > "bench-results/$scenario.cpu.json"
done
kill "$monitor_pid" 2>/dev/null || true
wait "$monitor_pid" 2>/dev/null || true
monitor_pid=
python3 scripts/load-report.py bench-results "${YHOTOS_PREVIOUS_LOAD:-bench-results/previous/load.json}"
