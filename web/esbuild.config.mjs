import { build } from "esbuild";

await build({
  absWorkingDir: import.meta.dirname,
  entryPoints: ["src/*.ts"],
  outdir: "../crates/yhotos-http/assets/js",
  bundle: true,
  format: "esm",
  platform: "browser",
  target: "es2022",
  minify: true,
  sourcemap: false,
});

// Independent worker bundle keeps worker-src restricted to 'self'.
await build({
  absWorkingDir: import.meta.dirname,
  entryPoints: {"map-worker": "node_modules/maplibre-gl/dist/maplibre-gl-worker.mjs"},
  outdir: "../crates/yhotos-http/assets/js",
  bundle: true, format: "esm", platform: "browser", target: "es2022", minify: true,
});
const {copyFile} = await import("node:fs/promises");
await copyFile(new URL("node_modules/maplibre-gl/dist/maplibre-gl.css", import.meta.url),
  new URL("../crates/yhotos-http/assets/js/maplibre.css", import.meta.url));
await copyFile(new URL("node_modules/maplibre-gl/LICENSE.txt", import.meta.url),
  new URL("../crates/yhotos-http/assets/js/maplibre-LICENSE.txt", import.meta.url));
