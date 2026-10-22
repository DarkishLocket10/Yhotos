// Use an existing Playwright install and Chromium; no image downloads are needed.
import fs from 'node:fs/promises';
const {chromium} = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const browser = await chromium.launch({
  executablePath: process.env.CHROME_PATH || undefined,
  headless: true,
  args: ['--no-sandbox'],
});
try {
  const page = await browser.newPage();
  await page.setContent(await fs.readFile(new URL('generate.html', import.meta.url), 'utf8'));
  const images = await page.locator('#result').textContent();
  if (JSON.parse(images).length !== 300) throw new Error('Incomplete fixtures');
  await fs.writeFile(new URL('generated.json', import.meta.url), images);
  console.log('Generated sixty distinct photos with four full-frame sizes and a square crop each.');
} finally {
  await browser.close();
}
