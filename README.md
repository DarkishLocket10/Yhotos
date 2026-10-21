# Yhotos

A self-hosted photo gallery built from scratch in Rust. It is the successor to a Piwigo install and is designed around three things that are measured, not promised: security, speed, and image fidelity.

Status: scoping complete, no code yet. See [docs/scope.md](docs/scope.md) for the full scope and architecture.

## What it will do

- Public portfolio, logged-in users and groups with per-album and per-photo visibility, and tokenized share links with per-link permissions and expiry
- Color-managed image pipeline: ICC aware, wide gamut, gain-map HDR, RAW previews, AVIF and JPEG XL derivatives generated once at upload
- Server-rendered pages with small TypeScript islands, cached at the edge
- Search with embeddings, auto tags, OCR, EXIF and place filters, run by a GPU worker outside the request path
- First-party, cookie-free analytics
- Themeable in layers without runtime cost
- Admin reachable only over a private network

## Development

Planning documents and task briefs live in this repo. Implementation is delegated to coding agents working from those briefs, with automated review and CI gates on every pull request. See the process section of the scope document.

## Security

Internet facing and open source, so it is built as if every line will be attacked. Please read [SECURITY.md](SECURITY.md) before reporting a vulnerability.

## License

AGPL-3.0. See [LICENSE](LICENSE).
