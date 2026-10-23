//! Video supervisor. Every parser is execed only after the child installs confinement.
use crate::Failure;
use nix::sys::resource::{Resource, setrlimit};
use serde::{Deserialize, Serialize};
use std::os::unix::process::ExitStatusExt;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
mod command;
mod probe;
use probe::Info;

const CAP: u64 = 1 << 20;
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Settings {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub max_duration: u32,
    pub timeout: u32,
    pub threads: u8,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            ffmpeg: "/usr/bin/ffmpeg".into(),
            ffprobe: "/usr/bin/ffprobe".into(),
            max_duration: 1800,
            timeout: 1800,
            threads: 2,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Job {
    input: PathBuf,
    out_dir: PathBuf,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    probe_only: bool,
}
#[derive(Deserialize, Serialize)]
struct Invocation {
    job: Job,
    stage: Stage,
    info: Option<Info>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
enum Stage {
    Probe,
    Av1Probe,
    H264,
    Av1,
    Poster,
    Scrub,
}
#[derive(Serialize)]
struct Output {
    name: &'static str,
    bytes: u64,
    blake3: String,
    width: u32,
    height: u32,
    av1_level: Option<u8>,
}
#[derive(Serialize)]
struct Report {
    info: Info,
    outputs: Vec<Output>,
}
fn unavailable() -> Failure {
    Failure(3, "video sandbox unavailable")
}
fn rejected() -> Failure {
    Failure(2, "video rejected")
}
fn canonical(path: &Path, directory: bool) -> Result<PathBuf, Failure> {
    if !path.is_absolute() {
        return Err(rejected());
    }
    let p = path.canonicalize().map_err(|_| unavailable())?;
    if (directory && !p.is_dir()) || (!directory && !p.is_file()) {
        return Err(rejected());
    }
    Ok(p)
}
fn validate(job: &mut Job) -> Result<(), Failure> {
    job.input = canonical(&job.input, false)?;
    job.out_dir = canonical(&job.out_dir, true)?;
    job.settings.ffmpeg = canonical(&job.settings.ffmpeg, false)?;
    job.settings.ffprobe = canonical(&job.settings.ffprobe, false)?;
    if job.input.starts_with(&job.out_dir)
        || job.settings.ffmpeg.starts_with(&job.out_dir)
        || job.settings.ffprobe.starts_with(&job.out_dir)
        || !(1..=1800).contains(&job.settings.max_duration)
        || !(1..=7200).contains(&job.settings.timeout)
        || !(1..=16).contains(&job.settings.threads)
        || std::fs::metadata(&job.input)
            .map_err(|_| unavailable())?
            .len()
            > 2 << 30
    {
        return Err(rejected());
    }
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>() -> Result<T, Failure> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| rejected())?;
    if bytes.len() as u64 > CAP {
        return Err(rejected());
    }
    serde_json::from_slice(&bytes).map_err(|_| rejected())
}
// Linux Landlock also checks PT_INTERP's execute right. Requiring static ELF
// binaries preserves the exact two-file execute policy without a loader grant.
fn require_static(binary: &Path) -> Result<(), Failure> {
    use std::io::{Seek, SeekFrom};
    let rejected = || Failure(3, "video executable requires static linking");
    let mut file = std::fs::File::open(binary).map_err(|_| unavailable())?;
    let mut header = [0_u8; 64];
    file.read_exact(&mut header).map_err(|_| rejected())?;
    if header.get(..7) != Some(&b"\x7fELF\x02\x01\x01"[..]) {
        return Err(rejected());
    }
    let offset = u64::from_le_bytes(
        header
            .get(32..40)
            .ok_or_else(rejected)?
            .try_into()
            .map_err(|_| rejected())?,
    );
    let size = u16::from_le_bytes(
        header
            .get(54..56)
            .ok_or_else(rejected)?
            .try_into()
            .map_err(|_| rejected())?,
    );
    let count = u16::from_le_bytes(
        header
            .get(56..58)
            .ok_or_else(rejected)?
            .try_into()
            .map_err(|_| rejected())?,
    );
    if offset > 65536 || size != 56 || !(1..=128).contains(&count) {
        return Err(rejected());
    }
    file.seek(SeekFrom::Start(offset)).map_err(|_| rejected())?;
    for _ in 0..count {
        let mut ph = [0_u8; 56];
        file.read_exact(&mut ph).map_err(|_| rejected())?;
        if ph.get(..4) == Some(&3_u32.to_le_bytes()[..]) {
            return Err(rejected());
        }
    }
    Ok(())
}
struct Group(u32);
impl Drop for Group {
    fn drop(&mut self) {
        if let Ok(id) = i32::try_from(self.0) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(id),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
fn bounded(
    mut cmd: Command,
    timeout: Duration,
    request: Option<&Invocation>,
) -> Result<Vec<u8>, Failure> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0)
        .stdin(if request.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| unavailable())?;
    let group = Group(child.id());
    if let Some(request) = request {
        let mut input = child.stdin.take().ok_or_else(unavailable)?;
        serde_json::to_writer(&mut input, request).map_err(|_| unavailable())?;
        input.flush().map_err(|_| unavailable())?;
    }
    let stdout = child.stdout.take().ok_or_else(unavailable)?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(CAP + 1).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|_| unavailable())? {
            break status;
        }
        if start.elapsed() >= timeout {
            drop(group);
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(Failure(4, "video timeout"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 > CAP {
        return Err(rejected());
    }
    // RLIMIT_CPU can terminate the parser before the wall watchdog fires.
    // ffmpeg may catch SIGXCPU and exit 255 instead of reporting a signal.
    // Both forms of interruption must preserve the retryable timeout contract.
    if matches!(
        status.signal(),
        Some(nix::libc::SIGXCPU | nix::libc::SIGKILL)
    ) || status.code() == Some(255)
    {
        return Err(Failure(4, "video timeout"));
    }
    if !status.success() {
        return Err(if status.code() == Some(3) {
            unavailable()
        } else {
            rejected()
        });
    }
    Ok(bytes)
}
pub(super) fn run() -> Result<(), Failure> {
    let mut job: Job = read_json()?;
    validate(&mut job)?;
    require_static(&job.settings.ffmpeg)?;
    require_static(&job.settings.ffprobe)?;
    let start = Instant::now();
    let call = |stage, info: Option<Info>| {
        let mut cmd = Command::new(std::env::current_exe().map_err(|_| unavailable())?);
        cmd.env_clear()
            .arg("--video-exec")
            .arg(std::process::id().to_string());
        #[cfg(feature = "test-hooks")]
        if std::env::args().any(|s| s == "--video-realtime") {
            cmd.arg("--video-realtime");
        }
        let remaining = Duration::from_secs(u64::from(job.settings.timeout))
            .checked_sub(start.elapsed())
            .ok_or(Failure(4, "video timeout"))?;
        bounded(
            cmd,
            if matches!(stage, Stage::Probe | Stage::Av1Probe) {
                remaining.min(Duration::from_secs(20))
            } else {
                remaining
            },
            Some(&Invocation {
                job: job.clone(),
                stage,
                info,
            }),
        )
    };
    let bytes = call(Stage::Probe, None)?;
    let info = probe::parse(&bytes, job.settings.max_duration)?;
    let mut outputs = Vec::new();
    if !job.probe_only {
        for (stage, name, dimensions) in [
            (Stage::H264, "h264.mp4", command::dimensions(&info, 1080)),
            (Stage::Av1, "av1.mp4", command::av1_dimensions(&info)),
            (Stage::Poster, "poster.jpeg", (info.width, info.height)),
            (Stage::Scrub, "scrub.jpeg", (1600, 1600)),
        ] {
            if job.out_dir.join(name).symlink_metadata().is_ok() {
                return Err(rejected());
            }
            call(stage, Some(info.clone()))?;
            let path = job.out_dir.join(name);
            let meta = std::fs::symlink_metadata(&path).map_err(|_| rejected())?;
            if !meta.is_file() || meta.len() == 0 || meta.len() > 20 << 30 {
                return Err(rejected());
            }
            let mut file = std::fs::File::open(&path).map_err(|_| rejected())?;
            let mut hash = blake3::Hasher::new();
            let mut buffer = vec![0; 65536];
            loop {
                let n = file.read(&mut buffer).map_err(|_| rejected())?;
                if n == 0 {
                    break;
                }
                hash.update(buffer.get(..n).ok_or_else(rejected)?);
            }
            outputs.push(Output {
                name,
                bytes: meta.len(),
                blake3: hash.finalize().to_hex().to_string(),
                width: dimensions.0,
                height: dimensions.1,
                av1_level: if matches!(stage, Stage::Av1) {
                    Some(probe::av1_level(&call(Stage::Av1Probe, None)?)?)
                } else {
                    None
                },
            });
        }
    }
    serde_json::to_writer(std::io::stdout().lock(), &Report { info, outputs })
        .map_err(|_| unavailable())
}
pub(super) fn child() -> Result<(), Failure> {
    use std::os::unix::process::CommandExt;
    let parent: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .ok_or_else(unavailable)?;
    yhotos_codec::sandbox::parent_lifetime(parent).map_err(|_| unavailable())?;
    nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
        .map_err(|_| unavailable())?;
    let mut invocation: Invocation = read_json()?;
    validate(&mut invocation.job)?;
    require_static(&invocation.job.settings.ffmpeg)?;
    require_static(&invocation.job.settings.ffprobe)?;
    let job = &invocation.job;
    // No descriptor from the worker or supervisor may bypass Landlock. Collect
    // before closing so the /proc enumeration descriptor has already been dropped.
    let fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")
        .map_err(|_| unavailable())?
        .map(|entry| {
            let entry = entry.map_err(|_| unavailable())?;
            entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
                .ok_or_else(unavailable)
        })
        .collect::<Result<Vec<_>, Failure>>()?
        .into_iter()
        .filter(|fd| *fd > 2)
        .collect();
    for fd in fds {
        let _ = nix::unistd::close(fd);
    }
    for (resource, limit) in [
        (Resource::RLIMIT_AS, 3 << 30),
        (Resource::RLIMIT_CPU, u64::from(job.settings.timeout)),
        (Resource::RLIMIT_FSIZE, 20 << 30),
        (Resource::RLIMIT_NOFILE, 64),
        (Resource::RLIMIT_CORE, 0),
    ] {
        setrlimit(resource, limit, limit).map_err(|_| unavailable())?;
    }
    nix::sys::prctl::set_no_new_privs().map_err(|_| unavailable())?;
    crate::filesystem::video(
        &job.input,
        &job.out_dir,
        [&job.settings.ffmpeg, &job.settings.ffprobe],
    )?;
    crate::seccomp::apply_video().map_err(|()| unavailable())?;
    #[cfg(debug_assertions)]
    if std::env::args().any(|s| s == "--video-test") {
        return test_policy(job);
    }
    let mut cmd = command::build(job, invocation.stage, invocation.info.as_ref())?;
    let _ = cmd.exec();
    Err(unavailable())
}
#[cfg(debug_assertions)]
fn test_policy(job: &Job) -> Result<(), Failure> {
    use std::os::unix::process::CommandExt;
    let denied = || Failure(3, "video policy failed");
    yhotos_codec::sandbox::check_no_fork().map_err(|_| denied())?;
    if std::net::UdpSocket::bind("127.0.0.1:0")
        .err()
        .and_then(|e| e.raw_os_error())
        != Some(nix::libc::EPERM)
    {
        return Err(denied());
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .open(&job.input)
        .err()
        .and_then(|e| e.raw_os_error())
        != Some(nix::libc::EACCES)
    {
        return Err(denied());
    }
    if std::fs::write(
        job.input
            .parent()
            .ok_or_else(denied)?
            .join("outside-denied"),
        b"denied",
    )
    .err()
    .and_then(|e| e.raw_os_error())
        != Some(nix::libc::EACCES)
    {
        return Err(denied());
    }
    std::fs::write(job.out_dir.join("permitted"), b"ok").map_err(|_| denied())?;
    if Command::new("/bin/true").exec().raw_os_error() != Some(nix::libc::EACCES) {
        return Err(denied());
    }
    if Command::new(&job.settings.ffmpeg)
        .spawn()
        .err()
        .and_then(|e| e.raw_os_error())
        != Some(nix::libc::EPERM)
    {
        return Err(denied());
    }
    // A binary copied into writable output has no execute grant either.
    if Command::new(job.out_dir.join("other"))
        .exec()
        .raw_os_error()
        != Some(nix::libc::EACCES)
    {
        return Err(denied());
    }
    Ok(())
}
