#!/usr/bin/env bash
# Source this script to export DATABASE_URL, or execute it to start Postgres.
set -euo pipefail
_yhotos_dev_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ ! -f "$_yhotos_dev_dir/.env" ]]; then
    (umask 077; printf 'POSTGRES_PASSWORD=%s\n' "$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')" > "$_yhotos_dev_dir/.env")
fi
# This file is data, never shell code. Accept only the generated format.
_yhotos_dev_env="$(cat -- "$_yhotos_dev_dir/.env")"
if [[ ! "$_yhotos_dev_env" =~ ^POSTGRES_PASSWORD=([0-9a-f]{64})$ ]]; then
    printf 'Invalid dev database environment file.\n' >&2
    return 1 2>/dev/null || exit 1
fi
_yhotos_dev_password="${BASH_REMATCH[1]}"
_yhotos_dev_port="${POSTGRES_PORT:-55432}"
if [[ ! "$_yhotos_dev_port" =~ ^[0-9]{1,5}$ ]] || ((10#$_yhotos_dev_port < 1 || 10#$_yhotos_dev_port > 65535)); then
    printf 'Invalid dev database port.\n' >&2
    return 1 2>/dev/null || exit 1
fi
POSTGRES_PASSWORD="$_yhotos_dev_password" POSTGRES_PORT="$_yhotos_dev_port" docker compose --project-directory "$_yhotos_dev_dir" -f "$_yhotos_dev_dir/compose.yml" up -d --wait
export DATABASE_URL="postgres://yhotos:${_yhotos_dev_password}@127.0.0.1:${_yhotos_dev_port}/yhotos_test?sslmode=disable"
unset _yhotos_dev_password _yhotos_dev_env _yhotos_dev_port _yhotos_dev_dir
