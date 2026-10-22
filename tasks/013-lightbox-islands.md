---
id: 013
slug: lightbox-islands
model: gpt-6.1-sol
effort: high
review_effort: high
depends_on: [010, 011]
crates: [yhotos-http]
---

# 013 Lightbox, prefetch, view transitions, like and flag islands

## Goal

The gallery feels instant: clicking a photo opens a lightbox without a page load, with keyboard, swipe, pinch zoom to full resolution, EXIF panel, download menu, like and flag buttons (for principals allowed to), slideshow, and deep-linkable URLs (`/p/{short_id}` updates in the address bar). Navigation between pages uses prefetch on hover and the View Transitions API. All of it is progressive enhancement over the task 011 HTML and stays under the JS budget.

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.1, 5.2
- `docs/architecture.md` 8 (`/api/v1/...`), 9
- `docs/performance.md` 1 (JS budget 50 KB compressed, INP < 100 ms, CLS 0)
- `docs/threat-model.md` 4.4 (CSRF on API, XSS)
- Brief 011 (markup and JSON island data), 009 (session), 010 (share-link caps)

## Scope

`web/src/{lightbox,prefetch,transitions,likes,thumbhash}.ts`, `web/src/lib/` helpers, `crates/yhotos-http/src/api/` (`/api/v1/albums/{id}/photos?page=`, `/api/v1/photos/{short_id}`, `POST/DELETE /api/v1/photos/{short_id}/like`, `POST /api/v1/photos/{short_id}/flag`), template changes in `album.html`/`photo.html` to add the hooks (`data-` attributes and the JSON island block), `crates/yhotos-http/templates/default/partials/lightbox.html` (the dialog markup rendered server-side, hidden until used).

## Design

### Lightbox (`lightbox.ts`, the biggest island)
- Opens from the grid: `click` on a photo link with modifier-free left click is intercepted; the `<dialog>` element from the partial is filled from the JSON island data (already in the page for the current page of photos) and shown; `history.pushState` to `/p/{short_id}`; `popstate` closes or navigates.
- Image: `<picture>` sized to the viewport using the same source sets; starts from the already-loaded grid derivative (no flash), then upgrades; HDR source when supported. Zoom: pointer and wheel, double-tap, pinch; beyond `xlarge` it loads the original lazily if downloadable by this principal, else stops at `xlarge`.
- Navigation: arrow keys, swipe (pointer events, velocity threshold), on-screen arrows; preloads the next and previous `large` derivatives; at the end of the loaded page, fetches the next page from `/api/v1/albums/{id}/photos`.
- Panels: info (title, date, camera, lens, settings, place), download menu (sizes + original per caps), like (heart showing the viewer's own state; the public listener never treats anyone as admin, so like counts live on the admin listener in `/admin/likes`, task 020), flag (one tap, confirm, shows "flagged"), share (copies the `/p/` URL), slideshow (play/pause, interval 4 s, pauses on interaction), fullscreen.
- Accessibility: focus trap, `aria-label`s, `Esc` closes, reduced-motion respected, `inert` on the page behind.
- Works with no JS: the `/p/` page is the fallback, so nothing is lost.

### Prefetch (`prefetch.ts`)
On `pointerenter`/`touchstart`/`focus` of internal links, `fetch` the HTML with `priority: low` after 65 ms hover (cancel on leave), store in a small `Map` (max 10), and on click swap the document with the View Transitions API (`transitions.ts`), falling back to normal navigation. Respect `Save-Data` and `prefers-reduced-data`.

### Likes and flags (`likes.ts`)
Buttons on the photo page and in the lightbox; `fetch` with `credentials: same-origin`, header `X-Requested-With: yhotos` (the CSRF check for the JSON API), optimistic UI with rollback. Hidden entirely for anonymous principals (the server does not render the buttons).

### API (`crates/yhotos-http/src/api/`)
JSON, same visibility rules as pages, `Cache-Control: private, no-store` when a principal is present, `public, max-age=60` otherwise. Errors as `{ "error": "code" }`. CSRF: require `Sec-Fetch-Site: same-origin` (or absent for old browsers plus the custom header) on mutations. Rate limits: `cheap` for reads, `expensive` for mutations (likes/flags per IP per minute 60).

### Thumbhash (`thumbhash.ts`)
Decodes `data-thumbhash` into a tiny canvas/data URI for below-the-fold images on first visibility (IntersectionObserver), replacing the solid color placeholder. ~120 lines, from the reference implementation (MIT), credited in a header comment.

### Budget and build
Each island is its own entry point; `album.html` loads `lightbox`, `prefetch`, `transitions`, `thumbhash`, `likes` only when the principal can like. Measured total for the album page under 50 KB compressed (test in CI: build and gzip-size the files). No third-party packages at runtime.

### Tests
- Rust: API visibility (restricted photo via API → 404 byte-identical), CSRF rejection without the header/`Sec-Fetch-Site`, like/unlike idempotency, flag once per principal per photo (second flag is a no-op 200), rate limit.
- TypeScript: unit tests for the grid-page cursor logic, swipe velocity math, prefetch cache eviction, and thumbhash decode against vectors, with Node's built-in test runner (`node --test`) and `tsc --noEmit` type-check added to `web/package.json` scripts and the CI `check` job (edit `ci.yml`: this task may push workflows, so run it on a machine whose `gh` token has the `workflow` scope).
- Browser: a Playwright smoke (`web/e2e/`, run manually and in the nightly workflow later): open album, click photo, arrow right, Esc, URL restored; works with JS disabled (page navigates to `/p/`).

## Acceptance

- [ ] All tests above pass; JS size test enforced in CI.
- [ ] Lighthouse on the album page still >= 95 with islands loaded; INP < 100 ms in a manual trace; report in the PR.
- [ ] Keyboard-only walkthrough and screen-reader labels described in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.4: API mutations require same-origin proof; no `innerHTML` with untrusted strings (use `textContent` and `createElement`); the JSON island is parsed with `JSON.parse` from a `type="application/json"` script, never evaluated.
- Downloads of originals through the lightbox respect caps (server enforces; client only hides).

## Performance

- No layout shift when the dialog opens (dialog is fixed-position, body scroll locked with `scrollbar-gutter: stable`).
- Image upgrade uses `decode()` before swap to avoid flashes.
- Measure and report: time from click to first paint of the lightbox image (should be under 50 ms since the grid derivative is reused).

## Not in this task

- Map, calendar, search UI (014, 015).
- Admin UI.
- Service worker / offline.
