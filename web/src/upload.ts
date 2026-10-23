import {remember, safeUpload, stored, transfer} from "./admin/tus";

// A small tus client. Persist only same-origin upload URLs, never file contents.
const root = document.querySelector<HTMLElement>("#upload");
if (root) {
  const album = root.querySelector<HTMLSelectElement>("#album")!;
  const input = root.querySelector<HTMLInputElement>("#files")!;
  const drop = root.querySelector<HTMLElement>("#drop")!;
  const list = root.querySelector<HTMLUListElement>("#uploads")!;
  const chunk = Number(root.dataset.chunkBytes);
  const csrf = root.dataset.csrf || "";
  type Work = {
    file: File; album: string; key: string; url: string | null;
    row: HTMLLIElement; progress: HTMLProgressElement; label: HTMLSpanElement;
    retry: HTMLButtonElement; cancel: HTMLButtonElement;
    abort: AbortController; cancelled: boolean; running: boolean; photo: string | null;
  };
  const waiting: Work[] = [];
  let active = 0;
  async function request(work: Work, method: string, url: string, headers: Record<string, string> = {}, body?: Blob) {
    return fetch(url, {
      method, headers: { "Tus-Resumable": "1.0.0", "X-CSRF-Token": csrf, ...headers },
      body, signal: work.abort.signal, credentials: "same-origin", redirect: "error",
    });
  }
  function requireSuccess(response: Response) {
    if (!response.ok) throw new Error(response.status === 422 ? "File rejected" : response.status === 413 ? "File or chunk is too large" : "Upload interrupted. Retry to resume.");
  }
  async function poll(work: Work) {
    while (!work.cancelled && work.photo) {
      const response = await request(work, "GET", `${work.photo}/status`);
      requireSuccess(response);
      const data: { status?: string } = await response.json();
      if (data.status === "ready") { work.label.textContent = "Ready"; return; }
      if (data.status === "failed") { work.label.textContent = "Processing failed"; return; }
      work.label.textContent = "Processing original";
      await new Promise<void>((resolve, reject) => {
        const stop = () => { clearTimeout(timer); reject(new DOMException("Cancelled", "AbortError")); };
        const timer = setTimeout(() => { work.abort.signal.removeEventListener("abort", stop); resolve(); }, 2000);
        work.abort.signal.addEventListener("abort", stop, { once: true });
      });
    }
  }
  async function upload(work: Work) {
    if (work.photo) return;
    work.photo = await transfer(work, chunk, csrf, offset => {
      work.progress.value = offset;
      work.label.textContent = `Uploading ${Math.round(offset * 100 / work.file.size)}%`;
    }, work.abort.signal);
    // Publication is complete; cancellation can no longer remove the photo.
    work.cancel.disabled = true;
    work.cancel.hidden = true;
  }

  function pump() {
    while (active < 4 && waiting.length) {
      const work = waiting.shift()!;
      if (work.cancelled || work.running) continue;
      active++; work.running = true; work.retry.hidden = true;
      upload(work).then(() => {
        // Processing polling does not occupy one of the four upload slots.
        void poll(work).catch(() => {
          if (!work.cancelled) { work.label.textContent = "Status unavailable. Retry to check again."; work.retry.hidden = false; }
        });
      }).catch((error: Error) => {
        if (!work.cancelled) { work.label.textContent = error.message || "Upload interrupted"; work.retry.hidden = false; }
      }).finally(() => { active--; work.running = false; pump(); });
    }
  }
  function add(files: FileList | File[]) {
    if (!album.value) return;
    for (const file of Array.from(files)) {
      if (!file.size) continue;
      const key = `yhotos-upload:${JSON.stringify([file.name, file.size, file.lastModified])}`;
      // A duplicate selection cannot start competing PATCH requests for the same fingerprint.
      if (Array.from(list.children).some(row => (row as HTMLElement).dataset.key === key)) continue;
      const row = document.createElement("li"); row.dataset.key = key;
      const name = document.createElement("span"); name.textContent = `${file.name}: `;
      const label = document.createElement("span"); label.textContent = "Waiting";
      const progress = document.createElement("progress"); progress.max = file.size; progress.value = 0;
      progress.setAttribute("aria-label", `Upload progress for ${file.name}`);
      const retry = document.createElement("button"); retry.textContent = "Retry"; retry.hidden = true;
      const cancel = document.createElement("button"); cancel.textContent = "Cancel";
      const work: Work = { file, album: album.value, key, url: safeUpload(stored(key)), row, progress, label, retry, cancel, abort: new AbortController(), cancelled: false, running: false, photo: null };
      retry.addEventListener("click", () => { if (work.running) return; work.abort.abort(); work.abort = new AbortController(); waiting.push(work); pump(); });
      cancel.addEventListener("click", () => {
        work.cancelled = true; work.abort.abort(); remember(key, null); retry.hidden = true; cancel.disabled = true;
        delete row.dataset.key;
        // Termination failed or was not answered: the upload may still exist, or the
        // final chunk may already have published it. Say so and let the admin retry.
        const unconfirmed = () => { label.textContent = "Cancel not confirmed. Try again."; cancel.disabled = false; };
        if (!work.url) { label.textContent = "Cancelled"; return; }
        // Only a confirmed termination is reported as cancelled: the final chunk may
        // already have published the photo even though its response never arrived.
        label.textContent = "Cancelling";
        void fetch(work.url, { method: "DELETE", credentials: "same-origin", redirect: "error", headers: { "Tus-Resumable": "1.0.0", "X-CSRF-Token": csrf } })
          .then(response => {
            if (response.status === 204 || response.status === 404) label.textContent = "Cancelled";
            else if (response.status === 409) label.textContent = "Already uploaded. Delete the photo from its album if it is not wanted.";
            else unconfirmed();
          })
          .catch(unconfirmed);
      });
      row.append(name, label, progress, retry, cancel); list.append(row); waiting.push(work);
    }
    pump();
  }
  input.addEventListener("change", () => { if (input.files) add(input.files); input.value = ""; });
  drop.addEventListener("dragover", event => event.preventDefault());
  drop.addEventListener("drop", event => { event.preventDefault(); if (event.dataTransfer) add(event.dataTransfer.files); });
  drop.addEventListener("keydown", event => { if (event.target === drop && (event.key === "Enter" || event.key === " ")) { event.preventDefault(); input.click(); } });
}
