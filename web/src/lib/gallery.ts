import type {VideoData} from "./video.js";
export interface Sources { all: string }
export interface Photo {
  id: string; title: string; description: string;
  image: { src: string; jpeg: Sources; avif: Sources; jxl: Sources; hdr_jpeg: Sources; hdr_avif: Sources; hdr_jxl: Sources; width: number; height: number };
  caps: { download: boolean; like: boolean; flag: boolean };
  zoom_width: number;
  video?: VideoData;
  original: string | null; downloads: { href: string; label: string }[]; details: [string, string][];
  state?: { liked: boolean; flagged: boolean; like_count?: number };
}
export interface Gallery { album: string; page: number; sort: string; next_page: number | null; photos: Photo[] }
/** The next cursor is supplied by the server, never inferred from visible photo count. */
export class PageCursor {
  private pending = false;
  constructor(public next: number | null) {}
  begin(): number | null { if (this.pending || this.next === null) return null; this.pending = true; return this.next; }
  finish(next: number | null): void { this.next = next; this.pending = false; }
  fail(): void { this.pending = false; }
}
export function mergePhotos(photos: Photo[], incoming: Photo[]): void {
  const ids = new Set(photos.map(p => p.id));
  for (const photo of incoming) if (!ids.has(photo.id)) { photos.push(photo); ids.add(photo.id); }
}
/** Require deliberate horizontal travel and velocity; diagonal and held drags stay put. */
export function swipeDirection(dx: number, dy: number, elapsed: number): -1 | 0 | 1 {
  return elapsed > 0 && elapsed <= 700 && Math.abs(dx) >= 48 && Math.abs(dx) > Math.abs(dy) * 1.5 && Math.abs(dx) / elapsed >= .3 ? (dx < 0 ? 1 : -1) : 0;
}
export function parseGallery(value: unknown): Gallery {
  const data = value as Gallery;
  if (!data || typeof data.album !== "string" || !Number.isInteger(data.page) || data.page < 1 || !Array.isArray(data.photos) || data.photos.length > 60 || !["taken", "posted", "name", "manual"].includes(data.sort) || !(data.next_page === null || (Number.isInteger(data.next_page) && data.next_page > data.page))) throw new Error("Invalid gallery");
  for (const p of data.photos) if (!p || !/^[a-z2-7]{12}$/.test(p.id) || typeof p.title !== "string" || !p.image || !p.caps) throw new Error("Invalid photo");
  return data;
}
export function hydrateGrid(value: unknown, images: Map<string, Photo["image"]>): Gallery {
  const data = value as Gallery;
  if (!data || !Array.isArray(data.photos) || data.photos.length > 60) throw new Error("Invalid gallery");
  for (const photo of data.photos) {
    const image = images.get(photo.id); if (!image) throw new Error("Missing grid image");
    photo.image = image;
  }
  return parseGallery(data);
}
