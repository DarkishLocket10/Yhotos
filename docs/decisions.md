# Maintainer decisions

Decisions the maintainer made after a brief or design document was written. Each one is
binding: a change that implements an entry here is in scope, and briefs or docs that still say
otherwise are out of date. Reviewers must not flag an implementation of these decisions as a
scope violation. Newest last.

| Date | Area | Decision | Where it applies |
|---|---|---|---|
| 2026-10-22 | Albums | Album visibility is `public` or `restricted` only; no `inherit`. Photos may inherit their album's access. | 020 |
| 2026-10-22 | Likes | Like counts appear only on the admin listener (`/admin/likes`); the public listener never treats anyone as admin. | 013, 020 |
| 2026-10-22 | Faces | Face models are OpenCV Zoo YuNet (MIT) and SFace (Apache-2.0, 128-d), not InsightFace buffalo_l (non-commercial weights). | 014, docs/scope.md, docs/architecture.md |
| 2026-10-22 | Faces | Face thumbnails are cropped by the sandboxed codec host into a private cached variant, served only on the admin listener (until 024). | 014 |
| 2026-10-22 | Faces | Public "more of this face": opt-in per person, chips and hover boxes, never names. | 024 |
| 2026-10-22 | Video | The video sandbox uses Landlock EXECUTE on exactly the configured static ffmpeg and ffprobe, seccomp allowing threads only (no fork, no sockets), and direct exec with process-group kill on timeout. Seccomp cannot match execve paths. | 021, docs/threat-model.md |
| 2026-10-22 | Migration | The Piwigo importer reads files only through a read-only `SourceTree` API in yhotos-storage (beneath-root, no symlinks, regular files only), documented in architecture and threat model. | 022 |
| 2026-10-22 | HDR | Compressed (`brob`) JPEG XL gain maps are not decompressed; the photo keeps SDR output with a recorded reason. | 019 |
| 2026-10-22 | Benchmarks | Mutation benchmarks timing real writes are owned by 018, not 013. | 013, 018 |
| 2026-10-22 | Deploy | 023 depends on 014 and 018. | 023 |
| 2026-10-22 | Search | Task 014's Docker image build, offline (no egress) run and combined GPU timing are verified during staging on tower's GTX 1080 Ti (task 023), not in the 014 PR. 014 merges once its code review passes on everything else. | 014, 023 |
| 2026-10-22 | Search | ML preprocessing converts every image to sRGB from its ICC profile before inference. | 014 |
| 2026-10-24 | Video | The one-minute 4K iPhone upload, transcode and desktop/mobile lightbox playback check, including timings, is verified in staging during task 023. Keep task 021's acceptance item pending until that staging check. | 021, 023 |
| 2026-10-24 | ZIP | The maintainer-approved STORE streaming budget is under 20 percent of one CPU core per stream, with throughput at least 200 MB/s. This supersedes the older 10 percent CPU limit in the task brief; enforce the 20 percent gate. | 021 |
