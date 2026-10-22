/** Bounded LPQA/DCT ThumbHash decoder, matching evanw/thumbhash's reference. */
export function decodeThumbhash(hash: Uint8Array): { w: number; h: number; rgba: Uint8ClampedArray } {
  if (hash.length < 5 || hash.length > 64) throw new Error("Invalid placeholder");
  const h24 = hash[0] | hash[1] << 8 | hash[2] << 16;
  const h16 = hash[3] | hash[4] << 8;
  const alpha = !!(h24 & 0x800000), landscape = !!(h16 & 0x8000);
  const short = h16 & 7, long = alpha ? 5 : 7;
  if (!short) throw new Error("Invalid placeholder");
  const lx = landscape ? long : short, ly = landscape ? short : long;
  const ratio = lx / ly;
  const w = Math.round(ratio > 1 ? 32 : 32 * ratio), h = Math.round(ratio > 1 ? 32 / ratio : 32);
  let index = 0;
  const start = alpha ? 6 : 5;
  const channel = (nx: number, ny: number, dc: number, scale: number) => {
    const terms: [number, number, number][] = [];
    for (let y = 0; y < ny; y++) for (let x = y ? 0 : 1; x * ny < nx * (ny - y); x++) {
      const offset = start + (index >> 1);
      if (offset >= hash.length) throw new Error("Invalid placeholder");
      const coefficient = (((hash[offset] >> ((index++ & 1) * 4)) & 15) / 7.5 - 1) * scale;
      terms.push([x, y, coefficient]);
    }
    return (fx: number[], fy: number[]) => terms.reduce((sum, [x, y, coefficient]) => sum + coefficient * fx[x] * fy[y] * 2, dc);
  };
  const l = channel(Math.max(3, lx), Math.max(3, ly), (h24 & 63) / 63, ((h24 >> 18) & 31) / 31);
  const p = channel(3, 3, ((h24 >> 6) & 63) / 31.5 - 1, ((h16 >> 3) & 63) / 63 * 1.25);
  const q = channel(3, 3, ((h24 >> 12) & 63) / 31.5 - 1, ((h16 >> 9) & 63) / 63 * 1.25);
  const a = alpha ? channel(5, 5, (hash[5] & 15) / 15, (hash[5] >> 4) / 15) : () => 1;
  const cosines = (size: number) => Array.from({ length: size }, (_, v) => Array.from({ length: 7 }, (_, c) => Math.cos(Math.PI / size * (v + .5) * c)));
  const xs = cosines(w), ys = cosines(h), rgba = new Uint8ClampedArray(w * h * 4);
  let offset = 0;
  for (const fy of ys) for (const fx of xs) {
    const luminance = l(fx, fy), blue = luminance - 2 / 3 * p(fx, fy), chroma = q(fx, fy), red = (3 * luminance - blue + chroma) / 2;
    for (const value of [red, red - chroma, blue, a(fx, fy)]) rgba[offset++] = Math.floor(Math.max(0, Math.min(1, value)) * 255);
  }
  return { w, h, rgba };
}

if (typeof document !== "undefined" && "IntersectionObserver" in globalThis) {
  const observer = new IntersectionObserver(entries => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      const img = entry.target as HTMLImageElement;
      observer.unobserve(img);
      if (img.complete && img.naturalWidth > 0) continue;
      try {
        const hash = img.dataset.thumbhash;
        if (!hash || hash.length > 88) continue;
        const image = decodeThumbhash(Uint8Array.from(atob(hash), c => c.charCodeAt(0)));
        const canvas = document.createElement("canvas");
        canvas.width = image.w; canvas.height = image.h;
        const context = canvas.getContext("2d");
        if (!context) continue;
        context.putImageData(new ImageData(image.rgba, image.w, image.h), 0, 0);
        img.style.backgroundImage = `url(${canvas.toDataURL("image/png")})`;
        img.style.backgroundSize = "cover";
      } catch { /* Corrupt optional placeholders never prevent image loading. */ }
    }
  }, { rootMargin: "200px" });
  document.querySelectorAll<HTMLImageElement>('img[loading="lazy"][data-thumbhash]').forEach(img => observer.observe(img));
}
