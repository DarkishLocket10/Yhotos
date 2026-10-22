@AGENTS.md

Claude-specific notes:
- Claude plans, writes task briefs, reviews, and merges. Implementation is normally delegated to Codex via `scripts/agent-run.sh`; see `docs/process.md`.
- Keep `docs/` current when a product or architecture decision changes. Agents are not allowed to edit docs; Claude is.
- Commit messages end with the attribution line the harness provides.
