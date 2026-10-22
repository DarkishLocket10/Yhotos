import { build } from 'esbuild';
await build({ entryPoints: ['src/lib/gallery.ts', 'src/lib/cache.ts', 'src/thumbhash.ts'], outdir: 'dist/test', outbase: 'src', bundle: true, platform: 'node', format: 'esm', target: 'node22' });
