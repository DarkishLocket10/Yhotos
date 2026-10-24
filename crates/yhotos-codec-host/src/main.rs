#![forbid(unsafe_code)]
//! One bounded JSON job per process. Linux confines filesystem access before decoding.
#[cfg(target_os = "linux")]
mod filesystem;
#[cfg(target_os = "linux")]
mod seccomp;
use base64::Engine;
use nix::sys::resource::{Resource, setrlimit};
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Instant,
};
use yhotos_codec::{CodecError, Encode, EncodeOpts, Limits, ResizeSpec};
#[cfg(feature = "test-hooks")]
static DECODE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const JOB_BYTES: u64 = 1 << 20;
const CACHE_BYTES: usize = 64 << 20;
const CHILD_ARGUMENT: &str = "--sandboxed-job";
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HostLimits {
    max_pixels: u64,
    max_bytes: u64,
    max_dim: u32,
    max_memory: u64,
    cpu_seconds: u64,
    max_file_size: u64,
    wall_seconds: u32,
}
impl Default for HostLimits {
    fn default() -> Self {
        let l = Limits::default();
        Self {
            max_pixels: l.max_pixels,
            max_bytes: l.max_bytes,
            max_dim: l.max_dim,
            max_memory: 3 << 30,
            cpu_seconds: 120,
            max_file_size: 2 << 30,
            wall_seconds: 125,
        }
    }
}
impl HostLimits {
    fn codec(&self) -> Limits {
        Limits {
            max_pixels: self.max_pixels,
            max_bytes: self.max_bytes,
            max_dim: self.max_dim,
        }
    }
    fn apply(&self) -> Result<(), Failure> {
        let cap = Self::default();
        if self.max_pixels == 0
            || self.max_pixels > cap.max_pixels
            || self.max_bytes == 0
            || self.max_bytes > cap.max_bytes
            || self.max_dim == 0
            || self.max_dim > cap.max_dim
            || self.max_memory == 0
            || self.max_memory > cap.max_memory
            || self.cpu_seconds == 0
            || self.cpu_seconds > cap.cpu_seconds
            || self.max_file_size == 0
            || self.max_file_size > cap.max_file_size
            || self.wall_seconds == 0
            || self.wall_seconds > cap.wall_seconds
        {
            return Err(Failure(2, "invalid limits"));
        }
        // macOS does not enforce RLIMIT_AS reliably; Linux is the production sandbox.
        #[cfg(target_os = "linux")]
        setrlimit(Resource::RLIMIT_AS, self.max_memory, self.max_memory)
            .map_err(|_| Failure(3, "sandbox unavailable"))?;
        setrlimit(Resource::RLIMIT_CORE, 0, 0).map_err(|_| Failure(3, "sandbox unavailable"))?;
        setrlimit(Resource::RLIMIT_CPU, self.cpu_seconds, self.cpu_seconds + 1)
            .map_err(|_| Failure(3, "sandbox unavailable"))?;
        setrlimit(
            Resource::RLIMIT_FSIZE,
            self.max_file_size,
            self.max_file_size,
        )
        .map_err(|_| Failure(3, "sandbox unavailable"))?;
        setrlimit(Resource::RLIMIT_NOFILE, 64, 64)
            .map_err(|_| Failure(3, "sandbox unavailable"))?;
        // Linux counts threads toward NPROC, so limit process creation via seccomp
        // while retaining the two libvips threads. NPROC=0 would prohibit them too.
        yhotos_codec::sandbox::process_limit().map_err(|_| Failure(3, "sandbox unavailable"))?;
        yhotos_codec::sandbox::timeouts(self.wall_seconds)
            .map_err(|_| Failure(3, "sandbox unavailable"))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    input: PathBuf,
    out_dir: PathBuf,
    #[serde(default)]
    limits: HostLimits,
    outputs: Vec<Output>,
    #[serde(default)]
    crop: Option<[f32; 4]>,
    #[serde(default)]
    want: Vec<Want>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    name: String,
    resize: ResizeSpec,
    encode: Encode,
    #[serde(default = "yes")]
    keep_icc: bool,
}
fn yes() -> bool {
    true
}
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Want {
    Identity,
    Metadata,
    Thumbhash,
    DominantColors,
}
#[derive(Serialize)]
struct OutputReport {
    name: String,
    bytes: u64,
    width: u32,
    height: u32,
    blake3: String,
    millis: u128,
}
#[derive(Serialize)]
struct SkippedOutput {
    name: String,
    reason: yhotos_codec::HdrSkippedReason,
}
#[derive(Serialize)]
struct Report {
    hdr_skipped_reason: Option<yhotos_codec::HdrSkippedReason>,
    skipped_outputs: Vec<SkippedOutput>,
    identity: Option<yhotos_codec::Identity>,
    metadata: Option<yhotos_codec::Metadata>,
    thumbhash: Option<String>,
    dominant_colors: Option<Vec<[u8; 3]>>,
    outputs: Vec<OutputReport>,
    timings: Timings,
}
#[derive(Serialize)]
#[allow(clippy::struct_field_names)] // Units are explicit in the JSON report.
struct Timings {
    #[cfg(feature = "test-hooks")]
    decode_calls: usize,
    identify_ms: u128,
    decode_ms: u128,
    hdr_base_reuses: usize,
    total_ms: u128,
}
struct Failure(u8, &'static str);
/// Maps an identification error to the host exit contract. Corrupt headers and
/// permanently unsupported content (for example animated JXL) are definitive
/// rejections (2). IO and native-host failures say nothing about the validity of
/// an otherwise resumable upload, so they stay retryable through `From`.
fn identify_failure(error: CodecError) -> Failure {
    if matches!(error, CodecError::Decode | CodecError::Unsupported) {
        Failure(2, "image rejected")
    } else {
        Failure::from(error)
    }
}
impl From<CodecError> for Failure {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::BadMagic
            | CodecError::Mismatch
            | CodecError::Limits
            | CodecError::Options => Self(2, "image rejected"),
            _ => Self(3, "image processing failed"),
        }
    }
}
// A new exec clears env before any libvips initialization, without unsafe Rust
// environment mutation. No PATH/HOME/LD_*/VIPS_* supplied by the caller survives.
fn clean_environment() -> Result<u8, Failure> {
    let concurrency = std::env::var("VIPS_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|n| (1..=64).contains(n))
        .unwrap_or(2);
    let concurrency_text = concurrency.to_string();
    #[allow(unused_mut)]
    let mut expected = vec![
        ("VIPS_CONCURRENCY", concurrency_text.as_str()),
        ("VIPS_WARNING", "0"),
        ("VIPS_DISC_THRESHOLD", "3g"),
        ("LC_ALL", "C"),
    ];
    #[cfg(feature = "test-hooks")]
    let test_delay = std::env::var("YHOTOS_TEST_DELAY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms <= 60_000)
        .map(|n| n.to_string());
    #[cfg(feature = "test-hooks")]
    if let Some(delay) = test_delay.as_deref() {
        expected.push(("YHOTOS_TEST_DELAY_MS", delay));
    }
    if std::env::vars_os()
        .filter(|(name, _)| name != "__CF_USER_TEXT_ENCODING")
        .count()
        == expected.len()
        && expected
            .iter()
            .all(|(k, v)| std::env::var(k).is_ok_and(|s| s == *v))
    {
        return Ok(concurrency);
    }
    let executable = std::env::current_exe().map_err(|_| Failure(3, "startup failed"))?;
    let error = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env_clear()
        .envs(expected)
        .exec();
    let _ = error;
    Err(Failure(3, "startup failed"))
}
fn paths(job: &Job) -> Result<(PathBuf, Vec<PathBuf>), Failure> {
    let input = job
        .input
        .canonicalize()
        .map_err(|_| Failure(3, "input unavailable"))?;
    if !input.is_file() {
        return Err(Failure(2, "invalid input"));
    }
    let dir = job
        .out_dir
        .canonicalize()
        .map_err(|_| Failure(3, "output directory unavailable"))?;
    if !dir.is_dir() || job.outputs.len() > 32 {
        return Err(Failure(2, "invalid output directory"));
    }
    let mut seen = HashSet::new();
    let mut outputs = Vec::new();
    for output in &job.outputs {
        let name = Path::new(&output.name);
        if output.name.is_empty()
            || output.name.len() > 128
            || name.components().count() != 1
            || !matches!(name.components().next(), Some(Component::Normal(_)))
            || !seen.insert(&output.name)
        {
            return Err(Failure(2, "invalid output name"));
        }
        let path = dir.join(name);
        // Reject existing entries, including dangling symlinks and hard links.
        // The worker supplies a private directory; production gives only this
        // directory a writable mount, avoiding races with untrusted writers.
        if path.symlink_metadata().is_ok() || !path.starts_with(&dir) || path == input {
            return Err(Failure(2, "invalid output name"));
        }
        outputs.push(path);
    }
    Ok((input, outputs))
}
#[allow(clippy::too_many_lines)] // The single-job protocol is kept in one sequential flow.
fn run() -> Result<(), Failure> {
    let concurrency = clean_environment()?;
    #[cfg(target_os = "linux")]
    {
        let parent = std::env::args()
            .nth(2)
            .and_then(|p| p.parse().ok())
            .ok_or(Failure(3, "startup failed"))?;
        yhotos_codec::sandbox::parent_lifetime(parent).map_err(|_| Failure(3, "startup failed"))?;
    }
    HostLimits::default().apply()?;
    #[cfg(target_os = "linux")]
    seccomp::apply().map_err(|()| Failure(3, "sandbox unavailable"))?;
    #[cfg(all(target_os = "linux", debug_assertions))]
    if std::env::args_os().any(|a| a == "--self-test-network") {
        return match std::net::UdpSocket::bind("127.0.0.1:0") {
            Err(e) if e.raw_os_error() == Some(nix::libc::EPERM) => Ok(()),
            _ => Err(Failure(3, "network sandbox failed")),
        };
    }
    #[cfg(all(target_os = "linux", debug_assertions))]
    if let Some(pid) =
        std::env::args().find_map(|a| a.strip_prefix("--self-test-process=").map(str::to_owned))
    {
        let pid = pid.parse().map_err(|_| Failure(3, "invalid test target"))?;
        return yhotos_codec::sandbox::check_process_isolation(pid)
            .map_err(|_| Failure(3, "process sandbox failed"));
    }
    #[cfg(feature = "test-hooks")]
    if let Ok(delay) = std::env::var("YHOTOS_TEST_DELAY_MS")
        && let Ok(ms) = delay.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms.min(60_000)));
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(JOB_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure(2, "invalid job"))?;
    if bytes.len() as u64 > JOB_BYTES {
        return Err(Failure(2, "job too large"));
    }
    let job: Job = serde_json::from_slice(&bytes).map_err(|_| Failure(2, "invalid job"))?;
    #[cfg(all(target_os = "linux", debug_assertions))]
    let memory_probe = std::env::args_os().any(|a| a == "--self-test-memory");
    job.limits.apply()?; // Job may only tighten startup caps.
    #[cfg(all(target_os = "linux", debug_assertions))]
    if memory_probe {
        // 128 MiB succeeds without the test's 64 MiB RLIMIT_AS. Under the
        // limit this takes Rust's real allocation-failure/abort path, including
        // its allocator diagnostic, rather than returning a simulated error.
        let allocation = vec![0u8; std::hint::black_box(128 << 20)];
        std::hint::black_box(allocation);
        return Err(Failure(2, "memory limit not enforced"));
    }
    let (input, paths) = paths(&job)?;
    yhotos_codec::sandbox::initialize(concurrency)?;
    #[cfg(target_os = "linux")]
    {
        filesystem::restrict(
            &input,
            &job.out_dir
                .canonicalize()
                .map_err(|_| Failure(3, "output directory unavailable"))?,
        )?;
        #[cfg(debug_assertions)]
        if std::env::args_os().any(|a| a == "--self-test-filesystem") {
            return filesystem::probe(&input, &job.out_dir);
        }
    }
    let start = Instant::now();
    let id = yhotos_codec::identify(&input, &job.limits.codec()).map_err(identify_failure)?;
    let identify_ms = start.elapsed().as_millis();
    if job.outputs.is_empty()
        && job
            .want
            .iter()
            .all(|want| matches!(want, Want::Identity | Want::Metadata))
    {
        let report = Report {
            hdr_skipped_reason: None,
            skipped_outputs: Vec::new(),
            identity: job.want.contains(&Want::Identity).then_some(id),
            metadata: if job.want.contains(&Want::Metadata) {
                Some(yhotos_codec::metadata(&input)?)
            } else {
                None
            },
            thumbhash: None,
            dominant_colors: None,
            outputs: Vec::new(),
            timings: Timings {
                #[cfg(feature = "test-hooks")]
                decode_calls: DECODE_CALLS.load(std::sync::atomic::Ordering::Relaxed),
                identify_ms,
                decode_ms: 0,
                hdr_base_reuses: 0,
                total_ms: start.elapsed().as_millis(),
            },
        };
        serde_json::to_writer(std::io::stdout().lock(), &report)
            .map_err(|_| Failure(3, "report failed"))?;
        return Ok(());
    }
    let decode_start = Instant::now();
    #[cfg(feature = "test-hooks")]
    DECODE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let working = yhotos_codec::decode(&input, &id, &job.limits.codec())?;
    let decode_ms = decode_start.elapsed().as_millis();
    let working = if let Some(bbox) = job.crop {
        if !matches!(id.kind, yhotos_codec::Kind::Jpeg)
            || job.outputs.len() != 1
            || !job.outputs.first().is_some_and(|o| {
                o.resize.long_edge == 256
                    && !o.keep_icc
                    && matches!(o.encode, Encode::Jpeg { hdr: false, .. })
            })
        {
            return Err(Failure(2, "invalid crop request"));
        }
        yhotos_codec::crop_face(&working, bbox)?
    } else {
        working
    };
    #[cfg(feature = "test-hooks")]
    if std::env::args_os().any(|a| a == "--self-test-concurrency") {
        // Query libvips after decode too, so lazy native initialization cannot
        // silently restore the default. The supervisor preserves these bytes.
        writeln!(
            std::io::stdout().lock(),
            "{}",
            yhotos_codec::sandbox::initialize(concurrency)?
        )
        .map_err(|_| Failure(3, "report failed"))?;
        return Ok(());
    }
    #[cfg(all(target_os = "linux", debug_assertions))]
    if std::env::args_os().any(|a| a == "--self-test-timeout") {
        loop {
            let resized = yhotos_codec::resize(
                &working,
                ResizeSpec {
                    long_edge: 192,
                    square: false,
                },
            )?;
            drop(std::hint::black_box(yhotos_codec::thumbhash(&resized)?));
        }
    }

    let mut outputs = Vec::new();
    let mut skipped_outputs = Vec::new();
    let mut resized_images: Vec<(ResizeSpec, yhotos_codec::Working)> = Vec::new();
    let mut bases: Vec<yhotos_codec::SdrBase> = Vec::new();
    let mut cached_bytes = 0_usize;
    let mut hdr_base_reuses = 0;
    for (output, path) in job.outputs.iter().zip(paths) {
        let t = Instant::now();
        if output.encode.hdr() && !working.has_gain_map() {
            skipped_outputs.push(SkippedOutput {
                name: output.name.clone(),
                reason: working
                    .hdr_skipped_reason()
                    .unwrap_or(yhotos_codec::HdrSkippedReason::NoGainMap),
            });
            continue;
        }
        // Reserve atomically, including case-folded aliases on macOS. The private
        // worker directory prevents other processes replacing this entry.
        let reservation = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| Failure(2, "invalid output name"))?;
        drop(reservation);
        let index = if let Some(index) = resized_images
            .iter()
            .position(|(spec, _)| *spec == output.resize)
        {
            index
        } else {
            resized_images.push((
                output.resize,
                yhotos_codec::resize(&working, output.resize)?,
            ));
            resized_images.len() - 1
        };
        let resized = &resized_images
            .get(index)
            .ok_or(Failure(3, "resize unavailable"))?
            .1;
        let opts = EncodeOpts {
            keep_icc: output.keep_icc,
            strip_metadata: true,
        };
        let cached = if output.encode.hdr() {
            bases
                .iter()
                .find(|base| base.matches(resized, output.encode, opts))
        } else {
            None
        };
        let reused = cached.is_some();
        let encoded = if let Some(base) = cached {
            yhotos_codec::encode_reusing_sdr(resized, output.encode, opts, &path, base)
        } else if !output.encode.hdr()
            && working.has_gain_map()
            && output.keep_icc
            && matches!(output.encode, Encode::Jpeg { .. } | Encode::Jxl { .. })
            && job.outputs.iter().any(|future| {
                future.encode.hdr()
                    && future.resize == output.resize
                    && future.keep_icc == output.keep_icc
                    && future.encode.sdr() == output.encode
            })
            && !bases
                .iter()
                .any(|base| base.matches(resized, output.encode, opts))
        {
            yhotos_codec::encode_sdr_cached(
                resized,
                output.encode,
                opts,
                &path,
                CACHE_BYTES - cached_bytes,
            )
            .map(|(encoded, base)| {
                if let Some(base) = base {
                    cached_bytes += base.bytes();
                    bases.push(base);
                }
                encoded
            })
        } else {
            yhotos_codec::encode(resized, output.encode, opts, &path)
        };
        let encoded = match encoded {
            Ok(encoded) => encoded,
            Err(_) if output.encode.hdr() => {
                std::fs::remove_file(&path).map_err(|_| Failure(3, "output cleanup failed"))?;
                skipped_outputs.push(SkippedOutput {
                    name: output.name.clone(),
                    reason: resized
                        .hdr_skipped_reason()
                        .unwrap_or(yhotos_codec::HdrSkippedReason::EncoderUnavailable),
                });
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if reused {
            hdr_base_reuses += 1;
        }
        let mut file = std::fs::File::open(&path).map_err(|_| Failure(3, "output unavailable"))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|_| Failure(3, "output unavailable"))?;
            if n == 0 {
                break;
            }
            hasher.update(buffer.get(..n).ok_or(Failure(3, "output unavailable"))?);
        }
        outputs.push(OutputReport {
            name: output.name.clone(),
            bytes: encoded.bytes,
            width: encoded.width,
            height: encoded.height,
            blake3: hasher.finalize().to_hex().to_string(),
            millis: t.elapsed().as_millis(),
        });
    }
    let report = Report {
        hdr_skipped_reason: working.hdr_skipped_reason(),
        skipped_outputs,
        identity: job.want.contains(&Want::Identity).then_some(id),
        metadata: if job.want.contains(&Want::Metadata) {
            Some(yhotos_codec::metadata(&input)?)
        } else {
            None
        },
        thumbhash: if job.want.contains(&Want::Thumbhash) {
            Some(
                base64::engine::general_purpose::STANDARD
                    .encode(yhotos_codec::thumbhash(&working)?),
            )
        } else {
            None
        },
        dominant_colors: if job.want.contains(&Want::DominantColors) {
            Some(yhotos_codec::dominant_colors(&working, 5)?)
        } else {
            None
        },
        outputs,
        timings: Timings {
            #[cfg(feature = "test-hooks")]
            decode_calls: DECODE_CALLS.load(std::sync::atomic::Ordering::Relaxed),
            identify_ms,
            decode_ms,
            hdr_base_reuses,
            total_ms: start.elapsed().as_millis(),
        },
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &report).map_err(|_| Failure(3, "report failed"))?;
    stdout
        .write_all(b"\n")
        .map_err(|_| Failure(3, "report failed"))?;
    Ok(())
}
// The small supervisor never calls a codec or reads the job. Only the child
// inherits stdin, and it applies its sandbox before reading it. Capture the
// report until success and discard native/allocator stderr so aborts and signals
// have the same parent-facing contract as a returned decode error.
fn supervise() -> Result<(), Failure> {
    use std::process::{Command, Stdio};
    clean_environment()?;
    let executable = std::env::current_exe().map_err(|_| Failure(3, "image processing failed"))?;
    let mut child = Command::new(executable)
        .arg(CHILD_ARGUMENT)
        .arg(std::process::id().to_string())
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Failure(3, "image processing failed"))?;
    let mut report = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or(Failure(3, "image processing failed"))?
        .take(JOB_BYTES + 1)
        .read_to_end(&mut report);
    if read.is_err() || report.len() as u64 > JOB_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Failure(3, "image processing failed"));
    }
    let status = child
        .wait()
        .map_err(|_| Failure(3, "image processing failed"))?;
    match status.code() {
        Some(0) => std::io::stdout()
            .lock()
            .write_all(&report)
            .map_err(|_| Failure(3, "report failed")),
        Some(2) => Err(Failure(2, "image rejected")),
        Some(4) => Err(Failure(4, "codec timeout")),
        _ => Err(Failure(3, "image processing failed")),
    }
}
fn main() -> std::process::ExitCode {
    let result = if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == CHILD_ARGUMENT)
    {
        run()
    } else {
        supervise()
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(Failure(code, message)) => {
            let _ = writeln!(std::io::stderr().lock(), "{message}");
            std::process::ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod identify_failure_tests {
    use super::{CodecError, identify_failure};

    #[test]
    fn unsupported_and_corrupt_inputs_are_rejections_not_retries() {
        assert_eq!(identify_failure(CodecError::Unsupported).0, 2);
        assert_eq!(identify_failure(CodecError::Decode).0, 2);
        assert_eq!(identify_failure(CodecError::BadMagic).0, 2);
        assert_eq!(identify_failure(CodecError::Limits).0, 2);
        // An IO failure says nothing about the file, so it stays retryable.
        assert_eq!(identify_failure(CodecError::Io).0, 3);
    }
}
