import type { Photo } from "./lib/gallery.js";
import { enhance } from "./lib/lifecycle.js";
import { staleRefresh } from "./lib/media.js";
function paint(root: ParentNode, state: NonNullable<Photo["state"]>): void {
  const like = root.querySelector<HTMLButtonElement>("[data-like]");
  const flag = root.querySelector<HTMLButtonElement>("[data-flag]");
  if (like) { like.setAttribute("aria-pressed", String(state.liked)); like.textContent = `${state.liked ? "Unlike photo" : "Like photo"}${state.like_count === undefined ? "" : ` (${state.like_count})`}`; }
  if (flag) { flag.setAttribute("aria-pressed", String(state.flagged)); flag.textContent = state.flagged ? "Flagged" : "Flag photo for review"; flag.disabled = state.flagged; }
}
enhance(signal => {
  const pending = new Set<string>(), settled = new Map<string, number>();
  // Refreshes that overlapped a mutation may carry pre-mutation state. A pending
  // mutation repaints when it settles; a settled one triggers a fresh read.
  function accept(root: ParentNode, id: string, state: NonNullable<Photo["state"]>, requestedAt: number): void {
    const current = root.querySelector<HTMLButtonElement>("[data-like], [data-flag]");
    if (current?.dataset.photoId && current.dataset.photoId !== id) return;
    if (!staleRefresh(pending, settled, id, requestedAt)) { paint(root, state); return; }
    if (pending.has(id)) return;
    const again = performance.now();
    void fetch(`/api/v1/photos/${id}`, { credentials: "same-origin", signal }).then(async response => {
      if (!response.ok) throw new Error("Unavailable");
      const fresh = await response.json() as Photo;
      if (!signal.aborted && fresh.state) accept(root, id, fresh.state, again);
    }).catch(() => {});
  }
  document.addEventListener("yhotos:photo-state", event => {
    if (!(event instanceof CustomEvent)) return;
    const detail = event.detail as { root: ParentNode; state: NonNullable<Photo["state"]>; id?: unknown; requestedAt?: unknown };
    if (typeof detail.id === "string" && typeof detail.requestedAt === "number") accept(detail.root, detail.id, detail.state, detail.requestedAt);
    else paint(detail.root, detail.state);
  }, { signal });
  const page = document.querySelector("#photo-data");
  if (page && document.querySelector("[data-like], [data-flag]")) {
    const photo = JSON.parse(page.textContent || "null") as Photo;
    const buttons = [...document.querySelectorAll<HTMLButtonElement>("[data-like], [data-flag]")];
    buttons.forEach(b => { b.disabled = true; });
    void fetch(`/api/v1/photos/${photo.id}`, { credentials: "same-origin", signal }).then(async response => {
      if (!response.ok) throw new Error("Unavailable");
      const current = await response.json() as Photo;
      if (signal.aborted) return;
      buttons.forEach(b => { b.disabled = false; });
      if (current.state) paint(document, current.state);
    }).catch(() => {
      if (signal.aborted) return;
      const status = document.querySelector<HTMLElement>("[data-action-status]");
      if (status) status.textContent = "Photo actions are unavailable. Please reload to try again.";
    });
  }
  document.addEventListener("click", event => {
    const button = event.target instanceof Element ? event.target.closest<HTMLButtonElement>("[data-like], [data-flag]") : null;
    if (!button || button.disabled) return;
    event.preventDefault();
    const id = button.dataset.photoId; if (!id || !/^[a-z2-7]{12}$/.test(id) || pending.has(id)) return;
    const flag = button.hasAttribute("data-flag");
    if (flag && !window.confirm("Flag this photo for the photographer to review?")) return;
    const root = button.closest("dialog, main") || document;
    const was = button.getAttribute("aria-pressed") === "true";
    const oldText = button.textContent;
    button.setAttribute("aria-pressed", String(!was)); button.textContent = flag ? "Flagged" : was ? "Like photo" : "Unlike photo";
    button.disabled = true; pending.add(id);
    void fetch(`/api/v1/photos/${id}/${flag ? "flag" : "like"}`, {
      method: !flag && was ? "DELETE" : "POST", credentials: "same-origin", headers: { "X-Requested-With": "yhotos" }, signal,
    }).then(async response => {
      if (!response.ok) throw new Error("Unavailable");
      const state = await response.json() as NonNullable<Photo["state"]>;
      if (button.dataset.photoId === id) paint(root, state);
    }).catch(() => {
      if (button.dataset.photoId !== id) return;
      button.setAttribute("aria-pressed", String(was)); button.textContent = oldText;
      const status = root.querySelector<HTMLElement>('[role="status"]');
      if (status) status.textContent = "Could not save your choice. Please try again.";
    }).finally(() => { pending.delete(id); settled.set(id, performance.now()); if (button.dataset.photoId === id) button.disabled = flag && button.getAttribute("aria-pressed") === "true"; });
  }, { signal });
});
