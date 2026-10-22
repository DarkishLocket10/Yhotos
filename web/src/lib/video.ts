import {safeMediaUrl} from "./media.js";
export interface VideoData { av1: string; h264: string; duration_ms: number; scrub: string; vtt: string }
/** The sprite and VTT share 100 evenly spaced cells, clamped at the final frame. */
export function scrubCell(seconds: number, durationMs: number): {x: number; y: number} {
  const index = Math.max(0, Math.min(99, Math.floor(seconds * 100000 / durationMs)));
  return {x: -(index % 10) * 160, y: -Math.floor(index / 10) * 160};
}
export function enhanceVideo(video: HTMLVideoElement, scrub: string, durationMs: number, signal: AbortSignal): void {
  const url = safeMediaUrl(scrub);
  if (!url || !Number.isFinite(durationMs) || durationMs <= 0 || durationMs > 1800000) return;
  const panel = document.createElement("label"); panel.className = "video-scrub"; panel.textContent = "Preview and seek video";
  const slider = document.createElement("input"); slider.type = "range"; slider.min = "0"; slider.max = String(durationMs / 1000); slider.step = ".01"; slider.value = "0";
  const preview = document.createElement("span"); preview.className = "video-preview"; preview.hidden = true; preview.style.backgroundImage = `url("${url}")`;
  const update = (seconds: number) => { const cell = scrubCell(seconds, durationMs); preview.style.backgroundPosition = `${cell.x}px ${cell.y}px`; preview.hidden = false; };
  slider.addEventListener("input", () => { const seconds = Number(slider.value); update(seconds); video.currentTime = seconds; }, {signal});
  slider.addEventListener("pointermove", event => { const box = slider.getBoundingClientRect(); if (box.width > 0) update(Math.max(0, Math.min(1, (event.clientX-box.left)/box.width))*durationMs/1000); }, {signal});
  slider.addEventListener("pointerleave", () => { preview.hidden = true; }, {signal});
  slider.addEventListener("blur", () => { preview.hidden = true; }, {signal});
  video.addEventListener("timeupdate", () => { slider.value = String(video.currentTime); }, {signal});
  panel.append(slider, preview); video.after(panel);
  signal.addEventListener("abort", () => { video.pause(); panel.remove(); }, {once: true});
}
export function createVideo(data: VideoData, poster: string, width: number, height: number, signal: AbortSignal): HTMLDivElement | null {
  const av1 = safeMediaUrl(data.av1), h264 = safeMediaUrl(data.h264), image = safeMediaUrl(poster), vtt = safeMediaUrl(data.vtt);
  if (!av1 || !h264 || !image || !vtt) return null;
  const wrapper = document.createElement("div"); wrapper.className = "video-player";
  const video = document.createElement("video"); video.controls = true; video.preload = "metadata"; video.playsInline = true; video.poster = image; video.width = width; video.height = height;
  for (const [src, type] of [[av1, 'video/mp4; codecs="av01"'], [h264, 'video/mp4; codecs="avc1.640028"']]) { const source = document.createElement("source"); source.src = src; source.type = type; video.append(source); }
  const track = document.createElement("track"); track.kind = "metadata"; track.src = vtt; track.label = "Scrub previews"; video.append(track); wrapper.append(video);
  enhanceVideo(video, data.scrub, data.duration_ms, signal);
  signal.addEventListener("abort", () => { video.pause(); video.removeAttribute("src"); video.replaceChildren(); video.load(); wrapper.remove(); }, {once: true});
  return wrapper;
}
