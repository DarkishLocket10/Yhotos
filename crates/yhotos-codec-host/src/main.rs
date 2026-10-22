#![forbid(unsafe_code)]
//! One bounded JSON job per process. Production confinement is Linux plus mounts.
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
const JOB_BYTES: u64 = 1 << 20;
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
            .map_err(|_| Failure(2, "sandbox unavailable"))?;
        setrlimit(Resource::RLIMIT_CPU, self.cpu_seconds, self.cpu_seconds + 1)
            .map_err(|_| Failure(2, "sandbox unavailable"))?;
        setrlimit(
            Resource::RLIMIT_FSIZE,
            self.max_file_size,
            self.max_file_size,
        )
        .map_err(|_| Failure(2, "sandbox unavailable"))?;
        setrlimit(Resource::RLIMIT_NOFILE, 64, 64)
            .map_err(|_| Failure(2, "sandbox unavailable"))?;
        // Linux counts threads toward NPROC, so limit process creation via seccomp
        // while retaining the two libvips threads. NPROC=0 would prohibit them too.
        yhotos_codec::sandbox::process_limit().map_err(|_| Failure(2, "sandbox unavailable"))?;
        yhotos_codec::sandbox::timeouts(self.wall_seconds)
            .map_err(|_| Failure(2, "sandbox unavailable"))
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
struct Report {
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
    identify_ms: u128,
    decode_ms: u128,
    total_ms: u128,
}
struct Failure(u8, &'static str);
impl From<CodecError> for Failure {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::BadMagic
            | CodecError::Mismatch
            | CodecError::Limits
            | CodecError::Io
            | CodecError::Options => Self(2, "image rejected"),
            _ => Self(3, "image processing failed"),
        }
    }
}
// A new exec clears env before any libvips initialization, without unsafe Rust
// environment mutation. No PATH/HOME/LD_*/VIPS_* supplied by the caller survives.
fn clean_environment() -> Result<(), Failure> {
    let expected = [
        ("VIPS_CONCURRENCY", "2"),
        ("VIPS_WARNING", "0"),
        ("VIPS_DISC_THRESHOLD", "3g"),
        ("LC_ALL", "C"),
    ];
    if std::env::vars_os()
        .filter(|(name, _)| name != "__CF_USER_TEXT_ENCODING")
        .count()
        == expected.len()
        && expected
            .iter()
            .all(|(k, v)| std::env::var(k).is_ok_and(|s| s == *v))
    {
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(|_| Failure(2, "startup failed"))?;
    let error = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env_clear()
        .envs(expected)
        .exec();
    let _ = error;
    Err(Failure(2, "startup failed"))
}
fn paths(job: &Job) -> Result<(PathBuf, Vec<PathBuf>), Failure> {
    let input = job
        .input
        .canonicalize()
        .map_err(|_| Failure(2, "invalid input"))?;
    if !input.is_file() {
        return Err(Failure(2, "invalid input"));
    }
    let dir = job
        .out_dir
        .canonicalize()
        .map_err(|_| Failure(2, "invalid output directory"))?;
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
    clean_environment()?;
    HostLimits::default().apply()?;
    #[cfg(target_os = "linux")]
    seccomp::apply().map_err(|()| Failure(2, "sandbox unavailable"))?;
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
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(JOB_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure(2, "invalid job"))?;
    if bytes.len() as u64 > JOB_BYTES {
        return Err(Failure(2, "job too large"));
    }
    let job: Job = serde_json::from_slice(&bytes).map_err(|_| Failure(2, "invalid job"))?;
    job.limits.apply()?; // Job may only tighten startup caps.
    let (input, paths) = paths(&job)?;
    let start = Instant::now();
    let id = yhotos_codec::identify(&input, &job.limits.codec())?;
    let identify_ms = start.elapsed().as_millis();
    let decode_start = Instant::now();
    let working = yhotos_codec::decode(&input, &id, &job.limits.codec())?;
    let decode_ms = decode_start.elapsed().as_millis();
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
    for (output, path) in job.outputs.iter().zip(paths) {
        let t = Instant::now();
        // Reserve atomically, including case-folded aliases on macOS. The private
        // worker directory prevents other processes replacing this entry.
        let reservation = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| Failure(2, "invalid output name"))?;
        drop(reservation);
        let resized = yhotos_codec::resize(&working, output.resize)?;
        let encoded = yhotos_codec::encode(
            &resized,
            output.encode,
            EncodeOpts {
                keep_icc: output.keep_icc,
                strip_metadata: true,
            },
            &path,
        )?;
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
            identify_ms,
            decode_ms,
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
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(Failure(code, message)) => {
            let _ = writeln!(std::io::stderr().lock(), "{message}");
            std::process::ExitCode::from(code)
        }
    }
}
