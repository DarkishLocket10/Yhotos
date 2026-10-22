import { test } from 'node:test';
import assert from 'node:assert/strict';
import { decodeThumbhash } from '../dist/test/thumbhash.js';
// Same upstream reference-encoder vectors as yhotos-core::thumbhash.
const vectors = [
  [[0,8,2,7,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],32,32,1986340293],
  [[63,8,2,7,0,104,137,78,137,120,136,133,136,167,136,136,135,127,4,0,0,0,0,0],32,32,2705403333],
  [[0,8,130,5,0,8,0,0,0,0,0,0,0,0,0,0,0,0,9,190,112,135,128,167,121],32,32,928054725],
  [[226,39,142,19,150,43,144,135,129,136,120,110,144,24,249,118,128,135,136,136,120,136,119],32,19,3157676160],
  [[33,40,142,19,20,43,145,144,119,120,136,144,111,8,249,134,128,135,136,136,135,136,120],19,32,2574598679],
];
for (const [hash,w,h,sum] of vectors) test(`reference pixels ${w}x${h}, ${sum}`, () => {
  const decoded = decodeThumbhash(new Uint8Array(hash)); assert.equal(decoded.w,w); assert.equal(decoded.h,h);
  const actual = decoded.rgba.reduce((n,b) => Math.imul(n ^ b,16777619) >>> 0,2166136261);
  assert.equal(actual,sum);
  for (let i=0;i<hash.length;i++) assert.throws(() => decodeThumbhash(new Uint8Array(hash.slice(0,i))));
});
test('malformed headers fail closed', () => { assert.throws(() => decodeThumbhash(new Uint8Array(64))); assert.throws(() => decodeThumbhash(new Uint8Array(65))); });
