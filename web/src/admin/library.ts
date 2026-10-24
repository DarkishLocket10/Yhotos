import {type Transfer, safeUpload, stored, transfer} from "./tus";

// Admin islands use the exact same JSON actions for pointer and keyboard input.
const root = document.querySelector<HTMLElement>("[data-admin-page]");
const csrf = document.querySelector<HTMLMetaElement>('meta[name="csrf-token"]')?.content ?? "";
const status = document.querySelector<HTMLElement>("#admin-status");
function message(text: string) { if (status) status.textContent = text; }
export function selectedIds(boxes: {checked: boolean; value: string}[]) { return boxes.filter(b => b.checked).map(b => b.value); }
export function rangeSelection(boxes: {checked: boolean}[], start: number, end: number, checked: boolean) { for (let i = Math.min(start, end); i <= Math.max(start, end); i++) if (boxes[i]) boxes[i].checked = checked; }
export function keyboardMove<T>(items: T[], index: number, down: boolean): T[] { const next = [...items], target = index + (down ? 1 : -1); if (index >= 0 && target >= 0 && target < items.length) [next[index], next[target]] = [next[target], next[index]]; return next; }
// Each request permutes at most 1,000 occupied slots; other photos retain their order.
export function reorderBatches(original: string[], desired: string[]): string[][] {
  const current = [...original], positions = new Map(current.map((id, i) => [id, i]));
  if (desired.length !== current.length || new Set(desired).size !== current.length || desired.some(id => !positions.has(id))) throw new Error("Photo order changed. Reload the album.");
  const batches: string[][] = [], touched = new Set<number>();
  const flush = () => { if (touched.size) batches.push([...touched].sort((a,b) => a-b).map(i => current[i])); touched.clear(); };
  for (let i = 0; i < desired.length; i++) {
    if (current[i] === desired[i]) continue;
    const j = positions.get(desired[i])!;
    if (new Set([...touched, i, j]).size > 1000) flush();
    touched.add(i); touched.add(j);
    [current[i], current[j]] = [current[j], current[i]];
    positions.set(current[i], i); positions.set(current[j], j);
  }
  flush();
  return batches;
}
async function api(endpoint: string, body: unknown, method = "POST") {
  const response = await fetch(endpoint, {method, credentials: "same-origin", headers: {"Content-Type": "application/json", "X-CSRF-Token": csrf}, body: JSON.stringify(body)});
  if (!response.ok || response.redirected) throw new Error("The change was rejected. Check your session and entered values.");
  return response.status === 204 ? {} : response.json();
}
function assign(obj: Record<string, unknown>, path: string, value: unknown) { const keys = path.split("."); let target = obj; for (const key of keys.slice(0, -1)) { target[key] ??= {}; target = target[key] as Record<string, unknown>; } target[keys.at(-1)!] = value; }
function boxes() { return [...document.querySelectorAll<HTMLInputElement>(".photo-select")]; }
function selection() { return selectedIds(boxes()); }
function updateCount() { const node = document.querySelector("#selection-count"); if (node) node.textContent = `${selection().length} photos selected`; }
let anchor: HTMLInputElement | null = null;
for (const box of boxes()) box.addEventListener("click", event => {
  const current = boxes(), start = anchor ? current.indexOf(anchor) : -1;
  if ((event as MouseEvent).shiftKey && start >= 0) rangeSelection(current, start, current.indexOf(box), box.checked);
  anchor = box;
  updateCount();
});
document.querySelector("#select-all")?.addEventListener("click", () => { for (const box of boxes()) box.checked = true; updateCount(); });
document.querySelector("#select-none")?.addEventListener("click", () => { for (const box of boxes()) box.checked = false; updateCount(); });
for (const form of document.querySelectorAll<HTMLFormElement>(".admin-form")) form.addEventListener("submit", async event => {
  event.preventDefault();
  const body: Record<string, unknown> = {};
  for (const input of form.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>("[name]")) {
    let value: unknown = input.value;
    switch (input.dataset.type) {
      case "boolean": value = (input as HTMLInputElement).checked; break;
      case "optional": case "optional-select": case "password": value = input.value || null; break;
      case "optional-number": value = input.value ? Number(input.value) : null; break;
      case "tags": value = input.value.split(",").map(v => v.trim()).filter(Boolean); break;
      case "ids-select": value = [...(input as HTMLSelectElement).selectedOptions].map(o => o.value); break;
      case "grants": value = [...(input as HTMLSelectElement).selectedOptions].map(o => { const [kind, id] = o.value.split(":"); return {kind, id}; }); break;
      case "selection": value = selection(); break;
    }
    assign(body, input.name, value);
  }
  if (Array.isArray(body.ids) && (body.ids.length === 0 || body.ids.length > 1000)) { message("Select between 1 and 1,000 photos."); return; }
  const submit = form.querySelector<HTMLButtonElement>('button[type="submit"]'); if (submit) submit.disabled = true;
  try {
    const data = await api(form.dataset.endpoint!, body, form.dataset.method);
    const secret = data.temporary_password ?? data.url;
    if (secret) { const output = document.querySelector<HTMLOutputElement>("#secret-value"); if (output) output.textContent = secret; document.querySelector<HTMLDialogElement>("#admin-secret")?.showModal(); }
    else location.reload();
  } catch (error) { message((error as Error).message); }
  finally { if (submit) submit.disabled = false; }
});
document.querySelector("#secret-copy")?.addEventListener("click", async () => { try { await navigator.clipboard.writeText(document.querySelector("#secret-value")?.textContent ?? ""); message("Copied."); } catch { message("Select and copy the displayed value."); } });
document.querySelector("#secret-close")?.addEventListener("click", () => location.reload());
function cards() { return [...document.querySelectorAll<HTMLElement>(".admin-card[data-id]")]; }
let dragged: HTMLElement | null = null;
for (const card of cards()) {
  card.addEventListener("dragstart", event => { dragged = card; event.dataTransfer?.setData("text/plain", card.dataset.id ?? ""); });
  card.addEventListener("dragover", event => event.preventDefault());
  card.addEventListener("drop", async event => { event.preventDefault(); if (!dragged || dragged === card) return;
    if (root?.dataset.adminPage === "albums") { try { await api("/admin/api/actions", {op: "album_move", id: dragged.dataset.id, direction: "move", parent_id: event.clientY < card.getBoundingClientRect().top + 60 ? (card.dataset.parent || null) : card.dataset.id, before_id: event.clientY < card.getBoundingClientRect().top + 60 ? card.dataset.id : null}); location.reload(); } catch (e) { message((e as Error).message); } }
    else if (root?.dataset.manual === "true") card.before(dragged);
    dragged = null;
  });
  card.addEventListener("keydown", event => { if (event.altKey && ["ArrowUp", "ArrowDown"].includes(event.key) && root?.dataset.manual === "true") { event.preventDefault(); const list = keyboardMove(cards(), cards().indexOf(card), event.key === "ArrowDown"); for (const item of list) document.querySelector(".admin-cards")?.append(item); card.focus(); } });
}
let savedOrder = cards().filter(c => c.querySelector(".photo-select")).map(c => c.dataset.id!);
document.querySelector("#save-photo-order")?.addEventListener("click", async event => {
  const button = event.currentTarget as HTMLButtonElement; button.disabled = true;
  try {
    const desired = cards().filter(c => c.querySelector(".photo-select")).map(c => c.dataset.id!);
    for (const ids of reorderBatches(savedOrder, desired)) {
      await api("/admin/api/actions", {op: "bulk", ids, action: {kind: "reorder", album_id: root?.dataset.album}});
      const selected = new Set(ids), slots = savedOrder.flatMap((id, i) => selected.has(id) ? [i] : []);
      for (const [i, slot] of slots.entries()) savedOrder[slot] = ids[i];
    }
    message("Photo order saved.");
  } catch (e) { message((e as Error).message); } finally { button.disabled = false; }
});
// Keep each target/file URL in the live island as well as across reloads.
const replacements = new Map<string, Transfer>();
document.querySelector("#replace-start")?.addEventListener("click", async () => {
  const input = document.querySelector<HTMLInputElement>("#replace-file"), file = input?.files?.[0];
  if (!file || !input) { message("Choose a new original file."); return; }
  const button = document.querySelector<HTMLButtonElement>("#replace-start"); if (button) button.disabled = true;
  const key = `yhotos-replace:${JSON.stringify([input.dataset.photo, file.name, file.size, file.lastModified])}`;
  let work = replacements.get(key);
  if (!work) {
    work = {file, album: input.dataset.album!, replace: input.dataset.photo!, key, url: safeUpload(stored(key))};
    replacements.set(key, work);
  }
  try {
    await transfer(work, Number(input.dataset.chunkBytes), csrf, offset => {
      const progress = document.querySelector<HTMLProgressElement>("#replace-progress"); if (progress) progress.value = offset * 100 / file.size;
    });
    replacements.delete(key);
    location.reload();
  } catch (e) { message((e as Error).message); } finally { if (button) button.disabled = false; }
});
