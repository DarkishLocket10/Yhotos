import {test} from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {build} from 'esbuild';

const compiled = await build({
  entryPoints:[new URL('../src/map.ts',import.meta.url).pathname],bundle:true,write:false,format:'iife',platform:'browser',target:'es2022',
  plugins:[{name:'maplibre-harness',setup(b){
    b.onResolve({filter:/^maplibre-gl$/},()=>({path:'maplibre-gl',namespace:'harness'}));
    b.onLoad({filter:/.*/,namespace:'harness'},()=>({contents:'const {Map,Marker,Popup,NavigationControl,setWorkerUrl,setWorkerCount}=globalThis.harness; export {Map,Marker,Popup,NavigationControl,setWorkerUrl,setWorkerCount};'}));
  }}],
});
const source=compiled.outputFiles[0].text;
const tick=()=>new Promise(resolve=>setImmediate(resolve));

function harness() {
  class Element {
    dataset={}; listeners={}; textContent=''; children=[];
    setAttribute(name,value){this[name]=value;}
    addEventListener(name,callback){this.listeners[name]=callback;}
    replaceChildren(...children){this.children=children;}
    append(...children){this.children.push(...children);}
    querySelector(tag){return this.children.find(child=>child instanceof Anchor)||this.children.map(child=>child.querySelector(tag)).find(Boolean);}
    focus(){this.focused=true;}
  }
  class Anchor extends Element {}
  const root=new Element();root.dataset={tiles:'/tiles/{z}/{x}/{y}.pbf',worker:'/static/worker.js'};
  const status=new Element();
  const markers=[],requests=[];
  let map,worker,workerCount;
  const state={count:5,zoom:5,terminal:false,photos:[{short_id:'aaaaaaaaaaab',title:'First photo'},{short_id:'aaaaaaaaaaac',title:'<script>Second photo</script>'}],total:2};
  const popups=[];
  class FakePopup {
    handlers={};
    constructor(){popups.push(this);}
    setLngLat(value){this.coordinates=value;return this;}
    setDOMContent(value){this.content=value;return this;}
    addTo(){return this;}
    on(event,callback){this.handlers[event]=callback;return this;}
    remove(){this.removed=true;this.handlers.close?.();}
  }
  class FakeMap {
    handlers={};
    constructor(options){map=this;this.options=options;}
    addControl(){}
    on(event,callback){this.handlers[event]=callback;}
    getBounds(){return {getWest:()=>-80,getSouth:()=>43,getEast:()=>-79,getNorth:()=>44};}
    getZoom(){return state.zoom;}
    easeTo(value){this.lastZoom=value;}
    remove(){this.removed=true;}
    resize(){this.resized=true;}
  }
  class FakeMarker {
    constructor({element}){this.element=element;markers.push(this);}
    setLngLat(value){this.coordinates=value;return this;}
    addTo(){return this;}
    remove(){this.removed=true;}
  }
  const window=new Element();
  const context=vm.createContext({
    harness:{Map:FakeMap,Marker:FakeMarker,Popup:FakePopup,NavigationControl:class {},setWorkerUrl:value=>{worker=value;},setWorkerCount:value=>{workerCount=value;}},
    document:{getElementById:id=>id==='photo-map'?root:status,createElement:tag=>tag==='a'?new Anchor():new Element()},
    HTMLAnchorElement:Anchor,window,AbortController,
    fetch:async(url,options)=>{requests.push({url,...options});return {ok:true,json:async()=>url.startsWith('/map/photos.json')?({photos:state.photos,total:state.total}):({features:[{geometry:{type:'Point',coordinates:[-79.4,43.7]},properties:{count:state.count,terminal:state.terminal,short_id:'aaaaaaaaaaab',thumb_hash:'a'.repeat(64)}}]})};},
  });
  vm.runInContext(source,context);
  return {state,map,worker,workerCount,markers,requests,popups,window,status};
}

test('cluster requests, zoom actions, photo links, thumbnails and stale-request cancellation',async()=>{
  const {state,map,worker,workerCount,markers,requests,window}=harness();
  assert.equal(worker,'/static/worker.js');assert.equal(workerCount,1);
  assert.equal(map.options.style.sources.basemap.tiles[0],'/tiles/{z}/{x}/{y}.pbf');
  map.handlers.load();await tick();
  assert.equal(requests[0].url,'/map/tiles.json?bbox=-80,43,-79,44&zoom=5');
  assert.equal(markers[0].element.textContent,'5');
  markers[0].element.listeners.click();
  assert.equal(map.lastZoom.zoom,7);
  state.count=1;state.zoom=15;map.handlers.moveend();await tick();
  assert.equal(requests[0].signal.aborted,true);assert.equal(markers[0].removed,true);
  assert.equal(markers[1].element.href,'/p/aaaaaaaaaaab');
  assert.equal(markers[1].element.children[0].src,`/i/${'a'.repeat(64)}/thumb.jpeg`);
  assert.equal(markers[1].element.children[0].width,40);
  window.listeners.pagehide({persisted:false});assert.equal(map.removed,true);assert.equal(requests[1].signal.aborted,true);
});

test('WebGL initialization failure preserves the place fallback',()=>{
  const status={};
  const context=vm.createContext({harness:{Map:class {constructor(){throw new Error('WebGL unavailable');}},setWorkerUrl(){},setWorkerCount(){}},document:{getElementById:id=>id==='photo-map'?{dataset:{tiles:'/tiles/{z}/{x}/{y}',worker:'/worker'}}:status}});
  vm.runInContext(source,context);
  assert.match(status.textContent,/Browse the places below/);
});


test('persisted hide and restore retain the map and refresh its data on every cycle',async()=>{
  const {map,requests,window}=harness();
  map.handlers.load();await tick();
  for (let i=0;i<2;i++) {
    window.listeners.pagehide({persisted:true});
    assert.equal(map.removed,undefined);
    assert.equal(requests.at(-1).signal.aborted,true);
    const before=requests.length;
    window.listeners.pageshow({persisted:true});await tick();
    assert.equal(map.resized,true);
    assert.equal(requests.length,before+1);
    assert.equal(requests.at(-1).signal.aborted,false);
  }
  window.listeners.pagehide({persisted:false});
  assert.equal(map.removed,true);
});

test('collocated photos use one count marker and an accessible paginated chooser',async()=>{
  const {state,map,markers,popups,requests,window}=harness();
  state.count=2;state.zoom=15;state.terminal=true;
  map.handlers.load();await tick();
  assert.equal(markers.length,1);
  assert.equal(markers[0].element.textContent,'2');
  assert.equal(markers[0].element['aria-label'],'Choose from 2 photos');
  markers[0].element.listeners.click();await tick();
  assert.equal(map.lastZoom,undefined);
  assert.equal(requests.at(-1).url,'/map/photos.json?lon=-79.4&lat=43.7&page=1');
  const list=popups[0].content.children[1];
  assert.deepEqual(list.children.map(item=>item.children[0].href),['/p/aaaaaaaaaaab','/p/aaaaaaaaaaac']);
  assert.equal(list.children[0].children[0].focused,true);
  assert.equal(list.children[1].children[0].textContent,'<script>Second photo</script>');
  assert.equal(list.children[1].children[0].children.length,0);
  state.total=61;
  state.zoom=3;map.handlers.moveend();await tick();
  markers.at(-1).element.listeners.click();await tick();
  assert.equal(map.lastZoom,undefined,'an unsplittable cluster opens the same chooser at low zoom');
  const next=popups.at(-1).content.children[2].children[0];
  assert.equal(next.textContent,'Next page');
  next.listeners.click();await tick();
  assert.match(requests.at(-1).url,/page=2$/);
  assert.equal(popups.at(-1).content.children[2].children[0].textContent,'Previous page');
  window.listeners.pagehide({persisted:true});
  assert.equal(requests.at(-1).signal.aborted,true);
});
