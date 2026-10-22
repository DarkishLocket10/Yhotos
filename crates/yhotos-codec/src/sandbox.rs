// Bounded native buffers and reference numeric kernels use checked dimensions.
#![allow(clippy::borrow_as_ptr)]
//! Async-signal-safe timeout exit, confined to the native boundary crate.
use crate::CodecError;
use nix::libc;

/// Kill the Linux decoder child if its supervisor exits, including a worker's
/// forced termination of the supervisor. Reject a parent that died during setup.
///
/// # Errors
/// Returns a typed error if the parent is invalid or the kernel setup fails.
#[cfg(target_os = "linux")]
pub fn parent_lifetime(parent: u32) -> Result<(), CodecError> {
    let parent = libc::pid_t::try_from(parent).map_err(|_| CodecError::Options)?;
    if parent <= 0 {
        return Err(CodecError::Options);
    }
    nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)
        .map_err(|_| CodecError::Io)?;
    // Check after installing the signal so a parent lost during startup cannot
    // leave an orphaned decoder running past the worker's deadline.
    if nix::unistd::getppid().as_raw() != parent {
        return Err(CodecError::Io);
    }
    Ok(())
}
unsafe extern "C" fn timeout(_: libc::c_int) {
    // SAFETY: write and _exit are async-signal-safe; the static buffer has the exact stated length.
    unsafe {
        libc::write(2, c"codec timeout\n".as_ptr().cast(), 14);
        libc::_exit(4);
    }
}
/// Install timeout handlers before creating threads; CPU and wall alarms exit with code 4.
/// The worker must still impose a separate wall deadline and kill crashed children.
///
/// # Errors
/// Returns a typed error when options are invalid or native processing fails.
pub fn timeouts(seconds: u32) -> Result<(), CodecError> {
    if seconds == 0 {
        return Err(CodecError::Options);
    }
    // SAFETY: zero is a valid sigaction initializer; handlers access no Rust state and never return.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = timeout as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut()) != 0
            || libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut()) != 0
        {
            return Err(CodecError::Io);
        }
        libc::alarm(seconds);
    }
    Ok(())
}

/// Limit child processes on platforms where doing so leaves native threads usable.
///
/// # Errors
/// Returns a typed error when options are invalid or native processing fails.
pub fn process_limit() -> Result<(), CodecError> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the struct is initialized; setrlimit reads it only for this call.
        let result = unsafe {
            libc::setrlimit(
                libc::RLIMIT_NPROC,
                &libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                },
            )
        };
        if result != 0 {
            return Err(CodecError::Io);
        }
    }
    Ok(())
}

/// Debug-only regression probe: self operations remain available, but signal and
/// resource-limit syscalls targeting a live same-user process must return EPERM.
/// Uses signal 0 and read-only limit queries, so it cannot harm the target.
///
/// # Errors
/// Rejects an invalid PID or a filter that permits cross-process operations.
#[cfg(all(target_os = "linux", debug_assertions))]
pub fn check_process_isolation(other: u32) -> Result<(), CodecError> {
    let pid = libc::pid_t::try_from(other).map_err(|_| CodecError::Options)?;
    if pid <= 0 || other == std::process::id() {
        return Err(CodecError::Options);
    }
    // SAFETY: signal 0 only probes permission. Both PIDs are positive; prlimit
    // receives a writable initialized output and no new limits (read-only query).
    unsafe {
        let me = libc::getpid();
        let tid = libc::syscall(libc::SYS_gettid);
        let mut limit: libc::rlimit64 = std::mem::zeroed();
        if libc::syscall(libc::SYS_tgkill, me, tid, 0) != 0
            || libc::prlimit64(0, libc::RLIMIT_NOFILE, std::ptr::null(), &mut limit) != 0
        {
            return Err(CodecError::Io);
        }
        if libc::syscall(libc::SYS_tgkill, pid, pid, 0) != -1
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
        {
            return Err(CodecError::Options);
        }
        if libc::prlimit64(pid, libc::RLIMIT_NOFILE, std::ptr::null(), &mut limit) != -1
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
        {
            return Err(CodecError::Options);
        }
    }
    Ok(())
}
