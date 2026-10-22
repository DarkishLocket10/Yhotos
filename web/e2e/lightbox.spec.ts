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
