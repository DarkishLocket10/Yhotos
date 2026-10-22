import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const source = await readFile(new URL('../assets/js/upload.js', import.meta.url), 'utf8');
const album = '019a1b34-4c56-7789-8b90-123456789abc';
class Element {
  dataset = {}; children = []; listeners = {}; value = ''; files = null;
  hidden = false; textContent = ''; disabled = false;
  addEventListener(name, handler) { this.listeners[name] = handler; }
  setAttribute() {}
  append(...nodes) { this.children.push(...nodes); }
  click() {}
}
function island(fetch, saved = new Map()) {
  const root = new Element();
  root.dataset = { chunkBytes: '2', csrf: 'generated-page-token' };
  const elements = Object.fromEntries(['album', 'files', 'drop', 'uploads'].map(id => [id, new Element()]));
  elements.album.value = album;
  root.querySelector = selector => elements[selector.slice(1)];
  const document = { querySelector: () => root, createElement: () => new Element() };
  vm.runInNewContext(source, {
    document, location: { origin: 'https://admin.example.test' }, URL, TextEncoder,
    localStorage: { getItem: key => saved.get(key) ?? null, setItem: (key, value) => saved.set(key, value), removeItem: key => saved.delete(key) },
    fetch, btoa: text => Buffer.from(text, 'binary').toString('base64'),
    AbortController, DOMException, setTimeout, clearTimeout,
  });
  return { elements, saved, choose(files) { elements.files.files = files; elements.files.listeners.change(); } };
}
async function until(condition) {
  for (let n = 0; n < 500; n++) {
    if (condition()) return;
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  assert.fail('Upload island did not settle');
}
const photo = '/admin/api/photos/abcdefghijkl';
function created(id) { return new Response(null, { status: 201, headers: { Location: `/admin/upload/${id}` } }); }
function patched(offset, complete) {
  return new Response(null, { status: 204, headers: { 'Upload-Offset': String(offset), ...(complete ? { 'Upload-Result': 'created', Location: photo } : {}) } });
}
function ready() { return Response.json({ status: 'ready' }); }
function metadata(destination = album) { return `album ${Buffer.from(destination).toString('base64')}`; }
function resumed(offset, length, destination = album) {
  return new Response(null, { headers: { 'Upload-Offset': String(offset), 'Upload-Length': String(length), 'Upload-Metadata': metadata(destination) } });
}

test('island resumes a saved upload, chunks and polls ready', async () => {
  const file = new File(['abcdef'], '雪.jpg', { lastModified: 123 });
  const key = `yhotos-upload:${JSON.stringify([file.name, file.size, file.lastModified])}`;
  const url = '/admin/upload/00000000-0000-4000-8000-000000000001';
  const saved = new Map([[key, url]]); const calls = [];
  const app = island(async (path, options) => {
    calls.push([path, options]);
    if (options.method === 'HEAD') return resumed(2, 6);
    if (options.method === 'GET') return ready();
    assert.equal(options.method, 'PATCH');
    assert.equal(options.headers['X-CSRF-Token'], 'generated-page-token');
    assert.equal(options.credentials, 'same-origin');
    const next = Number(options.headers['Upload-Offset']) + options.body.size;
    return patched(next, next === file.size);
  }, saved);
  app.choose([file]);
  await until(() => app.elements.uploads.children[0]?.children[1].textContent === 'Ready');
  assert.deepEqual(calls.map(([, options]) => options.method), ['HEAD', 'PATCH', 'PATCH', 'GET']);
  assert.equal(await calls[1][1].body.text(), 'cd');
  assert.equal(await calls[2][1].body.text(), 'ef');
  assert.equal(saved.size, 0);
});

test('resuming into a restricted album creates a new upload before sending any bytes', async () => {
  const file = new File(['abcd'], 'private.jpg', { lastModified: 123 });
  const key = `yhotos-upload:${JSON.stringify([file.name, file.size, file.lastModified])}`;
  const publicUpload = '/admin/upload/00000000-0000-4000-8000-000000000001';
  const privateUpload = '/admin/upload/00000000-0000-4000-8000-000000000002';
  const restrictedAlbum = '019a1b34-4c56-7789-8b90-123456789def';
  // Missing, malformed or duplicate destination metadata must also fail closed.
  for (const savedMetadata of [metadata(), null, 'album @@@', `${metadata(restrictedAlbum)},${metadata(restrictedAlbum)}`]) {
    const saved = new Map([[key, publicUpload]]); const calls = [];
    const app = island(async (path, options) => {
      calls.push([path, options.method]);
      if (options.method === 'HEAD') {
        assert.equal(path, publicUpload);
        return new Response(null, { headers: { 'Upload-Offset': '2', 'Upload-Length': '4', ...(savedMetadata ? { 'Upload-Metadata': savedMetadata } : {}) } });
      }
      if (options.method === 'POST') {
        assert.equal(path, '/admin/upload');
        assert.ok(options.headers['Upload-Metadata'].includes(metadata(restrictedAlbum)));
        return created(privateUpload.split('/').at(-1));
      }
      if (options.method === 'GET') return ready();
      assert.equal(path, privateUpload);
      const offset = Number(options.headers['Upload-Offset']);
      assert.equal(await options.body.text(), offset === 0 ? 'ab' : 'cd');
      return patched(offset + options.body.size, offset + options.body.size === file.size);
    }, saved);
    app.elements.album.value = restrictedAlbum;
    app.choose([file]);
    await until(() => app.elements.uploads.children[0]?.children[1].textContent === 'Ready');
    assert.deepEqual(calls.map(([, method]) => method), ['HEAD', 'POST', 'PATCH', 'PATCH', 'GET']);
    assert.equal(saved.size, 0);
  }
});

test('retrying a temporary completion failure uses finish without retransmitting bytes', async () => {
  const file = new File(['ab'], 'photo.jpg', { lastModified: 123 });
  const calls = [];
  const url = '/admin/upload/00000000-0000-4000-8000-000000000001';
  const app = island(async (path, options) => {
    calls.push([path, options.method]);
    if (options.method === 'PATCH') return new Response('Upload temporarily unavailable', { status: 503, headers: { 'Retry-After': '30' } });
    if (options.method === 'HEAD') return resumed(file.size, file.size);
    if (options.method === 'GET') return ready();
    if (path.endsWith('/finish')) {
      assert.equal(path, `${url}/finish`);
      assert.equal(options.method, 'POST');
      assert.equal(options.body, undefined);
      return patched(file.size, true);
    }
    return created(url.split('/').at(-1));
  });
  app.choose([file]);
  const row = app.elements.uploads.children[0];
  await until(() => row.children[3].hidden === false);
  assert.equal(app.saved.size, 1);
  row.children[3].listeners.click();
  await until(() => row.children[1].textContent === 'Ready');
  assert.equal(row.children[2].value, file.size);
  assert.deepEqual(calls.map(([, method]) => method), ['POST', 'PATCH', 'HEAD', 'POST', 'GET']);
  assert.equal(app.saved.size, 0);
});

test('island limits uploads to four and rejects stored cross-origin URLs', async () => {
  const files = Array.from({ length: 5 }, (_, n) => new File(['ab'], `original-${n}.jpg`, { lastModified: n }));
  const saved = new Map([[`yhotos-upload:${JSON.stringify([files[0].name, files[0].size, files[0].lastModified])}`, 'https://attacker.invalid/admin/upload/00000000-0000-4000-8000-000000000001']]);
  let createdCount = 0; let inFlight = 0; let maximum = 0; let posts = 0;
  const release = [];
  const app = island(async (path, options) => {
    assert.ok(path.startsWith('/admin/'));
    if (options.method === 'POST') {
      posts++;
      const metadata = options.headers['Upload-Metadata'];
      assert.ok(metadata.includes('album '));
      return created(`00000000-0000-4000-8000-${String(++createdCount).padStart(12, '0')}`);
    }
    if (options.method === 'GET') return ready();
    assert.equal(options.method, 'PATCH');
    maximum = Math.max(maximum, ++inFlight);
    await new Promise(resolve => release.push(resolve));
    inFlight--;
    return patched(2, true);
  }, saved);
  app.choose(files);
  await until(() => release.length === 4);
  assert.equal(posts, 4); assert.equal(maximum, 4);
  release.splice(0).forEach(resolve => resolve());
  await until(() => release.length === 1);
  release.splice(0).forEach(resolve => resolve());
  await until(() => app.elements.uploads.children.every(row => row.children[1].textContent === 'Ready'));
  assert.equal(posts, 5); assert.equal(maximum, 4); assert.equal(saved.size, 0);
});
