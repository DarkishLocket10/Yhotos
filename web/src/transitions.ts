/** Same-origin, server-rendered document swaps. The current CSP nonce remains authoritative. */
let swapping = false;
const assetName = (src: string) => src.split("/").at(-1)?.split(".")[0] || src;
const loadedAssets = new Map([...document.querySelectorAll<HTMLScriptElement>('script[type="module"][src]')].map(s => { const src = s.getAttribute("src") || ""; return [assetName(src), src]; }));
const nonce = document.querySelector<HTMLScriptElement>("script[nonce]")?.nonce;
function swap(url: string, html: string): void {
  const parsed = new DOMParser().parseFromString(html, "text/html");
  if (parsed.querySelector("base") || !parsed.querySelector("#gallery-data, #photo-data, .album-list")) throw new Error("Unsupported document");
  if (parsed.querySelector<HTMLLinkElement>('link[rel="stylesheet"]')?.getAttribute("href") !== document.querySelector<HTMLLinkElement>('link[rel="stylesheet"]')?.getAttribute("href")) throw new Error("Updated styles");
  const scripts = [...parsed.querySelectorAll<HTMLScriptElement>('script[type="module"][src]')].map(s => s.getAttribute("src") || "");
  if (scripts.some(s => !/^\/static\/(?:thumbhash|lightbox|likes|prefetch|transitions)\.[a-f0-9]{64}\.js$/.test(s))) throw new Error("Unsupported script");
  // Keep a rolling deployment from installing two revisions of the same island.
  if (scripts.some(src => loadedAssets.has(assetName(src)) && loadedAssets.get(assetName(src)) !== src)) throw new Error("Updated islands");
  parsed.querySelectorAll("script").forEach(s => { if (s.type !== "application/json") s.remove(); });
  parsed.querySelectorAll<HTMLElement>("style, script").forEach(s => { if (nonce) s.setAttribute("nonce", nonce); });
  document.dispatchEvent(new Event("yhotos:before-navigate"));
  document.title = parsed.title;
  document.head.querySelectorAll("style, meta[property], meta[name='description'], meta[name='twitter:card']").forEach(el => el.remove());
  for (const el of parsed.head.querySelectorAll("style, meta[property], meta[name='description'], meta[name='twitter:card']")) document.head.append(document.adoptNode(el));
  document.body.replaceWith(document.adoptNode(parsed.body));
  history.pushState({ yhotosPage: true }, "", url);
  window.scrollTo(0, 0);
  for (const src of scripts) {
    loadedAssets.set(assetName(src), src);
    const script = document.createElement("script"); script.type = "module"; script.src = src;
    if (nonce) script.nonce = nonce;
    document.body.append(script);
  }
  document.dispatchEvent(new Event("yhotos:navigate"));
  const main = document.querySelector("main"); if (main) { main.tabIndex = -1; main.focus({ preventScroll: true }); }
}
document.addEventListener("yhotos:visit", event => {
  if (!(event instanceof CustomEvent) || !document.startViewTransition || swapping) return;
  const { url, html } = event.detail as { url: string; html: string };
  if (new URL(url).origin !== location.origin) return;
  event.preventDefault(); swapping = true;
  const update = () => swap(url, html);
  let finished: Promise<unknown>;
  try { finished = matchMedia("(prefers-reduced-motion: reduce)").matches ? Promise.resolve().then(update) : document.startViewTransition(update).finished; } catch { location.assign(url); return; }
  void finished.catch(() => location.assign(url)).finally(() => { swapping = false; });
});
window.addEventListener("popstate", event => {
  // The lightbox owns its entries; other history traversal revalidates the HTML.
  if (!document.querySelector<HTMLDialogElement>("#lightbox")?.open && !event.state?.yhotosLightbox) location.reload();
});
