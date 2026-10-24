import {test} from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
// Bundle the real island so tests exercise its bound handlers and shared tus client.
import {build} from '../../../../web/node_modules/esbuild/lib/main.js';
const result=await build({entryPoints:[new URL('../../../../web/src/admin/library.ts',import.meta.url).pathname],bundle:true,write:false,format:'cjs',target:'es2022'});
const code=result.outputFiles[0].text;
const context={exports:{},module:{exports:{}},document:{querySelector:()=>null,querySelectorAll:()=>[]}};
vm.runInNewContext(code,context);
const helpers=context.module.exports;
test('shift-click includes both ends; select-all values retain visual order',()=>{
  const boxes=['a','b','c','d'].map(value=>({value,checked:false}));
  helpers.rangeSelection(boxes,3,1,true);
  assert.deepEqual(Array.from(helpers.selectedIds(boxes)),['b','c','d']);
  helpers.rangeSelection(boxes,1,2,false);
  assert.deepEqual(Array.from(helpers.selectedIds(boxes)),['d']);
});
test('keyboard photo reorder preserves identity and handles both edges',()=>{
  assert.deepEqual(Array.from(helpers.keyboardMove(['a','b','c'],1,false)),['b','a','c']);
  assert.deepEqual(Array.from(helpers.keyboardMove(['a','b','c'],1,true)),['a','c','b']);
  assert.deepEqual(Array.from(helpers.keyboardMove(['a','b'],0,false)),['a','b']);
  assert.deepEqual(Array.from(helpers.keyboardMove(['a','b'],1,true)),['a','b']);
});

test('orders above 1,000 photos save in bounded subset permutations',()=>{
  const original=Array.from({length:2101},(_,i)=>String(i));
  for (const desired of [[...original].reverse(),[...original.slice(1),original[0]]]) {
    const current=[...original];
    for (const batch of helpers.reorderBatches(original,desired)) {
      assert.ok(batch.length>0 && batch.length<=1000);
      assert.equal(new Set(batch).size,batch.length);
      const selected=new Set(batch),slots=current.flatMap((id,i)=>selected.has(id)?[i]:[]);
      slots.forEach((slot,i)=>{current[slot]=batch[i];});
    }
    assert.deepEqual(current,desired);
  }
});

class Element {
  dataset = {}; listeners = {}; value = ''; checked = false; disabled = false; textContent = '';
  addEventListener(name, handler) { this.listeners[name] = handler; }
  focus() {}
}
function island(fetch, saved = new Map()) {
  const elements = Object.fromEntries(['replace-start', 'replace-file', 'replace-progress', 'admin-status', 'selection-count'].map(id => [id, new Element()]));
  elements['replace-file'].dataset = {album: '019a1b34-4c56-7789-8b90-123456789abc', photo: '019a1b34-4c56-7789-8b90-123456789def', chunkBytes: '2'};
  const cards = ['a', 'b', 'c'].map(value => {
    const card = new Element(); card.dataset.id = value;
    card.box = new Element(); card.box.value = value;
    card.querySelector = () => card.box;
    return card;
  });
  const container = {append(card) { cards.splice(cards.indexOf(card), 1); cards.push(card); }};
  const root = {dataset: {manual: 'true', adminPage: 'album'}};
  const document = {
    querySelector: selector => selector === '[data-admin-page]' ? root : selector === '.admin-cards' ? container : selector === 'meta[name="csrf-token"]' ? {content: 'page-csrf'} : elements[selector.slice(1)] ?? null,
    querySelectorAll: selector => selector === '.photo-select' ? cards.map(c => c.box) : selector === '.admin-card[data-id]' ? [...cards] : [],
  };
  let reloads = 0;
  vm.runInNewContext(code, {
    module: {exports: {}}, exports: {}, document, fetch, URL, TextEncoder,
    location: {origin: 'https://admin.example.test', reload: () => { reloads++; }},
    localStorage: {getItem: key => saved.get(key) ?? null, setItem: (key, value) => saved.set(key, value), removeItem: key => saved.delete(key)},
    btoa: text => Buffer.from(text, 'binary').toString('base64'),
  });
  return {elements, cards, saved, get reloads() { return reloads; }, replace(file) { elements['replace-file'].files = [file]; return elements['replace-start'].listeners.click(); }};
}
test('bound shift-click handlers use visual positions after keyboard reorder', () => {
  for (const anchorBeforeMove of [false, true]) {
    const app = island();
    const [a, b, c] = app.cards;
    a.box.checked = true;
    if (anchorBeforeMove) a.box.listeners.click({shiftKey: false});
    for (let n = 0; n < 2; n++) c.listeners.keydown({altKey: true, key: 'ArrowUp', preventDefault() {}});
    assert.deepEqual(app.cards.map(card => card.dataset.id), ['c', 'a', 'b']);
    if (!anchorBeforeMove) a.box.listeners.click({shiftKey: false});
    b.box.checked = true; b.box.listeners.click({shiftKey: true});
    assert.deepEqual(app.cards.filter(card => card.box.checked).map(card => card.dataset.id), ['a', 'b']);
    assert.equal(app.elements['selection-count'].textContent, '2 photos selected');
  }
});
const uploadUrl = '/admin/upload/00000000-0000-4000-8000-000000000001';
const photoUrl = '/admin/api/photos/abcdefghijkl';
function created() { return new Response(null, {status: 201, headers: {Location: uploadUrl}}); }
function patched(offset, complete = false) {
  return new Response(null, {status: 204, headers: {'Upload-Offset': String(offset), ...(complete ? {'Upload-Result': 'created', Location: photoUrl} : {})}});
}
function resumed(app, file, offset, replace = app.elements['replace-file'].dataset.photo) {
  return new Response(null, {headers: {
    'Upload-Length': String(file.size), 'Upload-Offset': String(offset),
    'Upload-Metadata': `album ${Buffer.from(app.elements['replace-file'].dataset.album).toString('base64')}`,
    ...(replace ? {'Upload-Replace-Photo': replace} : {}),
  }});
}
for (const reload of [false, true]) test(`replacement resumes an interrupted chunk using HEAD${reload ? ' after reload' : ''}`, async () => {
  const file = new File(['abcdef'], 'replacement.jpg', {lastModified: 123});
  const calls = []; let failed = false; let app;
  const fetch = async (path, options) => {
    calls.push([path, options]);
    assert.equal(options.credentials, 'same-origin');
    assert.equal(options.redirect, 'error');
    assert.equal(options.headers['X-CSRF-Token'], 'page-csrf');
    if (options.method === 'POST') {
      assert.equal(path, '/admin/upload');
      assert.equal(options.headers['Upload-Replace-Photo'], app.elements['replace-file'].dataset.photo);
      return created();
    }
    assert.equal(path, uploadUrl);
    if (options.method === 'HEAD') return resumed(app, file, 2);
    const offset = Number(options.headers['Upload-Offset']);
    if (offset === 2 && !failed) { failed = true; throw new TypeError('Network interrupted'); }
    return patched(offset + options.body.size, offset + options.body.size === file.size);
  };
  app = island(fetch); await app.replace(file);
  assert.equal(app.reloads, 0); assert.equal(app.saved.size, 1);
  if (reload) app = island(fetch, app.saved);
  await app.replace(file);
  assert.equal(app.reloads, 1); assert.equal(app.saved.size, 0);
  assert.deepEqual(calls.map(([, o]) => o.method), ['POST', 'PATCH', 'PATCH', 'HEAD', 'PATCH', 'PATCH']);
  assert.equal(await calls[4][1].body.text(), 'cd');
  assert.equal(await calls[5][1].body.text(), 'ef');
});
for (const lostResponse of [false, true]) test(`replacement retries a completed transfer through finish (${lostResponse ? 'lost response' : 'recoverable failure'})`, async () => {
  const file = new File(['ab'], 'replacement.jpg', {lastModified: 123});
  const calls = []; let app;
  app = island(async (path, options) => {
    calls.push([path, options.method]);
    if (options.method === 'PATCH') {
      if (lostResponse) throw new TypeError('Response lost');
      return new Response(null, {status: 503});
    }
    if (options.method === 'HEAD') return resumed(app, file, file.size);
    if (path.endsWith('/finish')) { assert.equal(options.body, undefined); return patched(file.size, true); }
    return created();
  });
  await app.replace(file); await app.replace(file);
  assert.deepEqual(calls, [['/admin/upload', 'POST'], [uploadUrl, 'PATCH'], [uploadUrl, 'HEAD'], [`${uploadUrl}/finish`, 'POST']]);
  assert.equal(app.reloads, 1); assert.equal(app.saved.size, 0);
});
test('replacement retains the live upload URL when browser storage is unavailable', async () => {
  const file = new File(['ab'], 'replacement.jpg', {lastModified: 123});
  const saved = {get() { throw new Error('Storage blocked'); }, set() { throw new Error('Storage blocked'); }, delete() { throw new Error('Storage blocked'); }};
  const methods = []; let app;
  app = island(async (path, options) => {
    methods.push(options.method);
    if (options.method === 'HEAD') return resumed(app, file, 0);
    if (options.method === 'POST') return created();
    if (methods.length === 2) throw new TypeError('Interrupted');
    return patched(file.size, true);
  }, saved);
  await app.replace(file); await app.replace(file);
  assert.deepEqual(methods, ['POST', 'PATCH', 'HEAD', 'PATCH']);
  assert.equal(app.reloads, 1);
});
test('replacement fingerprints isolate target photos and reject untrusted saved URLs or bindings', async () => {
  const file = new File(['ab'], 'replacement.jpg', {lastModified: 123});
  for (const savedUrl of [uploadUrl, 'https://attacker.invalid' + uploadUrl]) {
    const calls = []; let app;
    app = island(async (path, options) => {
      calls.push([path, options.method]);
      if (options.method === 'HEAD') return resumed(app, file, 1, null);
      if (options.method === 'POST') return created();
      return patched(file.size, true);
    });
    const key = `yhotos-replace:${JSON.stringify([app.elements['replace-file'].dataset.photo, file.name, file.size, file.lastModified])}`;
    app.saved.set(key, savedUrl);
    await app.replace(file);
    assert.equal(app.reloads, 1);
    assert.deepEqual(calls.map(([, method]) => method), savedUrl === uploadUrl ? ['HEAD', 'POST', 'PATCH'] : ['POST', 'PATCH']);
    assert.ok(calls.every(([path]) => path.startsWith('/admin/')));
    // Same file for a different photo cannot reuse the other photo's transfer.
    app.saved.set(key, uploadUrl);
    app.elements['replace-file'].dataset.photo = '019a1b34-4c56-7789-8b90-123456789aaa';
    calls.length = 0;
    await app.replace(file);
    assert.deepEqual(calls.map(([, method]) => method), ['POST', 'PATCH']);
  }
});
