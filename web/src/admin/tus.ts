// Shared transfer state for batch uploads and replace-in-place. Never persist file bytes.
export type Transfer = {
  file: File; album: string; key: string; url: string | null; replace?: string;
};
export function stored(key: string): string | null {
  try { return localStorage.getItem(key); } catch { return null; }
}
export function remember(key: string, value: string | null) {
  try { if (value) localStorage.setItem(key, value); else localStorage.removeItem(key); } catch { /* The live island retains the URL when storage is unavailable. */ }
}
export function safeUpload(value: string | null): string | null {
  if (!value) return null;
  try {
    const url = new URL(value, location.origin);
    return url.origin === location.origin && /^\/admin\/upload\/[0-9a-f-]{36}$/.test(url.pathname) && !url.search && !url.hash ? url.pathname : null;
  } catch { return null; }
}
function encode(text: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(text)) binary += String.fromCharCode(byte);
  return btoa(binary);
}
function requireSuccess(response: Response) {
  if (!response.ok || response.redirected) throw new Error(response.status === 422 ? "File rejected" : response.status === 413 ? "File or chunk is too large" : "Upload interrupted. Retry to resume.");
}
function readOffset(response: Response, file: File): number {
  const text = response.headers.get("Upload-Offset");
  if (!text || !/^\d+$/.test(text)) throw new Error("Invalid upload offset");
  const offset = Number(text);
  if (!Number.isSafeInteger(offset) || offset < 0 || offset > file.size) throw new Error("Invalid upload offset");
  return offset;
}
function sameDestination(response: Response, work: Transfer): boolean {
  const albums = response.headers.get("Upload-Metadata")?.split(",").map(field => field.trim().split(" ")).filter(([key]) => key === "album") || [];
  return response.headers.get("Upload-Length") === String(work.file.size)
    && albums.length === 1 && albums[0].length === 2 && albums[0][1] === encode(work.album)
    && response.headers.get("Upload-Replace-Photo") === (work.replace ?? null);
}
// A failed request leaves work.url intact, including when the server stored the final chunk.
export async function transfer(work: Transfer, chunk: number, csrf: string, progress: (offset: number) => void, signal?: AbortSignal): Promise<string> {
  if (!Number.isSafeInteger(chunk) || chunk <= 0) throw new Error("Invalid upload chunk size");
  const request = (method: string, url: string, headers: Record<string, string> = {}, body?: Blob) => fetch(url, {
    method, headers: {"Tus-Resumable": "1.0.0", "X-CSRF-Token": csrf, ...headers},
    body, signal, credentials: "same-origin", redirect: "error",
  });
  const complete = (response: Response): string | null => {
    if (!response.headers.has("Upload-Result")) return null;
    const photo = response.headers.get("Location");
    if (!photo || !/^\/admin\/api\/photos\/[a-z0-9]{12}$/.test(photo)) throw new Error("Invalid upload response");
    progress(work.file.size);
    work.url = null;
    remember(work.key, null);
    return photo;
  };
  let offset = 0;
  if (work.url) {
    const response = await request("HEAD", work.url);
    if (response.status === 404 || response.status === 410) { remember(work.key, null); work.url = null; }
    else {
      requireSuccess(response);
      if (sameDestination(response, work)) offset = readOffset(response, work.file);
      else { remember(work.key, null); work.url = null; }
    }
  }
  if (!work.url) {
    const response = await request("POST", "/admin/upload", {
      "Upload-Length": String(work.file.size),
      "Upload-Metadata": `filename ${encode(work.file.name)},album ${encode(work.album)},filetype ${encode(work.file.type)}`,
      ...(work.replace ? {"Upload-Replace-Photo": work.replace} : {}),
    });
    requireSuccess(response);
    work.url = safeUpload(response.headers.get("Location"));
    if (!work.url) throw new Error("Invalid upload response");
    remember(work.key, work.url);
  }
  progress(offset);
  if (offset === work.file.size) {
    const response = await request("POST", `${work.url}/finish`);
    requireSuccess(response);
    const photo = complete(response);
    if (photo) return photo;
  }
  while (offset < work.file.size) {
    const end = Math.min(offset + chunk, work.file.size);
    const response = await request("PATCH", work.url, {
      "Upload-Offset": String(offset), "Content-Type": "application/offset+octet-stream",
    }, work.file.slice(offset, end));
    requireSuccess(response);
    const next = readOffset(response, work.file);
    if (next <= offset || next !== end) throw new Error("Invalid upload progress");
    offset = next;
    progress(offset);
    const photo = complete(response);
    if (photo) return photo;
  }
  throw new Error("Upload completion was interrupted. Retry to resume.");
}
