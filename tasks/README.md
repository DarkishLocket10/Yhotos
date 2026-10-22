# Task briefs

One file per task, `NNN-slug.md`. Numbers are allocation order, not execution order; the `depends_on` line says what must be merged first. A brief is complete when an agent with no other context could do the work and know when it is done.

Header (machine-read by `scripts/agent-run.sh`):

```
---
id: 003
slug: db-schema
model: gpt-6.1-sol
effort: high
review_effort: high
depends_on: [001]
crates: [yhotos-db, yhotos-core]
---
```

Body sections, in order:

1. **Goal.** One paragraph. What exists after this task that did not before.
2. **Read first.** Docs and sections to read before starting.
3. **Scope.** Files and crates that may change. Anything else is out of scope.
4. **Design.** Enough detail that there is one reasonable implementation. Signatures, table shapes, protocols, names.
5. **Acceptance.** A checklist. Each item is testable. The PR must show how each is proven.
6. **Security notes.** Threat-model sections that apply and what the agent must do about each.
7. **Performance.** Benchmarks to add or run, with the budget from `docs/performance.md`.
8. **Not in this task.** Things that look related and must be left alone.

Status is tracked by the PR, not in the brief. A brief with a merged PR is history and is not edited.
