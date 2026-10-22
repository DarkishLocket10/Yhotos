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

## Video mode

`--video` accepts `{input,out_dir,settings,probe_only}`. Settings include absolute
`ffmpeg`/`ffprobe` paths, `max_duration` (1..1800 seconds), `timeout` (1..7200
seconds, default 1800), and `threads` (1..16). The supervisor bounds report
capture to 1 MiB and invokes a fresh child for each probe/transcode/poster/sprite
operation, with a single deadline for all video operations. The child closes
inherited non-stdio descriptors and sets resource limits, parent-death SIGKILL,
a process group, no-new-privileges, Landlock and seccomp before directly execing
the parser. Unsupported Landlock fails closed with an operator-facing reason.

Landlock grants input read-only, output read/write, and execution only on the
configured parser binaries. The video parsers are pinned static ELF binaries. The host checks their bounded
ELF program headers for PT_INTERP and refuses a dynamic loader before parsing
media. No shared-library directories or loader receive access. This matters
because Landlock requires EXECUTE on PT_INTERP too; static binaries preserve
the exact two-file execution policy. The filesystem policy never grants
directory execution.
Video seccomp permits exec but denies fork/vfork, sockets, ptrace and mount;
clone3 returns ENOSYS and clone accepts the inspected pthread flags only.
Image mode retains its existing no-exec policy. Timeouts kill the parser's
process group, and parent-death SIGKILL covers worker/supervisor cancellation.

The probe accepts one H.264/HEVC video stream and known AAC/PCM/ALAC audio,
at most four streams, 8K dimensions and configured duration. Every native
invocation pins MOV demuxing and codec/protocol allow lists. Outputs are H.264
High CRF21 (1080-pixel long edge), SVT-AV1 CRF30 (3840x2160 maximum, including portrait rotation), AAC160k,
faststart MP4, JPEG poster at 10 percent and a 10x10 160-pixel scrub sprite.
The worker sends the poster through the normal AVIF/JXL/JPEG image path.
The authorized WebVTT route derives the 100 cell intervals from the committed
duration, so no request parses video. HDR inputs become SDR BT.709 via zscale
and Hable tone mapping; HDR delivery and HLS are outside this task.

Local video tests use `deploy/dev/video-tools.sh`, which extracts the digest-pinned
ffmpeg 9.0.2 static build into `target/video-tools` without installing host
packages. `scripts/check.sh` runs this bootstrap automatically. Test requests
can explicitly select another directory through `YHOTOS_TEST_VIDEO_BIN_DIR`;
production paths still come only from validated worker configuration. The codec
Dockerfile copies the same pinned static binaries to `/usr/bin/ffmpeg` and
`/usr/bin/ffprobe`, so codec CI exercises the production sandbox.
