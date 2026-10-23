import {test, expect} from '@playwright/test';
import {build} from 'esbuild';
import {execFileSync} from 'node:child_process';
import {mkdtempSync, readFileSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {resolve, join} from 'node:path';

const root = resolve(import.meta.dirname, '../..');
let directory: string, av1: Buffer, island: string, av1Type: string;
test.beforeAll(async () => {
  directory = mkdtempSync(join(tmpdir(), 'yhotos-video-'));
  const tools = process.env.YHOTOS_TEST_VIDEO_BIN_DIR || join(root, 'target/video-tools');
  execFileSync(join(tools, 'ffmpeg'), ['-v', 'error', '-i', join(root, 'fixtures/video/h264.mp4'), '-an', '-c:v', 'libsvtav1', '-preset', '8', '-svtav1-params', 'lp=2:tier=0', '-pix_fmt', 'yuv420p', '-movflags', '+faststart', join(directory, 'av1.mp4')], {stdio: 'pipe'});
  const probe = JSON.parse(execFileSync(join(tools, 'ffprobe'), ['-v', 'error', '-select_streams', 'v:0', '-show_streams', '-of', 'json', join(directory, 'av1.mp4')], {encoding: 'utf8'}));
  expect(probe.streams[0].profile).toBe('Main'); expect(probe.streams[0].pix_fmt).toBe('yuv420p');
  av1Type = `video/mp4; codecs="av01.0.${String(probe.streams[0].level).padStart(2, '0')}M.08"`;
  av1 = readFileSync(join(directory, 'av1.mp4'));
  const bundle = await build({entryPoints: [join(root, 'web/src/lib/video.ts')], bundle: true, write: false, format: 'iife', globalName: 'videoIsland'});
  island = bundle.outputFiles[0].text;
});
test.afterAll(() => { if (directory) rmSync(directory, {recursive: true, force: true}); });

for (const renderer of ['photo template', 'lightbox island']) {
  test(`${renderer} selects and decodes AV1 ahead of H.264`, async ({page}) => {
    await page.route('http://video.test/**', async route => {
      const path = new URL(route.request().url()).pathname;
      if (path === '/v/fixture/av1.mp4') return route.fulfill({contentType: 'video/mp4', body: av1});
      if (path === '/v/fixture/h264.mp4') return route.fulfill({contentType: 'video/mp4', body: readFileSync(join(root, 'fixtures/video/h264.mp4'))});
      if (path !== '/') return route.fulfill({status: 404});
      return route.fulfill({contentType: 'text/html', body: '<!doctype html><title>Video regression</title><main></main>'});
    });
    await page.goto('http://video.test/');
    const capability = await page.evaluate(type => {
      const video = document.createElement('video');
      return {bare: video.canPlayType('video/mp4; codecs="av01"'), complete: video.canPlayType(type)};
    }, av1Type);
    expect(capability.bare).toBe(''); expect(capability.complete).not.toBe('');
    if (renderer === 'photo template') {
      // Exercise the actual template's video element; Rust's publication test
      // checks that its av1_type binding matches ffprobe on the stored rendition.
      const template = readFileSync(join(root, 'crates/yhotos-http/templates/default/photo.html'), 'utf8');
      const element = template.match(/<video\s[\s\S]*?<\/video>/)![0];
      const values: Record<string, string> = {'video.av1': '/v/fixture/av1.mp4', 'video.av1_type': av1Type, 'video.h264': '/v/fixture/h264.mp4', 'image.width': '128', 'image.height': '96'};
      await page.locator('main').evaluate((main, html) => { main.innerHTML = html; }, element.replace(/{{\s*([^}]+?)\s*}}/g, (_, key: string) => values[key] || ''));
    } else {
      await page.addScriptTag({content: island});
      await page.evaluate(type => {
        const api = (window as unknown as {videoIsland: {createVideo: (data: unknown, poster: string, width: number, height: number, signal: AbortSignal) => HTMLElement}}).videoIsland;
        const player = api.createVideo({av1: '/v/fixture/av1.mp4', av1_type: type, h264: '/v/fixture/h264.mp4', duration_ms: 1200, scrub: '/i/fixture/scrub.jpeg', vtt: '/v/fixture/scrub.vtt'}, '/i/fixture/poster.jpeg', 128, 96, new AbortController().signal);
        document.querySelector('main')!.append(player);
      }, av1Type);
    }
    const video = page.locator('video');
    await expect.poll(() => video.evaluate(v => (v as HTMLVideoElement).readyState)).toBeGreaterThanOrEqual(2);
    expect(await video.evaluate(v => (v as HTMLVideoElement).currentSrc)).toBe('http://video.test/v/fixture/av1.mp4');
    await video.evaluate(async v => { const video = v as HTMLVideoElement; video.muted = true; await video.play(); });
    await expect.poll(() => video.evaluate(v => (v as HTMLVideoElement).currentTime)).toBeGreaterThan(0);
    expect(await video.evaluate(v => (v as HTMLVideoElement).videoWidth)).toBe(128);
  });
}
