---
id: 024
slug: public-face-browsing
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [008, 010, 011, 013, 014]
crates: [yhotos-http, yhotos-db]
---

# 024 Public "more of this face" browsing (opt-in per person, no names)

## Goal

Any visitor (anonymous, account, or share-link) can click a face on a photo they are allowed to see and land on "More of this face": a grid of the other photos *they are allowed to see* that contain the same person. Only people the admin has explicitly opted in are clickable. Names, face embeddings, cluster sizes and hidden people never reach a non-admin response. The admin keeps the full named experience on the admin listener (task 014).

## Read first

- `AGENTS.md`
- `docs/scope.md` 5.5 (faces), `docs/threat-model.md` 4.3, 4.4, 4.8
- Briefs 008 (file serving and visibility), 010 (share-link scoping), 011 (public pages), 013 (lightbox islands), 014 (faces, persons, `/admin/people`, codec-host face crops)

## Scope

Areas: `crates/yhotos-db/` (migration, person/face queries), `crates/yhotos-http/src/pages/` (`/f/{public_id}` page, photo page face data), `crates/yhotos-http/src/api/` (face data in the photo JSON the lightbox uses), the files route for public face crops, `/admin/people` toggle (from 014), templates, `web/src/` (face chips and hover boxes in the photo page and lightbox), plus supporting files.

## Design

### Data
- Migration: `persons.public_browse boolean NOT NULL DEFAULT false` (opt-in; existing and new people start off), and `persons.public_id text UNIQUE` holding an opaque random identifier (at least 72 random bits, base32 like short ids, generated with the OS CSPRNG when the admin first enables browsing). Never expose `persons.id` or any face id publicly.
- `/admin/people`: per-person toggle "Allow visitors to browse this face", and "Reset public link" (new `public_id`, old URLs 404). Hidden persons cannot be enabled; hiding a person also disables browsing. Every change is audited and purges the cache tags below.

### Public face data on a photo
- For a photo the viewer can see, the server includes only faces whose person has `public_browse = true` and `hidden = false`: normalized bbox, a face-crop URL, and the person's `public_id`. No name, no person count, no other faces. Faces of people not opted in produce nothing (no boxes, no chips).
- Photo page (no JS): a "People in this photo" row of face chips (face crop images) linking to `/f/{public_id}`, each with `alt`/`aria-label` "More photos of this person". Absent when the list is empty.
- Lightbox and photo page islands: the same chips in the info panel, plus hover/tap face boxes drawn over the image from the bboxes, each a real link with the same label, focusable by keyboard, hidden while zoomed or in slideshow, respecting `prefers-reduced-motion`. Boxes must track the contained image size (no letterboxing offset) and rotation.
- Face crops for visitors are served through the files route with exactly the same visibility checks as the photo they come from (anonymous, account grants, share-link scope). A crop is reachable only via a photo the viewer can see; a crop URL alone grants nothing. Crops come from the codec host (014) and are cached as private derivatives.

### "More of this face" page `/f/{public_id}`
- Title "More of this face", no name anywhere (title, OG tags, alt text, JSON, headers). `noindex, nofollow` meta and header; not in any sitemap; no Open Graph image of the face.
- Lists photos the viewer can see (same SQL visibility as album pages, including share-link scoping and account grants) that contain a face assigned to that person, newest taken first, 60 per page with the album grid and lightbox.
- Unknown id, person not opted in, person hidden, or zero visible photos: the same 404 as any missing page, byte-identical, so ids and opt-in status cannot be probed. Rate-limited like other public pages.
- Share-link visitors see only photos inside their unlocked links' scope.

### Cache
- Public responses carry tags `person:<public_id>` and `visibility`; toggling, resetting, hiding, merging or reassigning faces purges `person:*` for the affected people plus the photo tags of photos whose face list changed. Responses that depend on a session or share cookie are `private, no-store` as elsewhere.

### Tests
- Opt-in: a person with `public_browse = false` produces no face data in the photo page, lightbox JSON or crops; enabling exposes it; disabling or hiding removes it again (and purges).
- No names: generated test that renders every public route and JSON for a fixture with a named, opted-in person and asserts the name string never appears; `persons.id` and face ids never appear.
- Visibility: a restricted photo containing the person is absent from `/f/` for anonymous visitors, present for a granted account, present for a share link whose scope covers it and absent for one that does not; the page count and pagination reflect only visible photos.
- 404 indistinguishability across unknown id, opted-out, hidden, reset (old id), and zero-visible cases.
- Face crop route: denied for a photo the viewer cannot see, allowed when they can; same response as other missing files when denied.
- Islands: face boxes positioned from bboxes on contained images, links keyboard-reachable, chips render without JS on the photo page.
- Admin toggle and reset audited; purge calls made (mock `CachePurger`).

## Acceptance

- [ ] All tests above pass; `scripts/check.sh` passes.
- [ ] Screenshots of the photo page chips, lightbox boxes and a `/f/` page in the PR (or listed as pending for the staging walkthrough if a browser is unavailable).

## Security notes

- 4.8 (revised): face data reaches non-admin viewers only for opted-in people, only as bboxes, crops and an opaque id, only on photos they can already see. No names, embeddings, counts, similarity search, or face upload search for non-admins.
- `public_id` is random and resettable; never derived from `persons.id`.
- No new dependencies expected.

## Not in this task

- Public face search by name or by uploading a face; letting visitors name or correct faces; face data in analytics beyond ordinary page views.
