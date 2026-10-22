# Codec host protocol and confinement

One JSON Job on stdin, one JSON Report on stdout. There are no library decoder
messages in public errors. Rejections exit 2, processing failures 3, CPU/wall
backstops 4. The worker must independently enforce its wall deadline. A small
supervisor never reads the job or invokes decoders: it passes stdin to a
sandboxed child, buffers at most 1 MiB of report output until success, and
discards child stderr. Signals (including allocator SIGABRT under RLIMIT_AS)
and decode errors become exit 3, exactly `image processing failed` on stderr
and empty stdout. CPU/wall timeouts retain exit 4 and `codec timeout`; rejected
jobs retain exit 2. Core dumps are disabled. On Linux, parent-death SIGKILL also
kills the decoder if the worker terminates its supervisor. On macOS the worker
must terminate the process group. Encoder failures can leave partial outputs; a failed job's
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
An additional Linux-only test tightens RLIMIT_AS to 64 MiB and requests a 128 MiB
Rust allocation. It proves the direct sandboxed child aborts with SIGABRT, then
checks that the supervised host returns exit 3, one generic stderr line and no
report or files. It exercises the real allocator failure path, not a simulated
decode error.
The syscall list must be audited with `strace -f -c` inside that image whenever
native codec versions change. This workstation has no running Docker daemon;
Linux execution and the strace inventory have not been verified locally.


HDR outputs use the existing encoder `hdr` flag. The report adds
`hdr_skipped_reason` (a fixed enum or null) and `skipped_outputs` (name/reason).
Only HDR outputs may be skipped; sources without maps use `no_gain_map` for
those requested outputs and retain a null photo-level reason. Invalid map
metadata preserves all SDR outputs and reports the reason. An unavailable HDR
encoder skips just that format and its partial file is removed. The parent
matches every output/skip against its trusted plan before publishing HDR rows.
The default plan reserves nine optional HDR outputs at medium/large/xlarge,
including xlarge JPEG, alongside the existing 23 required SDR outputs. The 2x
SDR variants remain in that plan; HDR edges are configured separately.
Matching SDR/HDR outputs share the resized working image and an immutable
encoded-base snapshot (16 MiB each, 64 MiB total). Snapshot identity and encoder
policy are checked inside the codec; cache misses use normal encoding. The
report's `timings.hdr_base_reuses` records successful reuses.
