#!/usr/bin/env bash
# Exercise bootstrap safety with a fake Docker CLI; never touches a real volume.
set -euo pipefail
start_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/bin" "$work/one/deploy/dev" "$work/two/deploy/dev"
cp "$start_dir/start.sh" "$work/one/deploy/dev/start.sh"
cp "$start_dir/start.sh" "$work/two/deploy/dev/start.sh"
cat > "$work/bin/docker" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail
printf '%s ' "$COMPOSE_PROJECT_NAME" >> "$DOCKER_CALLS"
printf '%q ' "$@" >> "$DOCKER_CALLS"
printf '\n' >> "$DOCKER_CALLS"
if [[ " $* " == *' exec '* && "${DOCKER_FAIL:-}" == mismatch ]]; then
    printf 'password authentication failed\n' >&2
    exit 1
fi
if [[ " $* " == *' exec '* && "${DOCKER_FAIL:-}" == connection ]]; then
    printf 'connection refused\n' >&2
    exit 1
fi
DOCKER
chmod +x "$work/bin/docker"
export PATH="$work/bin:$PATH" DOCKER_CALLS="$work/calls"
for tree in one two; do
    bash "$work/$tree/deploy/dev/start.sh"
done
first="$(awk 'NR==1 {print $1}' "$work/calls")"
second="$(awk 'NR==3 {print $1}' "$work/calls")"
[[ "$first" =~ ^yhotos-[0-9a-f]{12}$ && "$second" =~ ^yhotos-[0-9a-f]{12}$ && "$first" != "$second" ]]
: > "$work/calls"
if DOCKER_FAIL=mismatch bash "$work/one/deploy/dev/start.sh" > "$work/out" 2>&1; then
    printf 'FAIL: credential mismatch succeeded\n' >&2
    exit 1
fi
rg -q 'credential mismatch. Data was preserved.' "$work/out"
rg -q "docker compose --project-name $first --env-file .* -f .* down --volumes" "$work/out"
if rg -q ' down ' "$work/calls"; then
    printf 'FAIL: bootstrap deleted a database\n' >&2
    exit 1
fi
: > "$work/calls"
if DOCKER_FAIL=connection bash "$work/one/deploy/dev/start.sh" > "$work/out" 2>&1; then
    printf 'FAIL: connection failure succeeded\n' >&2
    exit 1
fi
rg -q 'connection check failed' "$work/out"
if rg -q ' down ' "$work/calls"; then exit 1; fi
printf 'PASS: isolated worktree projects; credential and connection failures preserve volumes\n'
