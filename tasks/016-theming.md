---
id: 016
slug: theming
model: gpt-6.1-sol
effort: high
review_effort: high
depends_on: [011, 012]
crates: [yhotos-http, yhotos-db]
---

# 016 Theming: tokens, options, custom CSS, and the theme editor

## Goal

The four theming layers from `docs/architecture.md` section 10 exist end to end. An admin edits design tokens and layout options live in a theme editor (admin listener), previews the result against real pages, and publishes; the public site serves the result as hashed, immutable CSS with zero runtime cost. Custom CSS and a sanitized head-HTML snippet are the escape hatch, contained by CSP. The default theme is polished enough to ship.

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.8
- `docs/architecture.md` 9, 10
- `docs/threat-model.md` 4.4 (XSS, CSP), 4.7 (theme as backdoor)
- `docs/performance.md` 1 (CSS budget 30 KB compressed)
- Brief 011 (templates, `app.css`, tokens on `:root`)

## Scope

Areas: `crates/yhotos-http/src/theme/` (new), admin routes under `/admin/theme` (feature `admin`), `crates/yhotos-http/templates/default/` (consume tokens and options), `crates/yhotos-http/assets/css/`, `web/src/theme-editor.ts`, `crates/yhotos-db/src/settings.rs` (typed accessors), plus the always-in-scope supporting files.

## Design

### Layer 1: tokens
A typed `ThemeTokens` struct (serde, with defaults and validation): colors (background, surface, text, muted, accent, border, focus, each with a dark-mode counterpart), typography (font stack chosen from a fixed list of system stacks plus an optional self-hosted font family uploaded through the admin, see below), base size, scale ratio, line height, spacing scale, radii, grid gap, target row height for the justified grid, lightbox backdrop opacity, motion (duration, easing, reduced-motion behaviour). Every value is validated by type and range (colors as `#rrggbb` or `oklch()` only, lengths as numbers with a unit from an allow list) so no arbitrary CSS can enter through tokens. Stored in `settings.theme_tokens`.

Rendered to `/theme.{hash}.css` as `:root { --… }` plus a `@media (prefers-color-scheme: dark)` block and a `[data-theme=dark]` override for a user toggle. The hash is BLAKE3 of the rendered bytes; the CSS is rendered once on publish and cached in memory (and on disk under the data dir) so serving is a static file. `Cache-Control: public, max-age=31536000, immutable`.

### Layer 2: options
`ThemeOptions`: grid style (justified, masonry, uniform square), photos per page (24 to 120), show captions on grid (none, hover, always), show EXIF panel by default, show download button (all, accounts only, none), album card style, home layout (album grid, featured album hero plus grid), footer text (plain text, length-capped), social links (validated https URLs, fixed set of known sites), dark mode default (system, light, dark), show map and calendar links. Templates read these through the page context; changing them busts the HTML cache tags.

### Layer 3: templates
The default theme templates are cleaned up to read every visual decision from tokens and options; no hard-coded colors or sizes remain in templates or `app.css` (a test greps `app.css` for raw hex colors outside the token defaults block). Additional compiled themes are directories under `templates/<name>/` selected by the `theme` Cargo feature or `YHOTOS_THEME` at build time. This task ships only `default`; document how to add another.

### Layer 4: custom CSS and head HTML
- `settings.custom_css` (max 64 KB) served as `/custom.{hash}.css` after it passes a CSS tokenizer check (`cssparser`): reject `@import`, `url()` with any non-relative or non-`/i/`/`/static/` target, `expression(`, `-moz-binding`, `behavior:`, and `</style` sequences. CSP already blocks remote loads; this is defense in depth and gives the admin a clear error.
- `settings.custom_head_html` (max 8 KB) sanitized with `ammonia` to an allow list: `meta` (name, content, property), `link` (rel in an allow list, href same-origin only). No script, style, iframe, or event attributes. Rendered inside `<head>` with nothing else.
- Self-hosted font: admin uploads a WOFF2 file (max 400 KB, magic-byte checked, stored content-addressed via the store) and names a family; it is served from `/static/fonts/{hash}.woff2` with `font-display: swap` and included in `theme.css`.

### Theme editor (admin)
`/admin/theme`: a form for tokens and options with live preview in an iframe of the admin-listener rendering of a chosen public page (home, album, photo), using a draft token set held server-side (draft row in `settings.theme_draft`) and a `?theme=draft` parameter honored only on the admin listener. Color inputs, sliders for numeric ranges, a contrast checker that shows WCAG ratios for text on background and accent and warns below 4.5:1, Publish and Revert buttons, version history (last 20 published token/option sets with timestamps, one-click restore). Island `theme-editor.ts`, no framework.

### Default theme quality bar
The shipped default looks like a modern photography portfolio: generous whitespace, images first, restrained chrome, excellent dark mode, a clear focus ring, smooth but brief motion that respects reduced motion. Include before/after screenshots of home, album, photo (desktop and mobile, light and dark) in the PR.

### Tests
- Token validation: every field rejects out-of-range and wrong-type values; a fuzz target for the token JSON parser.
- Rendered `theme.css` snapshot for defaults; hash changes iff content changes.
- Custom CSS checker rejects each forbidden construct; sanitized head HTML strips scripts and event handlers; CSP header still has no `unsafe-inline`.
- `?theme=draft` has no effect on the public listener (test).
- Options change the rendered HTML as specified (snapshot per option).
- CSS budget: `app.css` + `theme.css` under 30 KB compressed.

## Acceptance

- [ ] All tests above pass; Lighthouse on album page still >= 95, CLS 0.
- [ ] Screenshots in the PR as described.
- [ ] `scripts/check.sh` passes.

## Security notes

- 4.7: themes are compiled; runtime customization is limited to validated tokens, sanitized HTML, and checked CSS under CSP. Map each to a test.
- The draft preview exists only on the admin listener.
- Allowed new dependencies: `cssparser`, `ammonia` (if not present), `palette` or hand-written color math for contrast (hand-written preferred).

## Performance

- Zero runtime cost on public requests: theme files are static, hashed, cached at the edge.

## Not in this task

- Per-album themes, user-selectable themes for visitors (beyond dark/light toggle).
