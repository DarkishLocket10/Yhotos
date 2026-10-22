#!/usr/bin/env bash
# Run a task brief with Codex in an isolated worktree and open a PR.
#
# Usage:
#   scripts/agent-run.sh NNN                 run tasks/NNN-*.md on this machine
#   scripts/agent-run.sh NNN --resume "msg"  continue in the existing worktree with extra instructions
#   scripts/agent-run.sh NNN --host yesktop  run on yesktop over ssh (repo must exist at ~/work/Yhotos there)
#   scripts/agent-run.sh NNN --base REF      branch from REF instead of origin/main
#
# Reads model/effort from the brief header. Refuses to start under 8 GB free memory.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TASK_ID="${1:?task id required}"
shift || true
RESUME_MSG=""
HOST="local"
BASE="origin/main"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --resume) RESUME_MSG="${2:?}"; shift 2 ;;
    --host) HOST="${2:?}"; shift 2 ;;
    --base) BASE="${2:?}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ "$HOST" != "local" ]]; then
  # Re-invoke this script on the remote machine. The remote checkout pulls main first.
  ssh -o BatchMode=yes "$HOST" "bash -lc 'cd ~/work/Yhotos && git fetch -q origin && git checkout -q main && git reset -q --hard origin/main && scripts/agent-run.sh $TASK_ID ${RESUME_MSG:+--resume \"$RESUME_MSG\"}'"
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
if (( FREE < 8 )); then
  echo "refusing to start: only ${FREE} GB free memory" >&2
  exit 3
fi

cd "$REPO_ROOT"
git fetch -q origin

if [[ -z "$RESUME_MSG" ]]; then
  if [[ -d "$WT" ]]; then
    echo "worktree $WT exists; use --resume or remove it" >&2
    exit 1
  fi
  if git show-ref --quiet "refs/remotes/origin/$BRANCH"; then
    git worktree add -q "$WT" "$BRANCH"
  else
    git worktree add -q -b "$BRANCH" "$WT" "$BASE"
  fi
else
  [[ -d "$WT" ]] || { echo "no worktree to resume at $WT" >&2; exit 1; }
fi

PROMPT_FILE="$(mktemp)"
trap 'rm -f "$PROMPT_FILE"' EXIT
{
  echo "You are implementing one task for Yhotos. Read AGENTS.md first, then the brief below in full, then the docs it lists."
  echo "Work only inside this worktree. Do not push, do not open a PR, do not run git commands that touch other branches."
  echo "Run scripts/check.sh before you finish. Commit your work in small logical commits using the commit style in AGENTS.md."
  echo "Your final message must be the PR description, following .github/pull_request_template.md exactly, starting with the '## Task' heading."
  echo
  if [[ -n "$RESUME_MSG" ]]; then
    echo "This is a continuation. A reviewer asked for changes. Address every point below, re-run scripts/check.sh, commit, and produce an updated PR description as your final message."
    echo
    echo "Reviewer notes:"
    echo "$RESUME_MSG"
    echo
  fi
  echo "----- BRIEF -----"
  cat "$BRIEF"
} > "$PROMPT_FILE"

LAST_MSG="$WT/.agent-last-message.md"
echo "running codex ($MODEL, effort $EFFORT) in $WT on $BRANCH"
codex exec \
  -C "$WT" \
  -m "$MODEL" \
  -c "model_reasoning_effort=\"$EFFORT\"" \
  -c 'approval_policy="never"' \
  -c 'mcp_servers={}' \
  -c 'sandbox_workspace_write.network_access=true' \
  -s workspace-write \
  --output-last-message "$LAST_MSG" \
  - < "$PROMPT_FILE"

cd "$WT"
if [[ -n "$(git status --porcelain)" ]]; then
  git add -A
  git -c user.name="$(git config user.name)" commit -q -m "Task $TASK_ID: remaining changes

- committed by agent-run.sh after the agent finished"
fi

git push -q -u origin "$BRANCH"

BODY_FILE="$(mktemp)"
if [[ -f "$LAST_MSG" ]] && grep -q '^## Task' "$LAST_MSG"; then
  sed -n '/^## Task/,$p' "$LAST_MSG" > "$BODY_FILE"
else
  { echo "## Task"; echo; echo "tasks/$(basename "$BRIEF")"; echo; echo "Agent did not produce a PR description. See commits."; } > "$BODY_FILE"
fi

if gh pr view "$BRANCH" --json number >/dev/null 2>&1; then
  gh pr edit "$BRANCH" --body-file "$BODY_FILE" >/dev/null
  echo "updated PR for $BRANCH"
else
  gh pr create --base main --head "$BRANCH" --title "$TASK_ID $SLUG" --body-file "$BODY_FILE"
fi
rm -f "$BODY_FILE"
