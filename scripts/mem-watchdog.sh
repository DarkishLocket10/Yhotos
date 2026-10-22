#!/usr/bin/env bash
# Pause agent runs and Rust builds when memory runs low, resume when it recovers.
#
# Usage: scripts/mem-watchdog.sh            (run in the background on each agent host)
#
# Pauses with SIGSTOP (no work is lost) every process in the trees of the agent runs listed in
# .worktrees/.running/*.pid, plus any cargo, rustc and test binaries of this repository.
# macOS reads kern.memorystatus_level (percent available); Linux reads MemAvailable/MemTotal.
# Overrides: YHOTOS_WATCHDOG_PAUSE_BELOW (default 15), YHOTOS_WATCHDOG_RESUME_ABOVE (default 30).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="$REPO_ROOT/.worktrees/.running"
PAUSE_BELOW="${YHOTOS_WATCHDOG_PAUSE_BELOW:-15}"
RESUME_ABOVE="${YHOTOS_WATCHDOG_RESUME_ABOVE:-30}"

level() {
  if [[ "$(uname)" == "Darwin" ]]; then
    sysctl -n kern.memorystatus_level
  else
    awk '/MemTotal/ {t=$2} /MemAvailable/ {a=$2} END {printf "%d", a*100/t}' /proc/meminfo
  fi
}

tree() {
  local pid="$1"
  echo "$pid"
  for child in $(pgrep -P "$pid" 2>/dev/null); do tree "$child"; done
}

targets() {
  for f in "$RUN_DIR"/*.pid; do
    [[ -f "$f" ]] || continue
    local pid; pid="$(cat "$f")"
    kill -0 "$pid" 2>/dev/null && tree "$pid"
  done
  # Builds and test binaries started outside agent runs (for example by the maintainer).
  pgrep -u "$(id -u)" -x cargo 2>/dev/null
  pgrep -u "$(id -u)" -x rustc 2>/dev/null
  pgrep -u "$(id -u)" -f "$REPO_ROOT/.*target/.*/deps/" 2>/dev/null
}

log() { echo "$(date -u +%FT%TZ) $*"; }

paused=0
log "watchdog started: pause below ${PAUSE_BELOW}%, resume above ${RESUME_ABOVE}%"
trap 'if (( paused )); then targets | sort -u | xargs kill -CONT 2>/dev/null; fi; exit 0' INT TERM
while true; do
  l="$(level)"
  if (( l < PAUSE_BELOW )); then
    # Re-send on every tick so processes started since the last pause are caught too.
    pids="$(targets | sort -u | tr '\n' ' ')"
    if [[ -n "${pids// /}" ]]; then
      # shellcheck disable=SC2086
      kill -STOP $pids 2>/dev/null
      (( paused )) || log "memory at ${l}%: paused ${pids}"
      paused=1
    fi
  elif (( paused && l > RESUME_ABOVE )); then
    pids="$(targets | sort -u | tr '\n' ' ')"
    # shellcheck disable=SC2086
    kill -CONT $pids 2>/dev/null
    log "memory at ${l}%: resumed"
    paused=0
  fi
  # Wait on a background sleep so INT/TERM run the trap (and resume everything) at once.
  sleep 5 &
  wait $!
done
