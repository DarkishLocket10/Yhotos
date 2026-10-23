import { test, expect } from '@playwright/test';
const album = '/a/gallery-fixture';
test('grid opens instantly, navigates, traps focus and restores URL on Esc', async ({ page }) => {
  const errors: string[] = []; page.on('pageerror', e => errors.push(e.message));
  await page.goto(album); const initial = page.url();
  await page.locator('[data-photo-link]').first().focus(); await page.keyboard.press('Enter');
  const viewer = page.getByRole('dialog'); await expect(viewer).toBeVisible(); await expect(page).toHaveURL(/\/p\//);
  const first = page.url(); await page.keyboard.press('ArrowRight'); await expect(page).not.toHaveURL(first);
  await page.getByRole('button', { name: 'Close photo viewer' }).focus(); await page.keyboard.press('Shift+Tab');
  expect(await viewer.locator(':focus').count()).toBe(1);
  await page.keyboard.press('Escape'); await expect(viewer).not.toBeVisible(); await expect(page).toHaveURL(initial);
  await expect(page.locator('[data-photo-link]').first()).toBeFocused();
  await page.goForward(); await expect(viewer).toBeVisible(); await page.keyboard.press('Escape');
  expect(errors).toEqual([]);
});
test('without JavaScript the same link opens a complete photo page', async ({ browser }) => {
  const context = await browser.newContext({ javaScriptEnabled: false }); const page = await context.newPage();
  await page.goto(`${process.env.YHOTOS_BASE_URL || 'http://127.0.0.1:18011'}${album}`);
  await page.locator('[data-photo-link]').first().click(); await expect(page).toHaveURL(/\/p\//);
  await expect(page.locator('main.photo-page picture img')).toBeVisible(); await context.close();
});

test.describe('zoom media selection', () => {
  test.use({ deviceScaleFactor: 2 });
  test('view-only zoom selects xlarge by display size and keeps it through delayed initial decode', async ({ page }) => {
    await page.setViewportSize({ width: 800, height: 700 });
    await page.addInitScript(() => {
      // Hold every lightbox upgrade decode so the test can resolve them out of order.
      const decode = HTMLImageElement.prototype.decode;
      const pending: { resolve: () => void; image: HTMLImageElement }[] = [];
      Object.assign(window, { pendingDecodes: pending });
      HTMLImageElement.prototype.decode = function () {
        if (!this.closest('.lightbox-upgrade')) return decode.call(this);
        return new Promise<void>((resolve, reject) => pending.push({ image: this, resolve: () => { void decode.call(this).then(resolve, reject); } }));
      };
    });
    await page.goto(album);
    const link = page.locator('[data-photo-link]').first();
    await link.evaluate(anchor => {
      const json = document.querySelector('#gallery-data')!;
      const data = JSON.parse(json.textContent!);
      for (const photo of data.photos) { photo.caps = { download: false, like: false, flag: false }; photo.original = null; photo.downloads = []; photo.zoom_width = 4096; }
      json.textContent = JSON.stringify(data);
      // Use a distinct, real fixture URL with an xlarge width descriptor.
      const img = anchor.querySelector('img')!;
      img.srcset += `, ${img.src}?xlarge=1 4096w`;
      document.dispatchEvent(new Event('yhotos:navigate'));
    });
    await link.click();
    const viewer = page.getByRole('dialog');
    await expect(viewer).toBeVisible();
    await expect(viewer.locator('[data-download]')).toBeHidden();
    await page.getByRole('button', { name: 'Zoom photo', exact: true }).click();
    const pending = () => page.evaluate(() => (window as unknown as { pendingDecodes: unknown[] }).pendingDecodes.length);
    await expect.poll(pending).toBe(2);
    const last = viewer.locator('.lightbox-upgrade').last().locator('img');
    await expect(last).toHaveAttribute('srcset', /xlarge=1 4096w$/);
    // The zoomed decode finishes first; the old viewport decode finishes last.
    await page.evaluate(() => {
      const items = (window as unknown as { pendingDecodes: { resolve: () => void }[] }).pendingDecodes;
      items[1].resolve();
    });
    const displayed = viewer.locator('[data-picture] img');
    await expect(displayed).toHaveAttribute('srcset', /xlarge=1 4096w$/);
    await page.evaluate(() => (window as unknown as { pendingDecodes: { resolve: () => void }[] }).pendingDecodes[0].resolve());
    await expect(viewer.locator('.lightbox-upgrade')).toHaveCount(0);
    await expect(displayed).toHaveAttribute('srcset', /xlarge=1 4096w$/);
    await expect(displayed).toHaveCSS('transform', /matrix\(2,/);
    expect(await page.evaluate(() => performance.getEntriesByType('resource').some(e => new URL(e.name).pathname.startsWith('/o/')))).toBe(false);
  });

});
