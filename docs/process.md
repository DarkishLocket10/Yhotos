# Development process

## Roles

- **Claude (planning, review, merge).** Owns `docs/`, writes task briefs, dispatches agents, reads every diff and every review, requests fixes, merges. Sets reasoning effort per task.
- **Codex workers.** Implement one brief each in an isolated worktree and open a PR. Default `gpt-6.1-sol`. Never merge.
- **Codex reviewers.** `gpt-6-astra` reviews every PR against `scripts/review-prompt.md` (security, performance, tests, fidelity, scope). Output is posted as a PR comment.
- **Smaller models** for fully specified mechanical work: `gpt-6-luna` or `gpt-reserve` for fixtures, docs from code, lint passes, translation scaffolding, mechanical refactors with a precise spec. Claude Haiku/Sonnet subagents for the same class of work when run from Claude.
- **Yash.** Product decisions, anything needing a password or account, final sign-off before launch.

## Effort tiers

| Work | Model | Effort |
|---|---|---|
| Architecture, auth, access control, codec sandbox, pipeline, storage, anything in the threat model | gpt-6.1-sol | xhigh |
| Ordinary features, handlers, templates, migrations, migration tool | gpt-6.1-sol | high |
| Mechanical: fixtures, lint fixes, docs from code, scaffolding with precise spec | gpt-6-luna or gpt-reserve | medium |
| Review of any PR | gpt-6-astra | high (xhigh for threat-model paths) |

The brief states the model and effort; `scripts/agent-run.sh` reads them from the brief's header.

## Where agents run

- yesktop (WSL2, 30 cores, 47 GB): primary. Up to 4 workers at once. Repo at `~/work/Yhotos`.
- yac (M4, 24 GB): 1 worker, or 2 if nothing else heavy is running.
- `scripts/agent-run.sh` refuses to start if free memory on the machine is under 8 GB, and each worker is capped at 6 GB via the Codex sandbox limits plus `cargo` job limits (`-j 8`).
- tower never runs agents; it hosts the stack, Postgres, and fixtures.

## Task lifecycle

1. Claude writes `tasks/NNN-slug.md` from the template in `tasks/README.md`: goal, scope (files and crates), acceptance list, security notes citing threat-model sections, benchmark expectations, model and effort, dependencies on other tasks.
2. `scripts/agent-run.sh NNN [--host yesktop|local]` creates worktree `.worktrees/NNN` on branch `y/NNN-slug` from `origin/main`, runs `codex exec` with the brief, commits anything left uncommitted, pushes, opens a PR with the template filled from the agent's final message.
3. CI runs: fmt, clippy, tests against Postgres, cargo deny, cargo audit, gitleaks, CodeQL, fuzz smoke, benchmarks vs baseline, Lighthouse where applicable.
4. `scripts/agent-review.sh <pr>` runs the astra review in the worktree and posts it as a PR comment.
5. Claude reads the diff, CI, and the review. Fixes go back to the same worktree with `scripts/agent-run.sh NNN --resume "fix: ..."`. Loop until clean.
6. Claude squash-merges. Branch deleted. Worktree removed.
7. If the brief turns out wrong, Claude fixes the docs and brief first, then re-runs.

## Branch and merge rules

- `main` is protected. Everything lands by squash merge from a PR with green checks.
- One task, one branch, one PR. Tasks are sized to a few hundred lines of diff; bigger work is split.
- Dependent tasks wait for their dependency to merge; they are not stacked.

## Fixtures

Small fixtures (< 200 KB each) live in `fixtures/` in the repo: a color chart, a tiny HDR gain-map JPEG, a tiny HEIC, a tiny RAW from each supported make, a short video, an EXIF-heavy file, and malicious samples for fuzzing (bombs, polyglots, truncated files).

Large fixtures live on tower at `/mnt/user/data/yhotos-fixtures/` (a few hundred real originals chosen for variety plus full-size RAW/HEIC/HDR samples) and are synced to a worker machine with `scripts/fixtures-sync.sh` over Tailscale into `fixtures/large/` (git-ignored). Tests that need them skip when the directory is missing, and CI runs them only on the self-hosted runner.

## Secrets and accounts

No agent ever receives a secret. CI secrets are set by Yash in GitHub. Local runs use a throwaway Postgres in Docker with a generated password. The Piwigo migration runs with a read-only MariaDB role created for the purpose.

## Communication

PR descriptions are the record. Decisions that change scope or architecture go into `docs/` by Claude. Agent output that needs a human decision goes under "Out of scope, needs a decision" in the PR and Claude raises it with Yash only if it is a real blocker.

## Ephemeral tower runners (maintainer setup)

Only scheduled or manually dispatched **main** jobs in `nightly.yml` and
`fuzz.yml` may select `[self-hosted, tower]`. PR jobs, including `perf`-labelled
PRs and forks, use GitHub-hosted runners. Do not register a persistent runner
on the production host OS.

The maintainer provisions a disposable Linux VM on tower in a separate network
with no route to production, the LAN, the tailnet, or cloud metadata endpoints.
Inside it, build a reviewed, digest-pinned runner container with the GitHub
runner release verified against its published SHA256, the repository Rust
version, Node 22, Docker CLI, git, curl, jq, Python, the codec libraries listed in
`ci.yml`, and a non-root user with passwordless sudo for browser dependencies.
Use a dedicated Docker daemon **inside the disposable VM** for Compose and
Actions services. Never mount tower's Docker socket, production volumes, SSH
agent, home directory, or credentials. Mount only a reviewed copy of the fixture
collection at `/fixtures:ro`; it must contain `24mp.jpg`. This is a copy, never
the original library. Use tmpfs or disposable VM disks for work, data and caches.

From a trusted maintainer terminal, capture a short-lived registration token in shell memory with
`TOKEN="$(gh api --method POST repos/DarkishLocket10/Yhotos/actions/runners/registration-token
--jq .token)"`. Deliver it to the container over stdin or a memory-only secret
file, never in an image, checked-in env file, log or artifact. Register with
`./config.sh --url https://github.com/DarkishLocket10/Yhotos --token "$TOKEN"
--name "tower-ephemeral-$RUN_ID" --labels tower --ephemeral --unattended
--disableupdate`, unset the token, then execute `./run.sh`. GitHub adds the
`self-hosted` label automatically. Pin and update the runner image regularly
when using `--disableupdate`. Each runner accepts exactly one job; forward its
sanitized diagnostic logs and destroy the container and VM after completion or
a watchdog deadline. Provision separate instances for concurrent nightly jobs.
Use an external nightly launcher to create them just before the scheduled jobs;
no registration is performed by this task. Restrict outbound traffic to the
GitHub/Actions, crate, npm, browser and pinned image registries needed for CI.
