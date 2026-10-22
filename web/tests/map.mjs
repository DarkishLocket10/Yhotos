import {test} from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {build} from 'esbuild';

const compiled = await build({
  entryPoints:[new URL('../src/map.ts',import.meta.url).pathname],bundle:true,write:false,format:'iife',platform:'browser',target:'es2022',
  plugins:[{name:'maplibre-harness',setup(b){
    b.onResolve({filter:/^maplibre-gl$/},()=>({path:'maplibre-gl',namespace:'harness'}));
    b.onLoad({filter:/.*/,namespace:'harness'},()=>({contents:'const {Map,Marker,NavigationControl,setWorkerUrl,setWorkerCount}=globalThis.harness; export {Map,Marker,NavigationControl,setWorkerUrl,setWorkerCount};'}));
  }}],
});
const source=compiled.outputFiles[0].text;
const tick=()=>new Promise(resolve=>setImmediate(resolve));

test('cluster requests, zoom actions, photo links, thumbnails and stale-request cancellation',async()=>{
  class Element {
    dataset={}; listeners={}; textContent=''; children=[];
    setAttribute(name,value){this[name]=value;}
    addEventListener(name,callback){this.listeners[name]=callback;}
    replaceChildren(...children){this.children=children;}
  }
  class Anchor extends Element {}
  const root=new Element();root.dataset={tiles:'/tiles/{z}/{x}/{y}.pbf',worker:'/static/worker.js'};
  const status=new Element();
  const markers=[],requests=[];
  let map,worker,workerCount;
  let count=5,zoom=5;
  class FakeMap {
    handlers={};
    constructor(options){map=this;this.options=options;}
    addControl(){}
    on(event,callback){this.handlers[event]=callback;}
    getBounds(){return {getWest:()=>-80,getSouth:()=>43,getEast:()=>-79,getNorth:()=>44};}
    getZoom(){return zoom;}
    easeTo(value){this.lastZoom=value;}
    remove(){this.removed=true;}
  }
  class FakeMarker {
    constructor({element}){this.element=element;markers.push(this);}
    setLngLat(value){this.coordinates=value;return this;}
    addTo(){return this;}
    remove(){this.removed=true;}
  }
  const window=new Element();
  const context=vm.createContext({
    harness:{Map:FakeMap,Marker:FakeMarker,NavigationControl:class {},setWorkerUrl:value=>{worker=value;},setWorkerCount:value=>{workerCount=value;}},
    document:{getElementById:id=>id==='photo-map'?root:status,createElement:tag=>tag==='a'?new Anchor():new Element()},
    HTMLAnchorElement:Anchor,window,AbortController,
    fetch:async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>({features:[{geometry:{type:'Point',coordinates:[-79.4,43.7]},properties:{count,short_id:'aaaaaaaaaaab',thumb_hash:'a'.repeat(64)}}]})};},
  });
  vm.runInContext(source,context);
  assert.equal(worker,'/static/worker.js');assert.equal(workerCount,1);
  assert.equal(map.options.style.sources.basemap.tiles[0],'/tiles/{z}/{x}/{y}.pbf');
  map.handlers.load();await tick();
  assert.equal(requests[0].url,'/map/tiles.json?bbox=-80,43,-79,44&zoom=5');
  assert.equal(markers[0].element.textContent,'5');
  markers[0].element.listeners.click();
  assert.equal(map.lastZoom.zoom,7);
  count=1;zoom=15;map.handlers.moveend();await tick();
  assert.equal(requests[0].signal.aborted,true);assert.equal(markers[0].removed,true);
  assert.equal(markers[1].element.href,'/p/aaaaaaaaaaab');
  assert.equal(markers[1].element.children[0].src,`/i/${'a'.repeat(64)}/thumb.jpeg`);
  assert.equal(markers[1].element.children[0].width,40);
  window.listeners.pagehide();assert.equal(map.removed,true);assert.equal(requests[1].signal.aborted,true);
});

test('WebGL initialization failure preserves the place fallback',()=>{
  const status={};
  const context=vm.createContext({harness:{Map:class {constructor(){throw new Error('WebGL unavailable');}},setWorkerUrl(){},setWorkerCount(){}},document:{getElementById:id=>id==='photo-map'?{dataset:{tiles:'/tiles/{z}/{x}/{y}',worker:'/worker'}}:status}});
  vm.runInContext(source,context);
  assert.match(status.textContent,/Browse the places below/);
});
