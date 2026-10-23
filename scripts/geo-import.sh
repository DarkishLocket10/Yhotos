#!/usr/bin/env bash
# The operator supplies an unzipped GeoNames cities500.txt. Never downloads data.
set -euo pipefail
if [[ $# != 1 || -z "${DATABASE_URL:-}" ]]; then
    printf 'Usage: DATABASE_URL=... scripts/geo-import.sh /local/cities500.txt\n' >&2
    exit 2
fi
exec python3 "$(dirname -- "${BASH_SOURCE[0]}")/../deploy/geo/import.py" "$1"
