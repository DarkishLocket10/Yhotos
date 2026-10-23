import {test, expect} from '@playwright/test';
import {build} from 'esbuild';
import {execFileSync} from 'node:child_process';
import {mkdtempSync, readFileSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {resolve, join} from 'node:path';

const root = resolve(import.meta.dirname, '../..');
let directory: string, av1: Buffer, island: string, lightbox: string, av1Type: string;
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
  lightbox = (await build({entryPoints: [join(root, 'web/src/lightbox.ts')], bundle: true, write: false, format: 'iife'})).outputFiles[0].text;
});
test.afterAll(() => { if (directory) rmSync(directory, {recursive: true, force: true}); });

for (const control of ['play', 'seek', 'volume', 'scrub']) {
  test(`swiping from a photo then using video ${control} never navigates`, async ({page}) => {
    const ids = ['aaaaaaaaaaaa', 'bbbbbbbbbbbb', 'cccccccccccc'];
    const photos = ids.map((id, index) => ({id, title: `Photo ${index}`, description: '', caps: {download: false, like: false, flag: false}, zoom_width: 128, original: null, downloads: [], details: [],
      ...(index === 1 ? {video: {av1: '/v/fixture/av1.mp4', av1_type: av1Type, h264: '/v/fixture/h264.mp4', duration_ms: 1200, scrub: '/i/fixture/scrub.jpeg', vtt: '/v/fixture/scrub.vtt'}} : {})}));
    const dialog = readFileSync(join(root, 'crates/yhotos-http/templates/default/partials/lightbox.html'), 'utf8').replace(/{%[^%]*%}/g, '');
    await page.route('http://video.test/**', route => {
      const path = new URL(route.request().url()).pathname;
      if (path === '/v/fixture/av1.mp4' || path === '/v/fixture/h264.mp4') {
        const bytes = path.endsWith('/av1.mp4') ? av1 : readFileSync(join(root, 'fixtures/video/h264.mp4'));
        const range = route.request().headers().range?.match(/^bytes=(\d+)-(\d*)$/);
        if (range) {
          const start = Number(range[1]), end = range[2] ? Number(range[2]) : bytes.length - 1;
          return route.fulfill({status: 206, contentType: 'video/mp4', headers: {'accept-ranges': 'bytes', 'content-range': `bytes ${start}-${end}/${bytes.length}`}, body: bytes.subarray(start, end + 1)});
        }
        return route.fulfill({contentType: 'video/mp4', headers: {'accept-ranges': 'bytes'}, body: bytes});
      }
      if (path !== '/') return route.fulfill({status: 404});
      return route.fulfill({contentType: 'text/html', body: `<!doctype html><style>[data-stage]{width:640px;height:360px}video{width:480px;height:300px}</style><main>${ids.map(id => `<a data-photo-link href="/p/${id}"><picture><img src="/i/fixture/poster.jpeg" width="128" height="96"></picture></a>`).join('')}</main>${dialog}<script id="gallery-data" type="application/json">${JSON.stringify({album: 'fixture', page: 1, sort: 'taken', next_page: null, photos})}</script>`});
    });
    await page.goto('http://video.test/');
    await page.addScriptTag({content: lightbox});
    // Broken images still supply the real grid structure and typed source URLs.
    await page.locator('[data-photo-link]').first().dispatchEvent('click', {button: 0});
    const stage = page.locator('[data-stage]');
    const box = (await stage.boundingBox())!;
    await stage.evaluate(element => element.addEventListener('pointerdown', event => { element.setAttribute('data-start-time', String((event as PointerEvent).timeStamp)); }, {once: true}));
    await page.mouse.move(box.x + 500, box.y + 180);
    await page.mouse.down();
    await page.mouse.move(box.x + 400, box.y + 180);
    // A cancelled/untracked pointer must never reuse the photo's start point.
    await stage.dispatchEvent('pointerup', {pointerId: 999, clientX: box.x + 350, clientY: box.y + 180});
    await expect(page).toHaveURL(new RegExp(ids[0]));
    await page.mouse.up();
    await expect(page).toHaveURL(new RegExp(ids[1]));
    const video = page.locator('video');
    await expect.poll(() => video.evaluate(v => (v as HTMLVideoElement).readyState)).toBeGreaterThanOrEqual(2);
    await video.evaluate((element, {control, x, y}) => {
      const video = element as HTMLVideoElement;
      const time = Number(document.querySelector('[data-stage]')!.getAttribute('data-start-time'));
      // Native controls retarget their shadow-tree pointers to <video>. Use a
      // deterministic short gesture, including a large seek/volume drag.
      const pointer = (type: string, x: number, time: number) => {
        const event = new PointerEvent(type, {bubbles: true, pointerId: 7, clientX: x, clientY: y});
        Object.defineProperty(event, 'timeStamp', {value: time}); video.dispatchEvent(event);
      };
      pointer('pointerdown', x + 500, time + 100);
      if (control === 'play') { video.muted = true; void video.play(); }
      if (control === 'seek') video.currentTime = 0.5;
      if (control === 'volume') video.volume = 0.25;
      if (control === 'scrub') { const slider = document.querySelector<HTMLInputElement>('.video-scrub input')!; slider.value = '0.6'; slider.dispatchEvent(new Event('input')); }
      pointer('pointermove', x + 100, time + 150); pointer('pointerup', x + 100, time + 200);
    }, {control, x: box.x, y: box.y + 180});
    await expect(page).toHaveURL(new RegExp(ids[1]));
    await expect(video).toHaveCount(1);
    if (control === 'play') await expect.poll(() => video.evaluate(v => (v as HTMLVideoElement).paused)).toBe(false);
    if (control === 'seek' || control === 'scrub') await expect.poll(() => video.evaluate(v => (v as HTMLVideoElement).currentTime)).toBeCloseTo(control === 'seek' ? 0.5 : 0.6, 1);
    if (control === 'volume') expect(await video.evaluate(v => (v as HTMLVideoElement).volume)).toBe(0.25);
    // Explicit lightbox navigation remains available after using controls.
    await page.locator('[data-next]').click();
    await expect(page).toHaveURL(new RegExp(ids[2]));
  });
}

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
