#!/usr/bin/env bash
set -euo pipefail
# The key exists only in this disposable server process, never in logs/artifacts.
export YHOTOS_SESSION_KEY
YHOTOS_SESSION_KEY=$(head -c 32 /dev/urandom | base64)
exec yhotos-server
