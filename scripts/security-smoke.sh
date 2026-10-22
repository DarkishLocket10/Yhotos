#!/usr/bin/env bash
# Exercise the real public listener. Routes come from the synthetic fixture loader.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
base=${YHOTOS_BENCH_URL:-http://127.0.0.1:${YHOTOS_BENCH_PORT:-18018}}
routes=${YHOTOS_BENCH_ROUTES:-bench-results/routes.json}
[[ "$base" =~ ^https?://(127\.0\.0\.1|localhost)(:[0-9]+)?$ ]] || { echo 'Smoke tests require a loopback disposable server' >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail() { printf 'Security smoke failed: %s\n' "$*" >&2; exit 1; }
# Duplicate security headers are rejected rather than silently taking the last one.
check_headers() {
    jq -Rn '[inputs | sub("\r$"; "") | select(test("^[^ :]+:")) |
      capture("^(?<name>[^:]+):[ ]*(?<value>.*)$") | .name |= ascii_downcase] |
      group_by(.name) | map({key: .[0].name, value: map(.value)}) | from_entries' < "$tmp/headers" > "$tmp/headers.json"
    jq -e --slurpfile expected scripts/security-headers.json '
      . as $h | ($expected[0] | to_entries | all(. as $e | $h[$e.key] == [$e.value])) and
      (has("server") | not) and (has("set-cookie") | not) and
      (."content-security-policy" | length == 1) and
      (."content-security-policy"[0] | capture("^default-src '\''none'\''; img-src '\''self'\'' data:; script-src '\''nonce-(?<nonce>[A-Za-z0-9+/]{22}==)'\''; style-src '\''self'\'' '\''nonce-(?<style>[A-Za-z0-9+/]{22}==)'\''; connect-src '\''self'\''; frame-ancestors '\''none'\''; base-uri '\''none'\''; form-action '\''self'\''$") | .nonce == .style)
    ' "$tmp/headers.json" >/dev/null || fail "headers for $path"
    local corp=same-origin referrer=strict-origin-when-cross-origin
    case "$path" in /static/*|/i/*|/o/*|/v/*) corp=cross-origin;; esac
    case "$path" in /admin/*|/login*|/u/*|/s/*) referrer=no-referrer;; esac
    jq -e --arg corp "$corp" --arg referrer "$referrer" '
      ."cross-origin-resource-policy" == [$corp] and ."referrer-policy" == [$referrer]
    ' "$tmp/headers.json" >/dev/null || fail "route-specific headers for $path"
}
request() {
    path=$1
    [[ "$path" == /* && "$path" != //* && "$path" != *$'\r'* && "$path" != *$'\n'* ]] || fail 'invalid fixture route'
    status=$(curl --silent --show-error --max-time 15 --path-as-is --request "${2:-GET}" \
        --dump-header "$tmp/headers" --output "$tmp/body" --write-out '%{http_code}' "$base$path")
    check_headers
}
for key in home album photo derivative; do
    request "$(jq -er --arg key "$key" '.[$key] | strings' "$routes")"
    [[ "$status" == 200 ]] || fail "$key status $status"
done
request /healthz
[[ "$status" == 200 ]] || fail 'healthz status'
printf ok | cmp -s - "$tmp/body" || fail 'healthz must be exactly ok' 
for path in /admin /admin/ /admin/healthz /admin/login /admin/upload /admin/users /admin/settings; do
    request "$path"
    [[ "$status" == 404 ]] || fail "public admin route $path status $status"
done
request "$(jq -er .restricted "$routes")"
[[ "$status" == 404 ]] || fail 'restricted photo must return 404'
cp "$tmp/body" "$tmp/restricted"
request "$(jq -er .missing "$routes")"
[[ "$status" == 404 ]] || fail 'missing photo must return 404'
cmp -s "$tmp/body" "$tmp/restricted" || fail 'restricted and missing photo bodies differ'
# Exercise the default auth bucket without credentials or a CSRF cookie. The
# limiter wraps CSRF, so rejected POSTs consume tokens without expensive hashing.
limited=false
for ((attempt=0; attempt<40; attempt++)); do
    request /login POST
    if [[ "$status" == 429 ]]; then
        jq -e '."retry-after" | length == 1 and (.[0] | test("^[1-9][0-9]*$"))' "$tmp/headers.json" >/dev/null || fail '429 lacks Retry-After'
        limited=true
        break
    fi
    [[ "$status" == 403 || "$status" == 400 ]] || fail "unexpected rate-limit probe status $status"
done
[[ "$limited" == true ]] || fail 'auth rate limit did not return 429'
echo 'Security smoke passed: headers, CSP, admin isolation, private 404s, rate limit, health and cookies'
