// Run against pages_fixture with existing Playwright and Chromium installations.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
const {chromium} = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const browser = await chromium.launch({
  executablePath: process.env.CHROME_PATH || undefined,
  headless: true,
  args: ['--no-sandbox'],
});
const results = [];
try {
  for (const [path, selector, count] of [
    ['/a/mobile-fixture', '.album-card img', 1],
    ['/a/single-fixture', '.cell img', 1],
    ['/a/gallery-fixture', '.cell img', 60],
  ]) {
    const context = await browser.newContext({
      viewport: {width: 390, height: 844}, deviceScaleFactor: 2,
      javaScriptEnabled: false,
    });
    try {
      const page = await context.newPage();
      const transfers = [];
      const errors = [];
      page.on('response', response => {
        if (new URL(response.url()).pathname.startsWith('/i/')) {
          transfers.push({url: response.url(), status: response.status()});
        }
      });
      page.on('console', message => {
        if (message.type() === 'error') errors.push(message.text());
      });
      await page.goto(`http://127.0.0.1:18011${path}`, {waitUntil: 'networkidle'});
      assert.equal(await page.locator(selector).count(), count);
      const images = await page.locator(selector).evaluateAll(images => images.map(image => ({
        src: image.currentSrc, loading: image.loading, naturalWidth: image.naturalWidth,
        slot: image.getBoundingClientRect().width,
        width: image.getAttribute('width'), height: image.getAttribute('height'),
      })));
      assert.ok(transfers.length > 0, 'No image requests observed');
      for (const transfer of transfers) {
        assert.equal(transfer.status, 200);
        assert.match(transfer.url, /\/(thumb|small)\.jpeg$/, 'Fetched more than 640w or a square crop');
      }
      const eager = images.filter(image => image.loading === 'eager');
      assert.ok(eager.length > 0);
      assert.ok(eager.every(image => image.naturalWidth > 0 && image.width && image.height));
      if (count === 1) {
        // Keep the real full-width slot: test the source cap at DPR 2, rather
        // than understating sizes or shrinking the card to avoid the regression.
        assert.ok(eager[0].slot > 320);
        assert.ok(eager[0].src.endsWith('/small.jpeg'));
      }
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
      assert.deepEqual(errors, []);
      results.push({path, viewport: '390x844', dpr: 2, javascript: false, eager, transfers});
    } finally {
      await context.close();
    }
  }
  await fs.mkdir('deploy/dev/reports', {recursive: true});
  await fs.writeFile('deploy/dev/reports/011-mobile-network.json', JSON.stringify(results, null, 2));
  console.log(JSON.stringify(results, null, 2));
} finally {
  await browser.close();
}
