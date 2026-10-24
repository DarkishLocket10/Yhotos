#!/usr/bin/env bash
# Run a task brief with Codex in an isolated worktree and open a PR.
#
# Exit codes: 0 pushed, 3 host busy or low on memory, 5 agent did not finish (nothing pushed).
# Usage:
#   scripts/agent-run.sh NNN                 run tasks/NNN-*.md on this machine
#   scripts/agent-run.sh NNN --resume "msg"  continue in the existing worktree with extra instructions
#   scripts/agent-run.sh NNN --host yesktop  run on yesktop over ssh (repo must exist at ~/work/Yhotos there)
#   scripts/agent-run.sh NNN --base REF      branch from REF instead of origin/main
#
# Optional YHOTOS_AGENT_NOTE env var: extra maintainer notes (for example test service URLs)
# added to the prompt.
# Reads model/effort from the brief header. Refuses to start when free memory is low or the
# host already runs its maximum number of agents; see the limits block below.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TASK_ID="${1:?task id required}"
shift || true
[[ "$TASK_ID" =~ ^[0-9]{3}$ ]] || { echo "task id must be three digits" >&2; exit 2; }
RESUME_MSG=""
HOST="local"
BASE="origin/main"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --resume) RESUME_MSG="${2:?}"; shift 2 ;;
    --resume-stdin) RESUME_MSG="$(cat)"; shift ;;
    --host) HOST="${2:?}"; shift 2 ;;
    --base) BASE="${2:?}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ "$HOST" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "bad host name" >&2; exit 2; }
[[ "$BASE" =~ ^[A-Za-z0-9._/-]+$ ]] || { echo "bad base ref" >&2; exit 2; }

if [[ "$HOST" != "local" ]]; then
  # Re-invoke this script on the remote machine. The remote checkout is moved to BASE so the
  # scripts and briefs it runs are the ones from that ref. Free text (the resume message) travels
  # on stdin, never on the command line, so it cannot be interpreted by the remote shell.
  REMOTE_CMD="cd ~/work/Yhotos && git fetch -q origin && git checkout -q --detach $(printf %q "$BASE") && source ~/.cargo/env 2>/dev/null; scripts/agent-run.sh $(printf %q "$TASK_ID") --base $(printf %q "$BASE")"
  if [[ -n "$RESUME_MSG" ]]; then
    printf '%s' "$RESUME_MSG" | ssh -o BatchMode=yes "$HOST" "bash -lc $(printf %q "$REMOTE_CMD --resume-stdin")"
  else
    ssh -o BatchMode=yes "$HOST" "bash -lc $(printf %q "$REMOTE_CMD")" < /dev/null
  fi
  exit $?
fi

BRIEF="$(ls "$REPO_ROOT"/tasks/"$TASK_ID"-*.md 2>/dev/null | head -1)"
[[ -f "$BRIEF" ]] || { echo "no brief for task $TASK_ID" >&2; exit 1; }

header() { sed -n '/^---$/,/^---$/p' "$BRIEF" | grep -E "^$1:" | head -1 | sed -E "s/^$1: *//"; }
SLUG="$(header slug)"
MODEL="$(header model)"; MODEL="${MODEL:-gpt-6.1-sol}"
EFFORT="$(header effort)"; EFFORT="${EFFORT:-high}"
BRANCH="y/${TASK_ID}-${SLUG}"
WT="$REPO_ROOT/.worktrees/$TASK_ID"

# Memory guard (Linux and macOS).
free_gb() {
  if [[ "$(uname)" == "Darwin" ]]; then
    local page; page="$(sysctl -n hw.pagesize)"
    vm_stat | awk -v p="$page" '/Pages free|Pages inactive|Pages speculative|Pages purgeable/ {s+=$NF} END {printf "%d", s*p/1073741824}'
  else
    awk '/MemAvailable/ {printf "%d", $2/1048576}' /proc/meminfo
  fi
}
FREE="$(free_gb)"
# Limits per host. vm_stat undercounts reclaimable memory on macOS, so the Mac uses a lower
# floor. On Linux each agent also runs in a systemd scope with a hard memory ceiling, so a
# runaway build is killed inside its own scope instead of exhausting the machine (an unbounded
# cargo -j30 per agent caused global OOM kills on yesktop on 2026-10-21).
# Overrides: YHOTOS_AGENT_MIN_FREE_GB, YHOTOS_AGENT_MAX_RUNNING, YHOTOS_AGENT_MEM_MAX.
# 2026-10-22: one agent per host after yesktop crashed twice and the 24 GB Mac reached 3.5 GB of
# swap with two agents. scripts/mem-watchdog.sh pauses builds if memory still runs low.
if [[ "$(uname)" == "Darwin" ]]; then
  MIN_FREE="${YHOTOS_AGENT_MIN_FREE_GB:-8}"
  MAX_RUNNING="${YHOTOS_AGENT_MAX_RUNNING:-1}"
  MAX_SWAP_GB="${YHOTOS_AGENT_MAX_SWAP_GB:-3}"
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
else
  # yesktop is shared with the owner's other work; leave it room.
  MIN_FREE="${YHOTOS_AGENT_MIN_FREE_GB:-20}"
  MAX_RUNNING="${YHOTOS_AGENT_MAX_RUNNING:-1}"
  MAX_SWAP_GB="${YHOTOS_AGENT_MAX_SWAP_GB:-2}"
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
fi
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-2}"
# Above MEM_HIGH the kernel throttles and reclaims; only MEM_MAX kills. At 12G with no soft
# limit, agents were OOM-killed mid-run on 2026-10-22 while linking test binaries.
MEM_HIGH="${YHOTOS_AGENT_MEM_HIGH:-9G}"
MEM_MAX="${YHOTOS_AGENT_MEM_MAX:-12G}"
# Full debug info dominates rustc and linker memory; line tables keep backtraces useful.
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-line-tables-only}"
export CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-line-tables-only}"
swap_used_gb() {
  if [[ "$(uname)" == "Darwin" ]]; then
    sysctl -n vm.swapusage | awk '{for (i=1;i<=NF;i++) if ($i=="used") {v=$(i+2); sub(/M/,"",v); printf "%d", v/1024}}'
  else
    awk '/SwapTotal/ {t=$2} /SwapFree/ {f=$2} END {printf "%d", (t-f)/1048576}' /proc/meminfo
  fi
}
if (( FREE < MIN_FREE )); then
  echo "refusing to start: only ${FREE} GB free memory (need ${MIN_FREE})" >&2
  exit 3
fi
SWAP="$(swap_used_gb)"
if (( SWAP > MAX_SWAP_GB )); then
  echo "refusing to start: ${SWAP} GB swap in use (max ${MAX_SWAP_GB})" >&2
  exit 3
fi
# Count running agent runs on this host through pid files; stale files are ignored.
RUN_DIR="$REPO_ROOT/.worktrees/.running"
mkdir -p "$RUN_DIR"
RUNNING=0
for f in "$RUN_DIR"/*.pid; do
  [[ -f "$f" ]] || continue
  if kill -0 "$(cat "$f")" 2>/dev/null; then RUNNING=$((RUNNING + 1)); else rm -f "$f"; fi
done
if (( RUNNING >= MAX_RUNNING )); then
  echo "refusing to start: ${RUNNING} agent runs already active on this host (max ${MAX_RUNNING})" >&2
  exit 3
fi
echo $$ > "$RUN_DIR/$TASK_ID.pid"

cd "$REPO_ROOT"
git fetch -q origin

if [[ -z "$RESUME_MSG" ]]; then
  if [[ -d "$WT" ]]; then
    echo "worktree $WT exists; use --resume or remove it" >&2
    exit 1
  fi
  if git show-ref --quiet "refs/remotes/origin/$BRANCH"; then
    echo "remote branch $BRANCH already exists; delete it (git push origin --delete $BRANCH) to start over, or use --resume" >&2
    exit 1
  fi
  git worktree add -q -b "$BRANCH" "$WT" "$BASE"
else
  [[ -d "$WT" ]] || { echo "no worktree to resume at $WT" >&2; exit 1; }
fi

PROMPT_FILE="$(mktemp)"
# Containers, volumes and networks that docker compose started from this worktree (dev
# databases, benchmark stacks). Agents have left these running for days; always remove them.
cleanup_containers() {
  command -v docker >/dev/null 2>&1 || return 0
  local id dir proj projects=""
  for id in $(docker ps -aq --filter label=com.docker.compose.project 2>/dev/null); do
    dir="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' "$id" 2>/dev/null)"
    case "$dir" in
      "$WT"|"$WT"/*)
        proj="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$id" 2>/dev/null)"
        projects="$projects $proj"
        docker rm -f -v "$id" >/dev/null 2>&1 ;;
    esac
  done
  for proj in $(printf '%s\n' $projects | sort -u); do
    docker volume ls -q --filter "label=com.docker.compose.project=$proj" | xargs -r docker volume rm >/dev/null 2>&1
    docker network ls -q --filter "label=com.docker.compose.project=$proj" | xargs -r docker network rm >/dev/null 2>&1
  done
}
trap 'cleanup_containers; rm -f "$PROMPT_FILE" "$RUN_DIR/$TASK_ID.pid"' EXIT
{
  echo "You are implementing one task for Yhotos. Read AGENTS.md first, then the brief below in full, then the docs it lists."
  echo "Work only inside this worktree. Do not push, do not open a PR, do not run git commands that touch other branches."
  echo "The .git directory is read only in your sandbox: do not try to commit; this script commits your working tree when you finish."
  echo "Run scripts/check.sh before you finish."
  echo "Your final message must be the PR description, following .github/pull_request_template.md exactly, starting with the '## Task' heading."
  echo
  if [[ -n "$RESUME_MSG" ]]; then
    echo "This is a continuation. A reviewer asked for changes. Address every point below, re-run scripts/check.sh, commit, and produce an updated PR description as your final message."
    echo
    echo "Reviewer notes:"
    echo "$RESUME_MSG"
    echo
  fi
  if [[ -n "${YHOTOS_AGENT_NOTE:-}" ]]; then
    echo "Environment notes from the maintainer (test services and similar):"
    echo "$YHOTOS_AGENT_NOTE"
    echo
  fi
  echo "----- BRIEF -----"
  cat "$BRIEF"
} > "$PROMPT_FILE"

LAST_MSG="$WT/.agent-last-message.md"
# A summary left over from an earlier round must never pass for this round's result.
rm -f "$LAST_MSG"
echo "running codex ($MODEL, effort $EFFORT) in $WT on $BRANCH (cargo jobs $CARGO_BUILD_JOBS)"
SCOPE=()
if [[ "$(uname)" == "Linux" ]] && command -v systemd-run >/dev/null 2>&1 \
  && systemd-run --user --scope -q -p MemoryMax=64M true 2>/dev/null; then
  SCOPE=(systemd-run --user --scope -q -p "MemoryHigh=$MEM_HIGH" -p "MemoryMax=$MEM_MAX" -p MemorySwapMax=0 --)
  echo "agent memory: throttle above $MEM_HIGH, hard ceiling $MEM_MAX"
fi
${SCOPE[@]+"${SCOPE[@]}"} codex exec \
  -C "$WT" \
  --add-dir "$REPO_ROOT/.git" \
  --add-dir "$HOME/.cargo" \
  --add-dir "$HOME/.rustup" \
  -m "$MODEL" \
  -c "model_reasoning_effort=\"$EFFORT\"" \
  -c 'approval_policy="never"' \
  -c 'mcp_servers={}' \
  -c 'sandbox_workspace_write.network_access=true' \
  -s workspace-write \
  --output-last-message "$LAST_MSG" \
  - < "$PROMPT_FILE" && AGENT_RC=0 || AGENT_RC=$?

# Only a finished agent's work is committed and pushed. A stopped or killed agent can still
# exit 0, so also require a fresh summary in the PR template format.
if (( AGENT_RC != 0 )) || [[ ! -s "$LAST_MSG" ]] || ! grep -q '^## Task' "$LAST_MSG"; then
  echo "agent did not finish (exit $AGENT_RC, summary $( [[ -s "$LAST_MSG" ]] && echo present || echo missing )):" \
    "changes left uncommitted in $WT; nothing pushed" >&2
  exit 5
fi

cd "$WT"
if [[ -n "$(git status --porcelain)" ]]; then
  git add -A
  git -c user.name="$(git config user.name)" commit -q -m "Task $TASK_ID: remaining changes

- committed by agent-run.sh after the agent finished"
fi

git push -q -u origin "$BRANCH"

BODY_FILE="$(mktemp)"
sed -n '/^## Task/,$p' "$LAST_MSG" > "$BODY_FILE"

if [[ "$(gh pr view "$BRANCH" --json state --jq .state 2>/dev/null)" == "OPEN" ]]; then
  gh pr edit "$BRANCH" --body-file "$BODY_FILE" >/dev/null
  echo "updated PR for $BRANCH"
else
  gh pr create --base main --head "$BRANCH" --title "$TASK_ID $SLUG" --body-file "$BODY_FILE"
fi
rm -f "$BODY_FILE"
# Build output is 14-30 GB per worktree. The source and branch stay for reviews and resumes;
# the next run rebuilds. Set YHOTOS_KEEP_TARGET=1 to keep it.
if [[ "${YHOTOS_KEEP_TARGET:-0}" != 1 ]]; then
  rm -rf "$WT/target"
fi
