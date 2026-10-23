import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PageCursor, mergePhotos, swipeDirection, parseGallery } from '../dist/test/lib/gallery.js';
import { PrefetchCache } from '../dist/test/lib/cache.js';
test('cursor serializes requests, retries failures and terminates using the server cursor', () => {
  const cursor = new PageCursor(3);
  assert.equal(cursor.begin(), 3); assert.equal(cursor.begin(), null);
  cursor.fail(); assert.equal(cursor.begin(), 3);
  cursor.finish(4); assert.equal(cursor.begin(), 4);
  cursor.finish(null); assert.equal(cursor.begin(), null);
});
test('page merge preserves server order and removes duplicate ids across pages', () => {
  const photos = [{ id: 'a' }]; mergePhotos(photos, [{ id: 'a' }, { id: 'b' }, { id: 'b' }, { id: 'c' }]);
  assert.deepEqual(photos.map(p => p.id), ['a', 'b', 'c']);
});
test('invalid or backwards server cursors are rejected', () => {
  const base = { album: 'album', page: 2, sort: 'taken', photos: [], next_page: 3 };
  assert.equal(parseGallery(base), base);
  for (const next_page of [0, 1, 2, -1, 2.5, '3', undefined]) assert.throws(() => parseGallery({ ...base, next_page }));
});
test('swipe requires distance, horizontal intent and velocity', () => {
  assert.equal(swipeDirection(-100, 10, 200), 1); assert.equal(swipeDirection(100, 10, 200), -1);
  for (const [dx, dy, ms] of [[30, 0, 20], [100, 90, 100], [100, 0, 600], [100, 0, 0], [100, 0, 1000]]) assert.equal(swipeDirection(dx, dy, ms), 0);
});
test('prefetch cache evicts least recently used entries, expires and stays at ten', () => {
  const cache = new PrefetchCache(10, 100);
  for (let i = 0; i < 10; i++) cache.put(String(i), i, 0);
  assert.equal(cache.get('0', 1), 0); cache.put('10', 10, 1);
  assert.equal(cache.get('1', 2), undefined); assert.equal(cache.get('0', 2), 0);
  cache.put('0', 20, 2); assert.equal(cache.get('0', 3), 20);
  assert.equal(cache.get('0', 102), undefined); cache.clear(); assert.equal(cache.get('10', 2), undefined);
});

test('grid metadata reuses the matching HTML image and rejects missing images', async () => {
  const { hydrateGrid } = await import('../dist/test/lib/gallery.js');
  const image = { src: '/i/digest/large.jpeg', width: 1500, height: 1000, jpeg:{all:'/i/digest/large.jpeg 2048w'}, avif:{all:''}, jxl:{all:''}, hdr_jpeg:{all:''}, hdr_avif:{all:''}, hdr_jxl:{all:''} };
  const photo = { id:'aaaaaaaaaaaa', title:'Photo', caps:{}, downloads:[], original:null };
  const data = {album:'id',page:1,sort:'taken',next_page:null,photos:[photo]};
  const gallery = hydrateGrid(data, new Map([[photo.id,image]]));
  assert.equal(gallery.photos[0].image,image);
  assert.throws(()=>hydrateGrid(data,new Map()));
});
