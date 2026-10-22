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
  function stored(key: string): string | null {
    try { return localStorage.getItem(key); } catch { return null; }
  }
  function remember(key: string, value: string | null) {
    try { if (value) localStorage.setItem(key, value); else localStorage.removeItem(key); } catch { /* Upload still works without persistence. */ }
  }
  function safeUpload(value: string | null): string | null {
    if (!value) return null;
    try {
      const url = new URL(value, location.origin);
      return url.origin === location.origin && /^\/admin\/upload\/[0-9a-f-]{36}$/.test(url.pathname) && !url.search && !url.hash ? url.pathname : null;
    } catch { return null; }
  }
  function safePhoto(value: string | null): string {
    if (!value || !/^\/admin\/api\/photos\/[a-z0-9]{12}$/.test(value)) throw new Error("Invalid upload response");
    return value;
  }
  function encode(text: string): string {
    const bytes = new TextEncoder().encode(text);
    let binary = "";
    for (const byte of bytes) binary += String.fromCharCode(byte);
    return btoa(binary);
  }
  async function request(work: Work, method: string, url: string, headers: Record<string, string> = {}, body?: Blob) {
    return fetch(url, {
      method, headers: { "Tus-Resumable": "1.0.0", "X-CSRF-Token": csrf, ...headers },
      body, signal: work.abort.signal, credentials: "same-origin", redirect: "error",
    });
  }
  function requireSuccess(response: Response) {
    if (!response.ok) throw new Error(response.status === 422 ? "File rejected" : response.status === 413 ? "File or chunk is too large" : "Upload interrupted. Retry to resume.");
  }
  function readOffset(response: Response, file: File): number {
    const text = response.headers.get("Upload-Offset");
    if (!text || !/^\d+$/.test(text)) throw new Error("Invalid upload offset");
    const offset = Number(text);
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > file.size) throw new Error("Invalid upload offset");
    return offset;
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
    let offset = 0;
    if (work.url) {
      const response = await request(work, "HEAD", work.url);
      if (response.status === 404 || response.status === 410) { remember(work.key, null); work.url = null; }
      else { requireSuccess(response); offset = readOffset(response, work.file); }
    }
    if (!work.url) {
      const response = await request(work, "POST", "/admin/upload", {
        "Upload-Length": String(work.file.size),
        "Upload-Metadata": `filename ${encode(work.file.name)},album ${encode(work.album)},filetype ${encode(work.file.type)}`,
      });
      requireSuccess(response);
      work.url = safeUpload(response.headers.get("Location"));
      if (!work.url) throw new Error("Invalid upload response");
      remember(work.key, work.url);
    }
    while (offset < work.file.size) {
      const end = Math.min(offset + chunk, work.file.size);
      work.label.textContent = `Uploading ${Math.round(offset * 100 / work.file.size)}%`;
      const response = await request(work, "PATCH", work.url, {
        "Upload-Offset": String(offset), "Content-Type": "application/offset+octet-stream",
      }, work.file.slice(offset, end));
      requireSuccess(response);
      const next = readOffset(response, work.file);
      if (next <= offset || next !== end) throw new Error("Invalid upload progress");
      offset = next;
      work.progress.value = offset;
      if (response.headers.has("Upload-Result")) {
        work.photo = safePhoto(response.headers.get("Location"));
        work.url = null;
        remember(work.key, null);
      }
    }
    if (!work.photo) throw new Error("Upload completion was interrupted. Retry to resume.");
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
        label.textContent = "Cancelled"; delete row.dataset.key;
        if (work.url) {
          void fetch(work.url, { method: "DELETE", credentials: "same-origin", redirect: "error", headers: { "Tus-Resumable": "1.0.0", "X-CSRF-Token": csrf } })
            .then(response => { if (!response.ok && response.status !== 404) label.textContent = "Cancelled. Temporary upload will expire automatically."; })
            .catch(() => { label.textContent = "Cancelled. Temporary upload will expire automatically."; });
        }
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
