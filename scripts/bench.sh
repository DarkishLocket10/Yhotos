#!/usr/bin/env bash
# Local/nightly origin measurements against synthetic data only.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
source scripts/bench-stack.sh
monitor_pid=
monitor_remote_pid=
stop_monitor() {
    if [[ -n "$monitor_remote_pid" ]]; then
        # Terminating only the Compose client leaves the container exec (and its
        # stdout writer) alive, contaminating this file with later scenarios.
        # shellcheck disable=SC2016 # The container shell reads its positional argument.
        bench_compose exec -T server bash -c 'kill -TERM "$1"' -- "$monitor_remote_pid" || true
        monitor_remote_pid=
    elif [[ -n "$monitor_pid" ]]; then
        kill "$monitor_pid" 2>/dev/null || true
    fi
    if [[ -n "$monitor_pid" ]]; then
        wait "$monitor_pid" 2>/dev/null || true
        monitor_pid=
    fi
}
cleanup() {
    stop_monitor
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
# Keep each scenario's samples separate: the 200 MB budget is for album views.

# shellcheck disable=SC2016 # awk evaluates fields inside the container.
cpu_usage() { bench_compose exec -T server awk '$1 == "usage_usec" {print $2}' /sys/fs/cgroup/cpu.stat; }
for scenario in album photo derivative search mixed; do
    : > "bench-results/$scenario.rss-kib.txt"
    # shellcheck disable=SC2016 # Expansion belongs to the container shell.
    bench_compose exec -T server bash -ceu '
    trap "exit 0" TERM
    printf "pid=%s\n" "$$"
    while true; do
      awk "/^VmRSS:/ {print \$2}" /proc/1/status
      sleep 0.2
    done' > "bench-results/$scenario.rss-kib.txt" &
    monitor_pid=$!
    for ((attempt=0; attempt<50; attempt++)); do
        if IFS= read -r marker < "bench-results/$scenario.rss-kib.txt" &&
            [[ "$marker" =~ ^pid=([1-9][0-9]{0,9})$ ]] && ((10#${BASH_REMATCH[1]} > 1)); then
            monitor_remote_pid=${BASH_REMATCH[1]}
            break
        fi
        sleep 0.1
    done
    [[ -n "$monitor_remote_pid" ]] || { echo 'RSS sampler did not start' >&2; exit 1; }
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
    stop_monitor
    sed -i '1d' "bench-results/$scenario.rss-kib.txt"
    jq -n --argjson before "$cpu_before" --argjson after "$cpu_after" --argjson elapsed "$((time_after-time_before))" \
        '{cores: (($after-$before) * 1000 / $elapsed)}' > "bench-results/$scenario.cpu.json"
done
cat bench-results/{album,photo,derivative,search,mixed}.rss-kib.txt > bench-results/rss-kib.txt
python3 scripts/load-report.py bench-results "${YHOTOS_PREVIOUS_LOAD:-bench-results/previous/load.json}"
