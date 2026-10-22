#!/usr/bin/env bash
# Run the gpt-6-astra review on a task's PR and post it as a comment.
#
# Usage: scripts/agent-review.sh NNN [--effort high|xhigh] [--host yesktop]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TASK_ID="${1:?task id required}"
shift || true
[[ "$TASK_ID" =~ ^[0-9]{3}$ ]] || { echo "task id must be three digits" >&2; exit 2; }
EFFORT=""
HOST="local"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --effort) EFFORT="${2:?}"; shift 2 ;;
    --host) HOST="${2:?}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ "$HOST" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "bad host name" >&2; exit 2; }
[[ -z "$EFFORT" || "$EFFORT" =~ ^(low|medium|high|xhigh|max)$ ]] || { echo "bad effort" >&2; exit 2; }

if [[ "$HOST" != "local" ]]; then
  REMOTE_CMD="cd ~/work/Yhotos && git fetch -q origin && scripts/agent-review.sh $(printf %q "$TASK_ID")${EFFORT:+ --effort $EFFORT}"
  ssh -o BatchMode=yes "$HOST" "bash -lc $(printf %q "$REMOTE_CMD")" < /dev/null
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
PROMPT_FILE="$(mktemp)"
{
  cat "$REPO_ROOT/scripts/review-prompt.md"
  echo
  echo "The PR is task $TASK_ID ($SLUG) on branch $BRANCH. The base is origin/main."
  echo "Get the diff with: git diff origin/main...HEAD   and the file list with: git diff --name-only origin/main...HEAD"
  echo "You may read any file in the worktree. Do not modify anything."
  echo "Your final message must be the review in the format above and nothing else."
} > "$PROMPT_FILE"
LAST="$(mktemp)"
codex exec \
  -C "$WT" \
  -m gpt-6-astra \
  -c "model_reasoning_effort=\"$EFFORT\"" \
  -c 'approval_policy="never"' \
  -c 'mcp_servers={}' \
  -s read-only \
  --output-last-message "$LAST" \
  - < "$PROMPT_FILE" > "$OUT" 2>&1 || true
rm -f "$PROMPT_FILE"
if [[ -s "$LAST" ]]; then cp "$LAST" "$OUT"; fi
rm -f "$LAST"

{
  echo "## Automated review (gpt-6-astra, effort $EFFORT)"
  echo
  cat "$OUT"
} | gh pr comment "$BRANCH" --body-file -
echo "posted review on PR for $BRANCH"
