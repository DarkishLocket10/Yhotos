#!/usr/bin/env bash
# Verify the real text, pgvector and fused paths using only seeded fixtures.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
base=${YHOTOS_BENCH_URL:-http://127.0.0.1:${YHOTOS_BENCH_PORT:-18018}}
routes=${YHOTOS_BENCH_ROUTES:-bench-results/routes.json}
private=$(jq -er '.restricted | ltrimstr("/p/")' "$routes")
seed=$(jq -er '.photo | ltrimstr("/p/")' "$routes")
count=$(jq -er '.photos' "$routes")
for key in search_text search_semantic search; do
    route=$(jq -er --arg key "$key" '.[$key]' "$routes")
    data=$(curl --fail --silent --show-error --max-time 15 "$base/api/v1${route}")
    jq -e --arg private "$private" '
      .total > 0 and (.photos | length > 0) and
      all(.photos[]; .id != $private and (.title | startswith("Garden view ")))
    ' <<< "$data" >/dev/null
    if [[ "$key" == search_semantic ]]; then
        expected=$((count - 1 < 200 ? count - 1 : 200))
        jq -e --arg seed "$seed" --argjson expected "$expected" '
          .total == $expected and all(.photos[]; .id != $seed)
        ' <<< "$data" >/dev/null
    elif [[ "$key" == search_text ]]; then
        expected=$((count < 200 ? count : 200))
        jq -e --argjson expected "$expected" '.total == $expected' <<< "$data" >/dev/null
    else
        # A score above the maximum one-list score proves both lists contributed.
        jq -e 'any(.photos[]; .score > (1 / 61))' <<< "$data" >/dev/null
    fi
done
echo 'Search smoke passed: text, seeded semantic neighbors, fusion and visibility'
