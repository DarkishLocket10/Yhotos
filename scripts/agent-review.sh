#!/usr/bin/env bash
# Run the gpt-6-astra review on a task's PR and post it as a comment.
#
# Usage: scripts/agent-review.sh NNN [--effort high|xhigh] [--host yesktop]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TASK_ID="${1:?task id required}"
shift || true
EFFORT=""
HOST="local"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --effort) EFFORT="${2:?}"; shift 2 ;;
    --host) HOST="${2:?}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ "$HOST" != "local" ]]; then
  ssh -o BatchMode=yes "$HOST" "bash -lc 'cd ~/work/Yhotos && git fetch -q origin && scripts/agent-review.sh $TASK_ID ${EFFORT:+--effort $EFFORT}'"
  exit $?
fi

BRIEF="$(ls "$REPO_ROOT"/tasks/"$TASK_ID"-*.md | head -1)"
header() { sed -n '/^---$/,/^---$/p' "$BRIEF" | grep -E "^$1:" | head -1 | sed -E "s/^$1: *//"; }
SLUG="$(header slug)"
EFFORT="${EFFORT:-$(header review_effort)}"; EFFORT="${EFFORT:-high}"
BRANCH="y/${TASK_ID}-${SLUG}"
WT="$REPO_ROOT/.worktrees/$TASK_ID"

cd "$REPO_ROOT"
git fetch -q origin
if [[ ! -d "$WT" ]]; then
  git worktree add -q "$WT" "$BRANCH"
fi
cd "$WT"
git pull -q --ff-only origin "$BRANCH" || true

OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT
echo "reviewing $BRANCH with gpt-6-astra (effort $EFFORT)"
codex exec review \
  --base main \
  -m gpt-6-astra \
  -c "model_reasoning_effort=\"$EFFORT\"" \
  -c 'approval_policy="never"' \
  --title "$TASK_ID $SLUG" \
  - < "$REPO_ROOT/scripts/review-prompt.md" > "$OUT" 2>&1 || true

{
  echo "## Automated review (gpt-6-astra, effort $EFFORT)"
  echo
  # Keep only the review body: drop codex's own progress lines.
  sed -E '/^(\[|codex|thinking|exec|tokens used|-{3,})/d' "$OUT"
} | gh pr comment "$BRANCH" --body-file -
echo "posted review on PR for $BRANCH"
