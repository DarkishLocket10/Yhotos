#!/usr/bin/env bash
# Sync the large fixture set from tower into fixtures/large/ (git-ignored).
#
# Usage: scripts/fixtures-sync.sh [subdir]   e.g. scripts/fixtures-sync.sh originals
# Requires ssh access to root@tower over Tailscale.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="root@tower:/mnt/user/data/yhotos-fixtures/${1:-}"
DST="$REPO_ROOT/fixtures/large/${1:-}"
mkdir -p "$DST"
rsync -a --info=progress2 --partial "$SRC/" "$DST/"
echo "synced to $DST"
du -sh "$DST"
