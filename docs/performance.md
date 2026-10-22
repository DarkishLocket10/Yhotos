# Performance

"Fastest it can possibly be" is operationalized as: budgets, measured on every change, with regressions blocking merges. Budgets start at the values below and tighten once real numbers exist; they never loosen without a written reason in this file.

## 1. Budgets

### Serving (origin, cache miss, tower hardware)
| Metric | Budget |
|---|---|
| Album page HTML (50 photos) TTFB | p50 < 20 ms, p99 < 50 ms |
| Photo page TTFB | p50 < 15 ms, p99 < 40 ms |
| Derivative file response (hash lookup + access check + send) | p99 < 5 ms before first byte |
| Search (text + semantic, 10k photos) | p99 < 50 ms |
| Like/flag API | p99 < 10 ms |
| Concurrency | 100 concurrent album views stay under 1 CPU core and 200 MB RSS |
| Server startup | < 1 s to ready |

### Page weight (public album page, 50 photos, first viewport)
| Asset | Budget (compressed) |
|---|---|
| HTML | < 30 KB |
| CSS total | < 30 KB |
| JS total loaded on first view | < 50 KB |
| Fonts | system stack by default; any web font < 40 KB subsetted |
| Images above the fold | correct `sizes` so a 390 px phone never downloads more than the 640w variant |

### Web Vitals (lab, Lighthouse CI, Moto G4 / slow 4G profile and desktop)
| Metric | Budget |
|---|---|
| LCP | < 1.5 s mobile, < 1.0 s desktop |
| CLS | 0 |
| INP | < 100 ms |
| Lighthouse performance score | >= 95 |

### Pipeline (tower, 16 cores, per 30 MP JPEG)
| Step | Budget |
|---|---|
| Hash + copy to originals | < 150 ms |
| All derivatives (13 files, AVIF + JXL + JPEG) | < 4 s wall clock per photo, 8 photos in parallel |
| Full migration of 4,925 photos | < 6 h wall clock including ML |
| ML per photo (embed + faces + OCR + tags on GTX 1080 Ti) | < 400 ms |

### Fidelity gates (not speed, but enforced in the same harness)
| Check | Threshold |
|---|---|
| SSIM of each derivative vs reference resize of the original | >= 0.98 for large and up, >= 0.97 below |
| Mean delta E 2000 on the color chart fixture through the pipeline | < 1.0, max < 2.5 |
| ICC profile preserved in AVIF/JXL | always |
| Gain map present in HDR variants when source has one | always |

## 2. How it is measured

### Micro benchmarks: `criterion`
In `crates/yhotos-bench/benches/`. One benchmark per hot path: access function, derivative route (in-process with a stub storage), search parser, thumbhash encode, safe_join, token verify, jobs claim query (against a real Postgres). CI runs them on every PR and compares to the `main` baseline stored as an artifact; a regression over 5 % on any benchmark fails the `perf` check with the numbers in the PR.

### Load tests: `oha`
`scripts/bench.sh` starts the compose stack with the fixture library, warms it, and runs `oha` scenarios: album page, photo page, derivative, search, mixed. Reports p50/p99 and RSS. Runs nightly on `main` and on PRs labeled `perf`.

### Lighthouse CI
`.github/workflows/lighthouse.yml` builds the stack, serves it, runs `lhci` against the home, an album, a photo page, and the search page, on mobile and desktop profiles, and fails under the budgets above. Reports attached to the PR.

### Real user monitoring
The public pages report Web Vitals (LCP, CLS, INP, TTFB) via a tiny beacon into the first-party `events` table, no third party. The admin analytics dashboard shows p75 by device class and country. This is the number that matters; lab numbers exist to catch regressions before users do.

### Pipeline benchmarks
`yhotos-bench` includes a `pipeline` bench over the fixture set with per-step timings; CI runs a reduced set, the nightly job runs the full set on tower via the self-hosted runner.

## 3. Rules that keep the budgets

- No work in the request path that is not needed to produce the response. No synchronous image work, ever.
- Every response that can be cached at the edge is: derivatives and originals immutable for a year, public HTML for 60 s with stale-while-revalidate, purge by cache tag on change.
- Templates compile to code. No runtime template parsing.
- HTML carries `width`/`height` on every image and a placeholder color, so nothing shifts.
- One database round trip per page where possible: album page = one query for the album, one for its visible photos with their derivative hashes (joined), one for the session. Measured by a test that counts queries.
- Connection pool sized to cores; statements prepared; indexes listed in `docs/architecture.md` exist from the first migration.
- Streaming everywhere files are involved: uploads, derivatives, originals, zips. Never read a whole file into memory except in the sandboxed codec child.
- Binary built with `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` in release; `mimalloc` as the global allocator (benchmarked against the system allocator before adoption).
- JS is optional: every public page works without it; islands enhance.

## 4. Changing a budget

Open a PR editing this file with the measurement that motivates it and the reason the current value is wrong. Tightening needs no discussion. Loosening needs the reason recorded here.
