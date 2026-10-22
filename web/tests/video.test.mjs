import {test} from 'node:test';
import assert from 'node:assert/strict';
import {scrubCell} from '../dist/test/lib/video.js';
test('scrub preview matches the first, middle, last and clamped VTT cells',()=>{
  assert.deepEqual(scrubCell(0,10000),{x:-0,y:-0});
  assert.deepEqual(scrubCell(5,10000),{x:-0,y:-800});
  assert.deepEqual(scrubCell(10,10000),{x:-1440,y:-1440});
  assert.deepEqual(scrubCell(-10,10000),{x:-0,y:-0});
});
