# Codec host protocol and confinement

One JSON Job on stdin, one JSON Report on stdout. There are no library decoder
messages in public errors. Rejections exit 2, processing failures 3, CPU/wall
backstops 4. The parent must independently enforce its wall deadline and handle
signals/crashes. Encoder failures can leave partial outputs; a failed job's
private output directory must be discarded by the worker.

Before reading stdin the host re-execs with a fixed environment (no caller PATH,
HOME, dynamic loader or libvips settings), installs resource defaults and timeout
handlers, and applies seccomp on Linux. Job limits may tighten the startup caps.
The macOS process runtime's `__CF_USER_TEXT_ENCODING` is the only extra variable
retained by std's exec implementation. No libvips operation runs before cleanup.

Defaults: 3 GiB address space, 120 CPU seconds, 125 wall seconds, 2 GiB per
output file, 64 descriptors, two libvips threads. macOS rejects RLIMIT_AS, so the
address-space cap is Linux-only. macOS applies the other limits and NPROC=0;
production confinement is Linux. Linux NPROC counts threads, so NPROC=0 cannot
be combined with libvips pthreads for non-root workers: seccomp instead permits
only the exact pthread clone flags and denies process/namespace creation.

The worker supplies trusted input and output directory paths. The host
canonicalizes them, requires a regular input and an output directory, and
accepts only unique single-component output names of at most 128 bytes.
Traversal, absolute paths, existing entries, symlinks and hard links are rejected
before decoding. Use a private directory per job with no other writers.

Seccomp is a default-deny syscall allow list, including exact clone flag checks.
`tgkill` may address only this host's thread group; `prlimit64` accepts only PID 0.
Same-user workers and other hosts cannot be signaled or have their limits changed.
Socket, connect, exec, ptrace, mount and user namespace creation are not allowed.
clone3 returns ENOSYS so glibc can use the inspected clone fallback.
BPF cannot inspect path pointers safely: **production must mount the input
read-only and out_dir as its only writable mount**, with no other original
files visible, a read-only runtime filesystem, no network, dropped capabilities,
and no-new-privileges. The host does not create that mount namespace itself.
System libraries and codec plugins must be visible read-only. The worker must
also verify the output hashes and sizes before moving them to storage.

Linux validation:

```
docker build --iidfile /tmp/codec-image -f deploy/dev/Dockerfile.codec .
docker run --rm --network=none -v "$PWD:/workspace" "$(cat /tmp/codec-image)"
```

The pinned base is the amd64 Rust 1.99.1 trixie manifest. CI runs the exact built
image ID. The image build installs the pinned repository Rust toolchain and
fetches locked Cargo dependencies with networking available; runtime tests use
`--offline --frozen` under `--network=none`.
Linux-only tests exercise denied cross-process signals/resource queries, network
access and an actual CPU
limit using repeated resizes/ThumbHash work; debug flags do not exist in release.
The syscall list must be audited with `strace -f -c` inside that image whenever
native codec versions change. This workstation has no running Docker daemon;
Linux execution and the strace inventory have not been verified locally.
