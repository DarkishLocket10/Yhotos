import {createVideo} from "./lib/video.js";
import { PageCursor, mergePhotos, parseGallery, hydrateGrid, swipeDirection, type Photo, type Gallery } from "./lib/gallery.js";
import { containedWidth, safeMediaUrl, zoomSourceSet, DecodedMedia } from "./lib/media.js";
import { enhance, plainClick, reducedData } from "./lib/lifecycle.js";
enhance(signal => {
  const element = document.querySelector<HTMLDialogElement>("#lightbox");
  const json = document.querySelector<HTMLScriptElement>("#gallery-data");
  if (!element || !json || typeof element.showModal !== "function") return;
  const dialog = element;
  const images = new Map<string, Photo["image"]>();
  const gridImages = new Map<string, HTMLImageElement>(), nearby = new Map<string, HTMLImageElement>();
  document.querySelectorAll<HTMLAnchorElement>("[data-photo-link]").forEach(link => {
    const img = link.querySelector("img"), picture = link.querySelector("picture");
    const id = new URL(link.href).pathname.split("/").pop(); if (!img || !picture || !id) return;
    gridImages.set(id, img);
    const source = (type: string, hdr = false) => ({ all: [...picture.querySelectorAll<HTMLSourceElement>("source")].find(s => s.type === type && (hdr ? s.media === "(dynamic-range: high)" : !s.media))?.srcset || "" });
    images.set(id, { src: img.getAttribute("src") || "", width: Number(img.getAttribute("width")), height: Number(img.getAttribute("height")),
      jpeg: { all: img.srcset }, avif: source("image/avif"), jxl: source("image/jxl"), hdr_jpeg: source("image/jpeg", true), hdr_avif: source("image/avif", true), hdr_jxl: source("image/jxl", true) });
  });
  let gallery: Gallery; try { gallery = hydrateGrid(JSON.parse(json.textContent || "null"), images); } catch { return; }
  const photos = gallery.photos, cursor = new PageCursor(gallery.next_page);
  const $ = <T extends HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const stage = $<HTMLDivElement>("[data-stage]"), picture = $<HTMLPictureElement>("[data-picture]");
  const status = $<HTMLParagraphElement>("[role=status]");
  const origin = location.href;
  let playback: AbortController | null = null;
  let index = 0, generation = 0, opener: HTMLElement | null = null, timer: ReturnType<typeof setInterval> | undefined;
  let scale = 1, tx = 0, ty = 0;
  const upgrades = new DecodedMedia();
  let lastTap: number | null = null;
  let restoring = false;
  const pointers = new Map<number, { x: number; y: number }>();
  let start = { x: 0, y: 0, time: 0 }, pinch = 0;
  function resetGesture(): void { pointers.clear(); pinch = 0; lastTap = null; start = { x: 0, y: 0, time: 0 }; }
  const setStatus = (message: string) => { status.textContent = message; };
  function pause(): void { clearInterval(timer); timer = undefined; $("[data-slideshow]").setAttribute("aria-pressed", "false"); $("[data-slideshow]").textContent = "Play slideshow"; }
  function transform(): void {
    const image = $<HTMLImageElement>("[data-image]");
    if (image) image.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
    stage.classList.toggle("zoomed", scale > 1);
    $("[data-zoom]").setAttribute("aria-pressed", String(scale > 1));
    $("[data-zoom]").setAttribute("aria-label", scale > 1 ? "Reset photo zoom" : "Zoom photo");
    $("[data-zoom]").textContent = scale > 1 ? "Reset zoom" : "Zoom";
  }
  function displayWidth(): number {
    const box = picture.querySelector<HTMLImageElement>("img"), photo = photos[index];
    return containedWidth(box?.clientWidth || stage.clientWidth, box?.clientHeight || stage.clientHeight, photo.image.width, photo.image.height);
  }
  function upgradeImage(): void {
    if (!dialog.open || photos[index].video) return;
    const photo = photos[index], width = displayWidth(), pixels = width * scale * devicePixelRatio;
    const original = photo.caps.download && pixels > photo.zoom_width && photo.original ? safeMediaUrl(photo.original) : null;
    const sets = (["hdr_jxl", "hdr_avif", "hdr_jpeg", "jxl", "avif", "jpeg"] as const).map(key => zoomSourceSet(photo.image[key].all, pixels));
    const key = original || sets.join("|");
    if (key === upgrades.key) return;
    const upgrade = document.createElement("picture");
    if (!original) {
      for (const [position, type, hdr] of [[0, "image/jxl", true], [1, "image/avif", true], [2, "image/jpeg", true], [3, "image/jxl", false], [4, "image/avif", false]] as const) {
        const candidate = sets[position]; if (!candidate) continue;
        const source = document.createElement("source"); source.type = type; source.srcset = candidate; source.sizes = `${width * scale}px`; if (hdr) source.media = "(dynamic-range: high)"; upgrade.append(source);
      }
    }
    const fallback = original || safeMediaUrl(photo.image.src);
    if (!fallback) return;
    const image = document.createElement("img"); image.dataset.image = ""; image.alt = photo.title; image.width = photo.image.width; image.height = photo.image.height;
    image.sizes = `${width * scale}px`; if (!original && sets[5]) image.srcset = sets[5]; image.src = fallback; upgrade.append(image);
    // A connected offscreen picture lets the browser negotiate HDR and format.
    upgrade.className = "lightbox-upgrade"; stage.append(upgrade);
    void upgrades.show(key, () => image.decode(), () => {
      if (dialog.open) { picture.replaceChildren(...upgrade.childNodes); transform(); }
    }).catch(() => setStatus("Higher resolution is unavailable.")).finally(() => upgrade.remove());
  }
  function zoom(value: number): void {
    if (photos[index].video) return;
    pause();
    const photo = photos[index], cap = Math.max(1, photo.zoom_width / (displayWidth() * devicePixelRatio));
    scale = Math.max(1, Math.min(value, photo.caps.download ? 12 : cap));
    if (scale === 1) { tx = 0; ty = 0; }
    transform(); upgradeImage();
  }
  let paging: Promise<void> | null = null;
  function more(): Promise<void> {
    if (paging) return paging;
    const page = cursor.begin(); if (page === null) return Promise.resolve();
    paging = (async () => { try {
      const response = await fetch(`/api/v1/albums/${gallery.album}/photos?page=${page}&sort=${gallery.sort}`, { credentials: "same-origin", signal });
      if (!response.ok) throw new Error("Unavailable");
      const next = parseGallery(await response.json());
      if (next.album !== gallery.album || next.page !== page || next.sort !== gallery.sort) throw new Error("Invalid cursor");
      mergePhotos(photos, next.photos); cursor.finish(next.next_page);
    } catch { cursor.fail(); setStatus("Could not load the next page. Try Next again."); }
    finally { paging = null; } })();
    return paging;
  }
  function preload(): void {
    if (reducedData()) return;
    const keep = new Set([photos[index - 1]?.id, photos[index]?.id, photos[index + 1]?.id]);
    for (const id of nearby.keys()) if (!keep.has(id)) nearby.delete(id);
    for (const position of [index - 1, index + 1]) {
      const photo = photos[position]; if (!photo) continue;
      const large = photo.image.jpeg.all.split(", ").find(s => /\/large\.jpeg /.test(s))?.split(" ")[0];
      const url = large ? safeMediaUrl(large) : null;
      if (url && !nearby.has(photo.id)) { const image = new Image(); image.src = url; nearby.set(photo.id, image); void image.decode().catch(() => {}); }
    }
  }
  function historyPhoto(push: boolean): void {
    const url = `/p/${photos[index].id}?sort=${gallery.sort}`;
    if (push) history.pushState({ yhotosLightbox: true, id: photos[index].id, origin }, "", url);
    else history.replaceState({ yhotosLightbox: true, id: photos[index].id, origin }, "", url);
  }
  function renderDownloads(photo: Photo): void {
    const downloads = dialog.querySelector<HTMLElement>("#lightbox-downloads"); downloads?.replaceChildren();
    const button = dialog.querySelector<HTMLButtonElement>("[data-download]"); if (button) button.hidden = !photo.caps.download;
    for (const download of photo.downloads) { const url = safeMediaUrl(download.href); if (!url) continue; const li = document.createElement("li"), a = document.createElement("a"); a.href = url; a.download = ""; a.textContent = download.label; li.append(a); downloads?.append(li); }
  }
  function render(seed?: HTMLImageElement): void {
    playback?.abort(); playback = null; picture.hidden = false;
    const photo = photos[index], version = ++generation;
    scale = 1; tx = 0; ty = 0; upgrades.reset(); resetGesture();
    $("#lightbox-title").textContent = photo.title;
    $("[data-description]").textContent = photo.description;
    const details = $("[data-details]"); details.replaceChildren();
    for (const [label, value] of photo.details) { const dt = document.createElement("dt"), dd = document.createElement("dd"); dt.textContent = label; dd.textContent = value; details.append(dt, dd); }
    const downloads = dialog.querySelector<HTMLElement>("#lightbox-downloads"); if (downloads) downloads.hidden = true;
    const downloadButton = dialog.querySelector<HTMLButtonElement>("[data-download]"); downloadButton?.setAttribute("aria-expanded", "false");
    renderDownloads(photo);
    for (const action of ["like", "flag"] as const) {
      const button = dialog.querySelector<HTMLButtonElement>(`[data-${action}]`);
      if (button) { button.hidden = !photo.caps[action]; button.disabled = true; button.dataset.photoId = photo.id; button.setAttribute("aria-pressed", "false"); button.textContent = action === "like" ? "Like photo" : "Flag photo for review"; }
    }
    const preview = document.createElement("img"); preview.dataset.image = ""; preview.alt = photo.title; preview.width = photo.image.width; preview.height = photo.image.height;
    const gridImage = gridImages.get(photo.id);
    const initial = seed || (gridImage?.naturalWidth ? gridImage : nearby.get(photo.id) || gridImage);
    const previewUrl = safeMediaUrl(initial?.currentSrc || initial?.src || photo.image.src);
    if (previewUrl) { preview.src = previewUrl; picture.replaceChildren(preview); transform(); }
    else picture.replaceChildren();
    $("[data-zoom]").hidden = !!photo.video;
    if (photo.video) {
      playback = new AbortController();
      const player = createVideo(photo.video, photo.image.src, photo.image.width, photo.image.height, playback.signal);
      if (player) { picture.hidden = true; stage.append(player); }
    }
    upgradeImage();
    $<HTMLButtonElement>("[data-previous]").disabled = index === 0;
    $<HTMLButtonElement>("[data-next]").disabled = index === photos.length - 1 && cursor.next === null;
    setStatus(`Photo ${index + 1} of ${photos.length}${cursor.next === null ? "" : ", more photos available"}`);
    const requestedAt = performance.now();
    if (photo.caps.download || photo.caps.like || photo.caps.flag) void fetch(`/api/v1/photos/${photo.id}`, { credentials: "same-origin", signal }).then(async response => {
      if (!response.ok) throw new Error("Unavailable"); const current = await response.json() as Photo;
      if (version !== generation || !dialog.open || !current.state) return;
      photo.original = current.original; photo.downloads = current.downloads; photo.caps = current.caps;
      renderDownloads(photo); if (photo.caps.download && dialog.querySelector<HTMLElement>("#lightbox-downloads")?.hidden === false) setStatus("Download sizes ready."); if (scale > 1) void zoom(scale);
      for (const action of ["like", "flag"]) { const b = dialog.querySelector<HTMLButtonElement>(`[data-${action}]`); if (b) { b.disabled = false; b.hidden = !current.caps[action as "like" | "flag"]; } }
      document.dispatchEvent(new CustomEvent("yhotos:photo-state", { detail: { root: dialog, state: current.state, id: photo.id, requestedAt } }));
    }).catch(() => { if (version === generation) setStatus("Could not refresh photo controls. Please reload to try again."); });
    preload();
    if (index === photos.length - 1 && cursor.next !== null && !reducedData()) void more().then(() => { if (dialog.open) preload(); });
  }
  async function navigate(direction: number, autoplay = false): Promise<void> {
    if (!autoplay) pause();
    const version = generation;
    if (direction > 0 && index === photos.length - 1 && cursor.next !== null) await more();
    if (!dialog.open || version !== generation) return;
    const next = index + direction;
    if (next < 0 || next >= photos.length) { pause(); return; }
    index = next; render(); historyPhoto(false);
  }
  function close(): void {
    if (!dialog.open) return;
    playback?.abort(); playback = null; picture.hidden = false;
    pause(); generation++; upgrades.reset(); if (document.fullscreenElement === dialog) void document.exitFullscreen().catch(() => {}); dialog.close();
    document.documentElement.classList.remove("lightbox-open");
    document.body.querySelectorAll<HTMLElement>("[data-lightbox-inert]").forEach(el => { el.inert = false; delete el.dataset.lightboxInert; });
    opener?.focus({ preventScroll: true });
  }
  document.addEventListener("click", event => {
    const link = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>("[data-photo-link]") : null;
    if (!link || !plainClick(event) || event.defaultPrevented) return;
    const id = new URL(link.href).pathname.split("/").pop(); const next = photos.findIndex(p => p.id === id); if (next < 0) return;
    event.preventDefault(); opener = link; index = next;
    performance.mark("lightbox-click"); render(link.querySelector("img") || undefined);
    document.documentElement.classList.add("lightbox-open");
    for (const child of document.body.children) if (child instanceof HTMLElement && child !== dialog && !child.inert) { child.inert = true; child.dataset.lightboxInert = ""; }
    dialog.showModal(); upgradeImage(); $("[data-close]").focus(); historyPhoto(true);
    requestAnimationFrame(() => requestAnimationFrame(() => { performance.measure("lightbox-first-paint", "lightbox-click"); }));
  }, { signal });
  function dismiss(): void { if (restoring) return; if (history.state?.yhotosLightbox) { restoring = true; history.back(); } else close(); }
  dialog.addEventListener("cancel", event => { event.preventDefault(); dismiss(); }, { signal });
  $("[data-close]").addEventListener("click", dismiss, { signal });
  window.addEventListener("popstate", event => {
    if (event.state?.yhotosLightbox && event.state.origin === origin) {
      const next = photos.findIndex(p => p.id === event.state.id); if (next >= 0) { index = next; if (!dialog.open) { dialog.showModal(); document.documentElement.classList.add("lightbox-open"); for (const el of document.body.children) if (el instanceof HTMLElement && el !== dialog && !el.inert) { el.inert = true; el.dataset.lightboxInert = ""; } } render(); }
    } else { close(); }
    // Prevent the transition island from reloading an album entry owned by this dialog.
    if (restoring || location.href === origin) { event.stopImmediatePropagation(); restoring = false; }
  }, { signal, capture: true });
  dialog.addEventListener("keydown", event => {
    if (!(event.target instanceof Element) || !event.target.closest("[data-slideshow]")) pause();
    if (event.target instanceof Element && event.target.closest(".video-player")) return;
    if (event.key === "ArrowRight") { event.preventDefault(); void navigate(1); }
    if (event.key === "ArrowLeft") { event.preventDefault(); void navigate(-1); }
    if (event.key === "Tab") {
      const focusable = [...dialog.querySelectorAll<HTMLElement>("button:not(:disabled), a[href], video[controls], input")].filter(e => !e.hidden && e.getClientRects().length);
      const first = focusable[0], last = focusable.at(-1);
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    }
  }, { signal });
  $("[data-next]").addEventListener("click", () => { void navigate(1); }, { signal });
  $("[data-previous]").addEventListener("click", () => { void navigate(-1); }, { signal });
  for (const [button, panel] of [["[data-info]", "#lightbox-info"], ["[data-download]", "#lightbox-downloads"]]) dialog.querySelector(button)?.addEventListener("click", () => { pause(); const target = $(panel); target.hidden = !target.hidden; if (panel === "#lightbox-downloads" && !target.hidden && !photos[index].downloads.length) setStatus("Loading download sizes…"); $(button).setAttribute("aria-expanded", String(!target.hidden)); }, { signal });
  $("[data-zoom]").addEventListener("click", () => { void zoom(scale === 1 ? 2 : 1); }, { signal });
  $("[data-share]").addEventListener("click", () => { pause(); if (!navigator.clipboard) { setStatus("Copy the photo URL from the address bar."); return; } void navigator.clipboard.writeText(new URL(`/p/${photos[index].id}`, location.origin).href).then(() => setStatus("Photo link copied.")).catch(() => setStatus("Copy the photo URL from the address bar.")); }, { signal });
  $("[data-fullscreen]").addEventListener("click", () => { pause(); if (!document.fullscreenEnabled || !dialog.requestFullscreen) { setStatus("Fullscreen is unavailable in this browser."); return; } const action = document.fullscreenElement ? document.exitFullscreen() : dialog.requestFullscreen(); void action.then(() => { $("[data-fullscreen]").textContent = document.fullscreenElement ? "Exit fullscreen" : "Enter fullscreen"; }).catch(() => setStatus("Fullscreen is unavailable in this browser.")); }, { signal });
  document.addEventListener("fullscreenchange", () => { $("[data-fullscreen]").textContent = document.fullscreenElement ? "Exit fullscreen" : "Enter fullscreen"; }, { signal });
  $("[data-slideshow]").addEventListener("click", () => {
    if (timer) { pause(); return; }
    $("[data-slideshow]").setAttribute("aria-pressed", "true"); $("[data-slideshow]").textContent = "Pause slideshow";
    timer = setInterval(() => { void navigate(1, true); }, 4000);
  }, { signal });
  dialog.addEventListener("pointerdown", event => { if (!(event.target instanceof Element) || !event.target.closest("[data-slideshow]")) pause(); }, { signal });
  document.addEventListener("visibilitychange", () => { if (document.hidden) pause(); }, { signal });
  stage.addEventListener("dragstart", event => event.preventDefault(), { signal });
  stage.addEventListener("wheel", event => { if (photos[index].video) return; event.preventDefault(); void zoom(scale * Math.exp(-event.deltaY * .002)); }, { signal, passive: false });
  stage.addEventListener("pointerdown", event => {
    if (photos[index].video) { resetGesture(); return; }
    pause(); stage.setPointerCapture(event.pointerId); pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.size === 1) start = { x: event.clientX, y: event.clientY, time: event.timeStamp };
    if (pointers.size === 2) { lastTap = null; const [a, b] = [...pointers.values()]; pinch = Math.hypot(a.x - b.x, a.y - b.y); }
  }, { signal });
  stage.addEventListener("pointermove", event => {
    const previous = pointers.get(event.pointerId); if (!previous) return;
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.size === 2) { const [a, b] = [...pointers.values()], distance = Math.hypot(a.x - b.x, a.y - b.y); if (pinch > 0) void zoom(scale * distance / pinch); pinch = distance; }
    else if (scale > 1) { tx += event.clientX - previous.x; ty += event.clientY - previous.y; transform(); }
  }, { signal });
  stage.addEventListener("pointerup", event => {
    if (photos[index].video) { resetGesture(); return; }
    if (!pointers.has(event.pointerId)) return;
    const multiple = pointers.size > 1 || pinch > 0; pointers.delete(event.pointerId);
    if (multiple) { if (!pointers.size) pinch = 0; return; }
    const dx = event.clientX - start.x, dy = event.clientY - start.y;
    if (scale === 1) { const direction = swipeDirection(dx, dy, event.timeStamp - start.time); if (direction) { void navigate(direction); return; } }
    if (Math.abs(dx) + Math.abs(dy) < 12) { if (lastTap !== null && event.timeStamp - lastTap < 300) { void zoom(scale === 1 ? 2 : 1); lastTap = null; } else lastTap = event.timeStamp; }
  }, { signal });
  stage.addEventListener("pointercancel", event => { pointers.delete(event.pointerId); pinch = 0; }, { signal });
  window.addEventListener("resize", () => { if (dialog.open) upgradeImage(); }, { signal });
  document.addEventListener("yhotos:before-navigate", close, { signal });
  signal.addEventListener("abort", () => { pause(); close(); });
});
