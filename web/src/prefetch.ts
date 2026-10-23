import { PrefetchCache } from "./lib/cache.js";
import { enhance, plainClick, reducedData } from "./lib/lifecycle.js";
const cache = new PrefetchCache<string>();
let fetching = new Set<string>();
function eligible(target: EventTarget | null): HTMLAnchorElement | null {
  const link = target instanceof Element ? target.closest<HTMLAnchorElement>("a[href]") : null;
  if (!link || link.target || link.hasAttribute("download")) return null;
  const url = new URL(link.href);
  return url.origin === location.origin && /^\/(?:$|a\/|p\/)/.test(url.pathname) && !url.hash && url.href !== location.href ? link : null;
}
async function prefetch(url: string, signal: AbortSignal): Promise<void> {
  if (reducedData() || cache.get(url) || fetching.has(url) || fetching.size >= 2) return;
  fetching.add(url);
  try {
    const response = await fetch(url, { credentials: "same-origin", priority: "low", signal });
    // Authenticated and token-bearing pages must be reauthorized on navigation.
    if (response.ok && !response.redirected && response.headers.get("content-type")?.startsWith("text/html") && !/private|no-store/.test(response.headers.get("cache-control") || "")) {
      const html = await response.text();
      if (html.length < 512_000) cache.put(url, html);
    }
  } catch { /* Prefetch failure leaves the real link intact. */ }
  finally { fetching.delete(url); }
}
enhance(signal => {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const schedule = (event: Event) => {
    const link = eligible(event.target); if (!link) return;
    clearTimeout(timer); timer = setTimeout(() => { void prefetch(link.href, signal); }, 65);
  };
  document.addEventListener("pointerover", schedule, { signal });
  document.addEventListener("focusin", schedule, { signal });
  document.addEventListener("touchstart", schedule, { signal, passive: true });
  document.addEventListener("pointerout", () => clearTimeout(timer), { signal });
  document.addEventListener("focusout", () => clearTimeout(timer), { signal });
  signal.addEventListener("abort", () => clearTimeout(timer));
  document.addEventListener("click", event => {
    const link = eligible(event.target);
    if (event.defaultPrevented || !plainClick(event) || !link || link.hasAttribute("data-photo-link") || reducedData() || !document.startViewTransition) return;
    const html = cache.get(link.href); if (!html) return;
    const navigation = new CustomEvent("yhotos:visit", { cancelable: true, detail: { url: link.href, html } });
    if (!document.dispatchEvent(navigation)) event.preventDefault();
  }, { signal });
});
window.addEventListener("pagehide", () => { cache.clear(); fetching = new Set(); });
