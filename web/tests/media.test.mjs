import { test } from 'node:test';
import assert from 'node:assert/strict';

test('media URLs accept only same-origin gallery paths and reject traversal', async () => {
  const { safeMediaUrl, zoomSourceSet } = await import('../dist/test/lib/media.js');
  globalThis.location = { origin: 'https://photos.test' };
  for (const value of ['javascript:alert(1)', 'data:image/png;base64,abc', '//evil.test/i/photo', 'https://evil.test/i/photo', '/i/../login', '/i/%2e%2e/login', '/i/a/../b', '/i/a/%2E%2E/b', '/i/..\\login', '/o/%2f../login', '/login', '/image/photo']) {
    assert.equal(safeMediaUrl(value), null, value);
  }
  assert.equal(safeMediaUrl('https://photos.test/i/hash/large.jpeg?download=1#fragment'), '/i/hash/large.jpeg?download=1');
  assert.equal(safeMediaUrl('/o/hash/photo.jpeg'), '/o/hash/photo.jpeg');
  assert.equal(safeMediaUrl('/v/hash/h264.mp4'), '/v/hash/h264.mp4');
  assert.equal(zoomSourceSet('javascript:alert(1) 640w, //evil.test/i/a 1280w, /i/hash/large.jpeg 2048w', 600), '/i/hash/large.jpeg 2048w');
  assert.equal(zoomSourceSet('data:image/png;base64,abc 2048w', 600), null);
});

test('zoom chooses by CSS display width times scale times DPR and caps at the largest derivative', async () => {
  const { zoomSourceSet } = await import('../dist/test/lib/media.js');
  globalThis.location = { origin: 'https://photos.test' };
  const sources = '/i/a/xlarge.jpeg 4096w, /i/b/small.jpeg 640w, /i/c/large.jpeg 2048w, /i/d/medium.jpeg 1280w';
  assert.equal(zoomSourceSet(sources, 600), '/i/b/small.jpeg 640w');
  assert.equal(zoomSourceSet(sources, 600 * 2 * 2), '/i/a/xlarge.jpeg 4096w');
  assert.equal(zoomSourceSet(sources, 10_000), '/i/a/xlarge.jpeg 4096w');
});

test('zoom before initial decode completes displays the zoom derivative even if decodes finish out of order', async () => {
  const { DecodedMedia, zoomSourceSet } = await import('../dist/test/lib/media.js');
  globalThis.location = { origin: 'https://photos.test' };
  const state = new DecodedMedia(), displayed = [];
  const sources = '/i/hash/medium.jpeg 1280w, /i/hash/xlarge.jpeg 4096w';
  let finishInitial, finishZoom;
  const initial = state.show(zoomSourceSet(sources, 600 * 1 * 2), () => new Promise(resolve => { finishInitial = resolve; }), () => displayed.push('medium'));
  // No original URL or download capability is needed for this derivative upgrade.
  const zoom = state.show(zoomSourceSet(sources, 600 * 2 * 2), () => new Promise(resolve => { finishZoom = resolve; }), () => displayed.push('xlarge'));
  assert.deepEqual(displayed, []);
  finishZoom(); await zoom;
  assert.deepEqual(displayed, ['xlarge']);
  finishInitial(); await initial;
  assert.deepEqual(displayed, ['xlarge']);
});

test('delayed initial decode cannot swap while a zoom upgrade is pending, and navigation cancels stale swaps', async () => {
  const { DecodedMedia } = await import('../dist/test/lib/media.js');
  const state = new DecodedMedia(), displayed = [];
  let finishInitial, finishZoom;
  const initial = state.show('medium', () => new Promise(resolve => { finishInitial = resolve; }), () => displayed.push('medium'));
  const zoom = state.show('xlarge', () => new Promise(resolve => { finishZoom = resolve; }), () => displayed.push('xlarge'));
  finishInitial(); await initial; assert.deepEqual(displayed, []);
  finishZoom(); await zoom; assert.deepEqual(displayed, ['xlarge']);
  let finishPrevious;
  const previous = state.show('original', () => new Promise(resolve => { finishPrevious = resolve; }), () => displayed.push('stale'));
  state.reset(); finishPrevious(); await previous;
  assert.deepEqual(displayed, ['xlarge']);
  await assert.rejects(state.show('failed', () => Promise.reject(new Error('decode failed')), () => displayed.push('failed')));
  await state.show('failed', () => Promise.resolve(), () => displayed.push('retry'));
  assert.deepEqual(displayed, ['xlarge', 'retry']);
});

test('zoom retaining the same selected derivative keeps the pending initial upgrade', async () => {
  const { DecodedMedia, zoomSourceSet } = await import('../dist/test/lib/media.js');
  globalThis.location = { origin: 'https://photos.test' };
  const state = new DecodedMedia(), displayed = [];
  const sources = '/i/hash/xlarge.jpeg 4096w';
  let finish;
  const pending = state.show(zoomSourceSet(sources, 800), () => new Promise(resolve => { finish = resolve; }), () => displayed.push('decoded'));
  await state.show(zoomSourceSet(sources, 1600), () => assert.fail('same candidate should reuse its decode'), () => assert.fail('already pending'));
  finish(); await pending;
  assert.deepEqual(displayed, ['decoded']);
});

test('zoom width excludes letterboxing for portrait and landscape photos', async () => {
  const { containedWidth } = await import('../dist/test/lib/media.js');
  // Portrait 4000x6000 photo in a wide 2000x1000 box is drawn 666.67 px wide, not 2000.
  assert.ok(Math.abs(containedWidth(2000, 1000, 4000, 6000) - 2000 / 3) < 1e-9);
  // Landscape photo limited by width keeps the box width.
  assert.equal(containedWidth(1000, 1000, 6000, 4000), 1000);
  // Missing or zero sizes fall back safely and never return zero.
  assert.equal(containedWidth(800, 0, 4000, 6000), 800);
  assert.equal(containedWidth(0, 0, 0, 0), 1);
  assert.equal(containedWidth(800, 600, 0, 0), 800);
});

test('photo-state refreshes that overlap a mutation are stale', async () => {
  const { staleRefresh } = await import('../dist/test/lib/media.js');
  const pending = new Set(), settled = new Map();
  assert.equal(staleRefresh(pending, settled, 'a', 10), false);
  pending.add('a');
  // GET issued, then the like is still in flight: the GET may read old state.
  assert.equal(staleRefresh(pending, settled, 'a', 10), true);
  pending.delete('a'); settled.set('a', 20);
  // Like settled after the GET was requested (responses reordered): stale.
  assert.equal(staleRefresh(pending, settled, 'a', 10), true);
  // A GET requested after the like settled sees the saved state.
  assert.equal(staleRefresh(pending, settled, 'a', 25), false);
  // Other photos are unaffected.
  assert.equal(staleRefresh(pending, settled, 'b', 10), false);
});
