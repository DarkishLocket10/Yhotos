//! Fail closed unless Linux can enforce read/write/truncate confinement (ABI v3).
use crate::Failure;
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};
use std::path::Path;

pub(super) fn restrict(input: &Path, out: &Path) -> Result<(), Failure> {
    let unavailable = || Failure(3, "filesystem sandbox unavailable");
    // An input inside the writable output directory is an invalid request, not a
    // missing sandbox facility, so it is rejected (2) rather than retryable (3).
    if input.starts_with(out) {
        return Err(Failure(2, "input inside output directory"));
    }
    // Landlock applies to this thread and future children. No pre-existing native
    // thread may retain ambient access after trusted plugin initialization.
    if std::fs::read_dir("/proc/self/task")
        .map_err(|_| unavailable())?
        .try_fold(0_usize, |count, entry| {
            entry.map(|_| count.saturating_add(1))
        })
        .map_err(|_| unavailable())?
        != 1
    {
        return Err(unavailable());
    }
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V3))
        .map_err(|_| unavailable())?
        .create()
        .map_err(|_| unavailable())?
        .add_rule(PathBeneath::new(
            PathFd::new(input).map_err(|_| unavailable())?,
            AccessFs::ReadFile,
        ))
        .map_err(|_| unavailable())?
        .add_rule(PathBeneath::new(
            PathFd::new(out).map_err(|_| unavailable())?,
            AccessFs::ReadFile
                | AccessFs::ReadDir
                | AccessFs::WriteFile
                | AccessFs::MakeReg
                | AccessFs::Truncate,
        ))
        .map_err(|_| unavailable())?
        .restrict_self()
        .map_err(|_| unavailable())?;
    if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
        return Err(unavailable());
    }
    Ok(())
}

#[cfg(debug_assertions)]
pub(super) fn probe(input: &Path, out: &Path) -> Result<(), Failure> {
    use std::fs::{File, OpenOptions};
    let failed = || Failure(3, "filesystem sandbox failed");
    let denied = |result: std::io::Result<File>| matches!(result, Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied);
    File::open(input).map_err(|_| failed())?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("allowed"))
        .map_err(|_| failed())?;
    file.set_len(5).map_err(|_| failed())?;
    File::open(out.join("allowed")).map_err(|_| failed())?;
    // Paths and symlinks are planted by the parent test. This is real access,
    // performed after the exact same confinement used by the decoding path.
    if !denied(OpenOptions::new().write(true).open(input))
        || !denied(OpenOptions::new().write(true).truncate(true).open(input))
        || !denied(File::open(out.join("../unrelated")))
        || !denied(
            OpenOptions::new()
                .write(true)
                .open(out.join("../unrelated")),
        )
        || !denied(
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(out.join("../unrelated")),
        )
        || !denied(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(out.join("../created")),
        )
        || !denied(File::open(out.join("escape")))
        || !denied(
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(out.join("escape")),
        )
    {
        return Err(failed());
    }
    Ok(())
}
