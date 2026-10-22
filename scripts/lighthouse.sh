#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
base=${YHOTOS_BENCH_URL:-http://127.0.0.1:${YHOTOS_BENCH_PORT:-18018}}
# Fail before Lighthouse if a required route is absent (a fast 404 is no page).
for key in home album photo search; do
    route=$(jq -er --arg key "$key" '.[$key]' bench-results/routes.json)
    status=$(curl -sS --max-time 15 -o /dev/null -w '%{http_code}' "$base$route")
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
  config.ci.collect.settings = {onlyCategories: ['performance'], chromeFlags: '--headless --no-sandbox', preset: 'desktop'};
  config.ci.assert.assertions['largest-contentful-paint'][1].maxNumericValue = 999;
}
config.ci.upload.outputDir = `bench-results/lighthouse-${preset}`;
fs.writeFileSync('bench-results/lhci.json', JSON.stringify(config));
JS
    rm -rf .lighthouseci
    lhci autorun --config=bench-results/lhci.json
done
