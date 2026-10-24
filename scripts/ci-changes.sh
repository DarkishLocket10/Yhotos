#!/usr/bin/env bash
# Prints code=true|false to $GITHUB_OUTPUT: false only for a pull request whose every changed
# file is documentation, a task brief, or an agent-host script. Anything else, any error, and
# every non-PR event count as code. Inputs: EVENT, BASE, HEAD (set by the workflow).
set -uo pipefail
out="${GITHUB_OUTPUT:-/dev/stdout}"
code=true
if [[ "${EVENT:-}" == "pull_request" && -n "${BASE:-}" && -n "${HEAD:-}" ]]; then
  if files="$(git diff --name-only "$BASE...$HEAD")" && [[ -n "$files" ]]; then
    code=false
    while IFS= read -r f; do
      case "$f" in
        docs/*|tasks/*|*.md|LICENSE) ;;
        scripts/agent-run.sh|scripts/agent-review.sh|scripts/review-prompt.md) ;;
        scripts/mem-watchdog.sh|scripts/setup-agent-host-linux.sh) ;;
        *) code=true; echo "code change: $f" ;;
      esac
    done <<< "$files"
  fi
fi
echo "code=$code" >> "$out"
echo "code=$code"
