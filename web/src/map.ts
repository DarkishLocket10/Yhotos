import { Map, Marker, NavigationControl, setWorkerUrl, setWorkerCount } from "maplibre-gl";

interface Cluster {
  geometry: { type: "Point"; coordinates: [number, number] };
  properties: { count: number; short_id: string; thumb_hash: string | null };
}
const container = document.getElementById("photo-map");
const status = document.getElementById("map-status");
if (container?.dataset.tiles && container.dataset.worker) {
  try {
    setWorkerUrl(container.dataset.worker);
    setWorkerCount(1);
    const map = new Map({
      container, center: [0, 20], zoom: 1, maxZoom: 20, renderWorldCopies: false,
      attributionControl: false, maxBounds: [[-180,-85.051129],[180,85.051129]],
      // A local style avoids remote styles, sprites and fonts introducing origins.
      style: {
        version: 8,
        sources: { basemap: { type: "vector", tiles: [container.dataset.tiles], maxzoom: 14 } },
        layers: [
          { id: "background", type: "background", paint: { "background-color": "#e5e9dc" } },
          { id: "water", type: "fill", source: "basemap", "source-layer": "water", paint: { "fill-color": "#a7cddd" } },
          { id: "parks", type: "fill", source: "basemap", "source-layer": "park", paint: { "fill-color": "#c4d8b1" } },
          { id: "roads", type: "line", source: "basemap", "source-layer": "transportation", paint: { "line-color": "#ffffff", "line-width": 1 } },
          { id: "borders", type: "line", source: "basemap", "source-layer": "boundary", paint: { "line-color": "#83927a", "line-width": 1 } },
        ],
      },
    });
    map.addControl(new NavigationControl({showCompass: false}), "top-right");
    let markers: Marker[] = [];
    let pending: AbortController | undefined;
    const refresh = async () => {
      pending?.abort();
      const controller = new AbortController();
      pending = controller;
      const bounds = map.getBounds();
      const bbox = [Math.max(-180,bounds.getWest()),Math.max(-85.051129,bounds.getSouth()),Math.min(180,bounds.getEast()),Math.min(85.051129,bounds.getNorth())];
      const zoom = Math.min(20,Math.floor(map.getZoom()));
      try {
        const response = await fetch(`/map/tiles.json?bbox=${bbox.join(",")}&zoom=${zoom}`, {signal: controller.signal, credentials: "same-origin"});
        if (!response.ok) throw new Error("Map data unavailable");
        const data: {features: Cluster[]} = await response.json();
        if (controller.signal.aborted) return;
        for (const marker of markers) marker.remove();
        markers = data.features.map(feature => {
          const {count,short_id,thumb_hash} = feature.properties;
          const individual = zoom >= 15 && count === 1;
          const element = document.createElement(individual ? "a" : "button");
          element.className = "map-marker";
          element.textContent = String(count);
          element.setAttribute("aria-label", individual ? "Open photo" : `Zoom to ${count} photos`);
          if (individual && element instanceof HTMLAnchorElement && /^[a-z2-7]{12}$/.test(short_id)) element.href = `/p/${short_id}`;
          if (thumb_hash && /^[0-9a-f]{64}$/.test(thumb_hash) && individual) {
            const image = document.createElement("img");
            image.src = `/i/${thumb_hash}/thumb.jpeg`;
            image.width = 40; image.height = 40; image.alt = "Photo thumbnail";
            element.replaceChildren(image);
          }
          if (!individual) element.addEventListener("click", () => map.easeTo({center:feature.geometry.coordinates,zoom:Math.min(20,zoom+2)}));
          return new Marker({element}).setLngLat(feature.geometry.coordinates).addTo(map);
        });
        if (status) status.textContent = `${data.features.length} photo locations shown. Use the map controls or browse the places below.`;
      } catch (error) {
        if (!controller.signal.aborted && status) status.textContent = "Map data unavailable. Browse the places below.";
      }
    };
    map.on("load", () => { void refresh(); });
    map.on("moveend", () => { void refresh(); });
    map.on("error", () => { if (status) status.textContent = "Some map tiles are unavailable. Photo locations and places remain available."; });
    window.addEventListener("pagehide", () => { pending?.abort(); map.remove(); }, {once:true});
  } catch {
    if (status) status.textContent = "Interactive maps are unavailable in this browser. Browse the places below.";
  }
}
