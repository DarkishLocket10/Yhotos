#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
base=${YHOTOS_LIGHTHOUSE_URL:-https://localhost:${YHOTOS_LIGHTHOUSE_PORT:-18019}}
ca=${YHOTOS_LIGHTHOUSE_CA:-bench-results/lighthouse-ca.crt}
# Chromium 146+ reads NSS from XDG_DATA_HOME unless a legacy home DB exists.
# Share this isolated trust store with every temporary Lighthouse Chrome profile;
# never disable certificate verification or modify the user's certificate store.
[[ ! -d "$HOME/.pki/nssdb" ]] || { echo 'Use a disposable browser account without a legacy NSS database' >&2; exit 1; }
chrome_data=$(mktemp -d)
trap 'rm -rf "$chrome_data"' EXIT
export XDG_DATA_HOME=$chrome_data
mkdir -p "$chrome_data/pki/nssdb"
certutil -N --empty-password -d "sql:$chrome_data/pki/nssdb"
certutil -A -d "sql:$chrome_data/pki/nssdb" -n yhotos-lighthouse -t 'C,,' -i "$ca"
# Fail before Lighthouse if a required route is absent (a fast 404 is no page).
for key in home album photo search; do
    route=$(jq -er --arg key "$key" '.[$key]' bench-results/routes.json)
    status=$(curl -sS --cacert "$ca" --max-time 15 -o /dev/null -w '%{http_code}' "$base$route")
    [[ "$status" == 200 ]] || { echo "Missing Lighthouse route: $key ($status)" >&2; exit 1; }
done
for preset in mobile desktop; do
    node --input-type=module - "$preset" "$base" <<'JS'
import fs from 'node:fs';
const [preset, base] = process.argv.slice(2);
const config = JSON.parse(fs.readFileSync('lighthouserc.json'));
const routes = JSON.parse(fs.readFileSync('bench-results/routes.json'));
config.ci.collect.url = ['home', 'album', 'photo', 'search'].map(key => base + routes[key]);
if (preset === 'desktop') {
  // The HTTP status audit belongs to SEO; collect it on both profiles so the
  // assertion checks real status codes rather than failing on a missing audit.
  config.ci.collect.settings = {onlyCategories: ['performance', 'seo'], chromeFlags: '--headless --no-sandbox', preset: 'desktop'};
  config.ci.assert.assertions['largest-contentful-paint'][1].maxNumericValue = 999;
}
config.ci.upload.outputDir = `bench-results/lighthouse-${preset}`;
fs.writeFileSync('bench-results/lhci.json', JSON.stringify(config));
JS
    rm -rf .lighthouseci
    lhci autorun --config=bench-results/lhci.json
done
