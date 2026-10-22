/** Only local gallery media may reach media URL attributes. URL parsing also
 * normalizes absolute and protocol-relative input before the origin check. */
export function safeMediaUrl(value: string): string | null {
  try {
    if (/[\u0000-\u001f\u007f]/.test(value)) return null;
    // Reject traversal even when URL normalization would keep it inside /i/.
    const path = decodeURIComponent(value.split(/[?#]/, 1)[0]);
    if (path.includes("\\") || path.split("/").some(part => part === "." || part === "..")) return null;
    const url = new URL(value, location.origin);
    if (url.origin !== location.origin) return null;
    if (!url.pathname.startsWith("/i/") && !url.pathname.startsWith("/o/") && !url.pathname.startsWith("/v/")) return null;
    return url.pathname + url.search;
  } catch { return null; }
}
/** Rendered width of a photo drawn with `object-fit: contain` inside a box.
 * Letterboxing is excluded, so portrait photos in wide boxes get their real width. */
export function containedWidth(boxWidth: number, boxHeight: number, imageWidth: number, imageHeight: number): number {
  if (!(boxWidth > 0)) return 1;
  if (!(boxHeight > 0) || !(imageWidth > 0) || !(imageHeight > 0)) return Math.max(1, boxWidth);
  return Math.max(1, Math.min(boxWidth, boxHeight * imageWidth / imageHeight));
}
/** Validate every width candidate, then choose the smallest sufficient derivative
 * (or the largest available). Invalid candidates never enter a srcset. */
export function zoomSourceSet(value: string, pixels: number): string | null {
  const candidates: { url: string; width: number }[] = [];
  for (const candidate of value.split(",")) {
    const parts = candidate.trim().match(/^(\S+)\s+([1-9]\d*)w$/);
    if (!parts) continue;
    const url = safeMediaUrl(parts[1]), width = Number(parts[2]);
    if (url && Number.isSafeInteger(width)) candidates.push({ url, width });
  }
  candidates.sort((a, b) => a.width - b.width);
  const selected = candidates.find(c => c.width >= pixels) || candidates.at(-1);
  return selected ? `${selected.url} ${selected.width}w` : null;
}

/** Only the most recently requested decode may replace the displayed media. */
export class DecodedMedia {
  key = "";
  private version = 0;
  reset(): void { this.key = ""; this.version++; }
  async show(key: string, decode: () => Promise<void>, swap: () => void): Promise<void> {
    if (key === this.key) return;
    this.key = key;
    const version = ++this.version;
    try {
      await decode();
      if (version === this.version) swap();
    } catch (error) {
      if (version === this.version) { this.key = ""; throw error; }
    }
  }
}

/** A photo-state refresh requested at `requestedAt` may predate a like or flag on
 * the same photo when that mutation is still pending or settled after the request. */
export function staleRefresh(pending: ReadonlySet<string>, settled: ReadonlyMap<string, number>, id: string, requestedAt: number): boolean {
  return pending.has(id) || (settled.get(id) ?? -Infinity) >= requestedAt;
}
