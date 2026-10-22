//! Linux production policy. Unknown syscalls return EPERM; clone3 returns ENOSYS
//! so glibc can fall back to the flags-inspected clone syscall.
//! Reproduce the syscall inventory in Docker with:
//! strace -f -c target/debug/yhotos-codec-host < job.json
//! The initial list must be validated in that image when encoder versions change.
use nix::libc;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use std::collections::BTreeMap;
#[allow(clippy::too_many_lines)] // Keep syscall groups and the stacked policy auditable together.
pub(super) fn apply() -> Result<(), ()> {
    nix::sys::prctl::set_no_new_privs().map_err(|_| ())?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let calls = [
        // Buffered IO, mmap-backed loaders and encoder output. Path permissions are
        // enforced by Landlock before decoding, in addition to production mounts.
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_close,
        libc::SYS_lseek,
        libc::SYS_openat,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_getcwd,
        libc::SYS_readlinkat,
        libc::SYS_getdents64,
        libc::SYS_fcntl,
        libc::SYS_fsync,
        libc::SYS_ftruncate,
        // Startup installs a deny-by-default filesystem policy. These calls can
        // only further restrict rights; a compromised decoder cannot relax it.
        libc::SYS_landlock_create_ruleset,
        libc::SYS_landlock_add_rule,
        libc::SYS_landlock_restrict_self,
        // Heap, mappings, allocator page release, CPU topology and thread stacks.
        libc::SYS_brk,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_yield,
        libc::SYS_futex,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_getpid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        // A stacked filter returns ENOSYS for clone3; this Allow avoids
        // overriding its errno with the main filter's default EPERM.
        libc::SYS_clone3,
        libc::SYS_gettid,
        // Timers, timeout handlers, entropy, process limits and normal termination.
        libc::SYS_clock_gettime,
        libc::SYS_setitimer,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_getrandom,
        libc::SYS_getrusage,
        libc::SYS_uname,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_restart_syscall,
    ];
    for call in calls {
        rules.insert(call, vec![]);
    }
    #[cfg(target_arch = "x86_64")]
    for call in [
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_access,
        libc::SYS_readlink,
        libc::SYS_arch_prctl,
        libc::SYS_alarm,
    ] {
        rules.insert(call, vec![]);
    }
    // Landlock redundantly sets this flag after seccomp startup. Allow only
    // the monotonic operation, with the exact unused arguments required by Linux.
    let nnp = [
        u64::try_from(libc::PR_SET_NO_NEW_PRIVS).map_err(|_| ())?,
        1,
        0,
        0,
        0,
    ];
    let conditions = nnp
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            SeccompCondition::new(
                u8::try_from(index).map_err(|_| ())?,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::Eq,
                value,
            )
            .map_err(|_| ())
        })
        .collect::<Result<Vec<_>, ()>>()?;
    rules.insert(
        libc::SYS_prctl,
        vec![SeccompRule::new(conditions).map_err(|_| ())?],
    );
    // Thread signals stay in this host's thread group; resource operations use
    // PID 0 (the calling process). Same-UID workers must never be targets.
    for (call, target) in [
        (libc::SYS_tgkill, u64::from(std::process::id())),
        (libc::SYS_prlimit64, 0),
    ] {
        let rule = SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, target)
                .map_err(|_| ())?,
        ])
        .map_err(|_| ())?;
        rules.insert(call, vec![rule]);
    }
    // Only pthread-style clones; no process creation or new namespaces. NPROC=0
    // additionally restricts unprivileged process creation where supported.
    let thread_flags = u64::try_from(
        libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD
            | libc::CLONE_SYSVSEM
            | libc::CLONE_SETTLS
            | libc::CLONE_PARENT_SETTID
            | libc::CLONE_CHILD_CLEARTID,
    )
    .map_err(|_| ())?;
    let clone = SeccompRule::new(vec![
        SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, thread_flags)
            .map_err(|_| ())?,
    ])
    .map_err(|_| ())?;
    rules.insert(libc::SYS_clone, vec![clone]);
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Errno(u32::try_from(libc::EPERM).map_err(|_| ())?),
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into().map_err(|_| ())?,
    )
    .map_err(|_| ())?
    .try_into()
    .map_err(|_| ())?;

    // clone3 has pointer-based arguments, so it is never permitted. Return ENOSYS
    // in a stacked filter to trigger pthread's safe clone fallback.
    let extra: BpfProgram = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, vec![])]),
        SeccompAction::Allow,
        SeccompAction::Errno(u32::try_from(libc::ENOSYS).map_err(|_| ())?),
        std::env::consts::ARCH.try_into().map_err(|_| ())?,
    )
    .map_err(|_| ())?
    .try_into()
    .map_err(|_| ())?;
    seccompiler::apply_filter_all_threads(&extra).map_err(|_| ())?;
    seccompiler::apply_filter_all_threads(&filter).map_err(|_| ())
}
