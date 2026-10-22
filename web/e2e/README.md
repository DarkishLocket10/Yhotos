Run against the fixture server described in `deploy/dev/PAGES.md`:

```sh
npm --prefix web ci
npm --prefix web run check
cargo run -p yhotos-http --example pages_fixture
# In another terminal; optionally set CHROME_PATH to an installed Chromium.
npm --prefix web run e2e -- --trace on
```

`YHOTOS_BASE_URL` changes the server URL. Traces stay in the ignored
`deploy/dev/reports/playwright` directory. No browser test runs in ordinary CI;
the smoke is ready for the later nightly browser job.

Keyboard walkthrough: Tab to a grid photo, Enter to open, ArrowRight/ArrowLeft
to navigate, Tab and Shift+Tab through the viewer controls, Enter/Space to toggle
details or slideshow, Escape to close. Focus should return to the original grid
link and the complete album URL should be restored. Browser Back closes and
Forward reopens the current photo. With JavaScript disabled the grid link opens
the complete `/p/` page.

The native dialog is labelled by its photo title. Previous, Next, Close, Zoom,
photo details, downloads, share, slideshow, and fullscreen have descriptive
names. Toggles expose pressed/expanded state. Photo position and action outcomes
use polite live status regions. Verify those announcements with a screen reader;
a DOM assertion does not substitute for listening to them.

For the manual performance trace, start a browser Performance recording on the
warm album and open a grid photo. The `lightbox-first-paint` performance measure
spans click to the frame after the reused grid image paints; inspect its duration
against 50 ms. Inspect interaction timings against 100 ms and layout shift events
against zero. Run Lighthouse separately with the documented mobile and desktop
profiles and retain the reports in `deploy/dev/reports`. Navigation-only
Lighthouse scores do not establish INP.
