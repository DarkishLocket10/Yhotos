#!/usr/bin/env bash
# Against a disposable account created with create-admin. No credentials printed.
set -euo pipefail
: "${YHOTOS_USERNAME:?Set YHOTOS_USERNAME to a new temporary-password account}"
: "${YHOTOS_TEMP_PASSWORD:?Set YHOTOS_TEMP_PASSWORD from create-admin output}"
base="${YHOTOS_AUTH_URL:-http://localhost:8080}"
base="${base%/}"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT
chmod 700 "$work"
umask 077
printf '%s' "$YHOTOS_USERNAME" > "$work/username"
printf '%s' "$YHOTOS_TEMP_PASSWORD" > "$work/current"
printf '%s' "${YHOTOS_NEW_PASSWORD:-$(openssl rand -hex 24)}" > "$work/new"
request() {
    curl --silent --show-error --cookie "$work/cookies" --cookie-jar "$work/cookies" \
        --dump-header "$work/headers" --output "$work/body" --write-out '%{http_code}' "$@"
}
csrf() { awk '$6 == "__Host-yhotos-csrf" { print $7 }' "$work/cookies"; }
expect() { if [[ "$1" != "$2" ]]; then printf 'FAIL: expected HTTP %s, got %s\n' "$2" "$1" >&2; exit 1; fi; }
location() { tr -d '\r' < "$work/headers" | rg -qi "^location: $1$"; }
expect "$(request "$base/login")" 200
expect "$(request "$base/login" --data-urlencode "_csrf=$(csrf)" --data-urlencode "username@$work/username" --data-urlencode "password@$work/current")" 303
location /u/password
printf 'PASS: login requires password change\n'
expect "$(request "$base/u/security")" 303
location /u/password
printf 'PASS: security page blocked until password change\n'
expect "$(request "$base/u/password")" 200
expect "$(request "$base/u/password" --data-urlencode "_csrf=$(csrf)" --data-urlencode "new_password@$work/new")" 303
expect "$(request "$base/u/security")" 200
printf 'PASS: password changed and security page accessible\n'
if command -v oathtool >/dev/null 2>&1; then
    expect "$(request "$base/u/security/totp")" 200
    secret="$(sed -n 's/.*secret=\([A-Z2-7]*\).*/\1/p' "$work/body" | head -1)"
    code="$(printf '%s' "$secret" | oathtool --totp --base32 -)"
    expect "$(request "$base/u/security/totp" --data-urlencode "_csrf=$(csrf)" --data-urlencode "code=$code")" 303
    unset secret code
    printf 'PASS: authenticator app enrolled\n'
else
    printf 'SKIP: authenticator app enrollment (oathtool unavailable)\n'
fi
expect "$(request "$base/u/security")" 200
expect "$(request "$base/logout" --data-urlencode "_csrf=$(csrf)")" 303
expect "$(request "$base/u/security")" 303
location /login
printf 'PASS: logout invalidates session immediately\n'
