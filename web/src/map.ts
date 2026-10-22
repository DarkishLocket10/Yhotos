import { Map, Marker, Popup, NavigationControl, setWorkerUrl, setWorkerCount } from "maplibre-gl";

interface Cluster {
  geometry: { type: "Point"; coordinates: [number, number] };
  properties: { count: number; short_id: string; thumb_hash: string | null; terminal: boolean };
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
    let chooser: Popup | undefined;
    let choosing: AbortController | undefined;
    const showPhotos = async (coordinates: [number, number], page = 1) => {
      choosing?.abort();
      const controller = new AbortController();
      choosing = controller;
      try {
        const [lon, lat] = coordinates;
        const response = await fetch(`/map/photos.json?lon=${lon}&lat=${lat}&page=${page}`, {signal: controller.signal, credentials: "same-origin"});
        if (!response.ok) throw new Error("Photos unavailable");
        const data: {photos: {short_id: string; title: string}[]; total: number} = await response.json();
        if (controller.signal.aborted) return;
        const content = document.createElement("section");
        content.className = "map-chooser";
        content.setAttribute("aria-label", "Photos at this location");
        const heading = document.createElement("h2");
        heading.textContent = `${data.total} photos at this location`;
        const list = document.createElement("ul");
        for (const photo of data.photos) {
          if (!/^[a-z2-7]{12}$/.test(photo.short_id)) continue;
          const item = document.createElement("li");
          const link = document.createElement("a");
          link.href = `/p/${photo.short_id}`;
          link.textContent = photo.title || "Open photo";
          item.append(link);
          list.append(item);
        }
        content.append(heading, list);
        const navigation = document.createElement("nav");
        navigation.setAttribute("aria-label", "Photo list pages");
        for (const [label, number] of [["Previous page", page - 1], ["Next page", page + 1]] as const) {
          if (number < 1 || (number - 1) * 60 >= data.total) continue;
          const button = document.createElement("button");
          button.type = "button";
          button.textContent = label;
          button.addEventListener("click", () => { void showPhotos(coordinates, number); });
          navigation.append(button);
        }
        content.append(navigation);
        chooser?.remove();
        chooser = new Popup({maxWidth: "320px"}).setLngLat(coordinates).setDOMContent(content).addTo(map);
        chooser.on("close", () => controller.abort());
        list.querySelector("a")?.focus();
      } catch {
        if (!controller.signal.aborted && status) status.textContent = "Photo list unavailable. Browse the places below.";
      }
    };
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
          const {count,short_id,thumb_hash,terminal} = feature.properties;
          const individual = zoom >= 15 && count === 1;
          const element = document.createElement(individual ? "a" : "button");
          element.className = "map-marker";
          element.textContent = String(count);
          element.setAttribute("aria-label", individual ? "Open photo" : terminal ? `Choose from ${count} photos` : `Zoom to ${count} photos`);
          if (individual && element instanceof HTMLAnchorElement && /^[a-z2-7]{12}$/.test(short_id)) element.href = `/p/${short_id}`;
          if (thumb_hash && /^[0-9a-f]{64}$/.test(thumb_hash) && individual) {
            const image = document.createElement("img");
            image.src = `/i/${thumb_hash}/thumb.jpeg`;
            image.width = 40; image.height = 40; image.alt = "Photo thumbnail";
            element.replaceChildren(image);
          }
          if (!individual) element.addEventListener("click", () => {
            if (terminal || zoom === 20) void showPhotos(feature.geometry.coordinates);
            else map.easeTo({center:feature.geometry.coordinates,zoom:Math.min(20,zoom+2)});
          });
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
    window.addEventListener("pagehide", event => {
      pending?.abort();
      choosing?.abort();
      if (!event.persisted) map.remove();
    });
    window.addEventListener("pageshow", event => {
      if (event.persisted) {
        map.resize();
        void refresh();
      }
    });
  } catch {
    if (status) status.textContent = "Interactive maps are unavailable in this browser. Browse the places below.";
  }
}
