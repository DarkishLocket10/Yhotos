import { readFile } from 'node:fs/promises';
import { gzipSync } from 'node:zlib';
import assert from 'node:assert/strict';
let total = 0;
for (const name of ['lightbox', 'prefetch', 'transitions', 'thumbhash', 'likes']) {
  const source = await readFile(new URL(`../../crates/yhotos-http/assets/js/${name}.js`, import.meta.url));
  const bytes = gzipSync(source).length; total += bytes; console.log(`${name}: ${bytes} bytes gzip`);
}
assert(total < 50_000, `Album JavaScript ${total} exceeds 50 KB compressed`);
console.log(`Album total: ${total} / 50000 bytes gzip`);
