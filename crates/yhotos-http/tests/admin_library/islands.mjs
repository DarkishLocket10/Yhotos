import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';
// Compile only exported pure helpers through the existing esbuild installation.
import {transform} from '../../../../web/node_modules/esbuild/lib/main.js';
const source=await readFile(new URL('../../../..//web/src/admin/library.ts',import.meta.url),'utf8');
const {code}=await transform(source,{loader:'ts',format:'cjs',target:'es2022'});
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
