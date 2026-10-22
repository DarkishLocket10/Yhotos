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
