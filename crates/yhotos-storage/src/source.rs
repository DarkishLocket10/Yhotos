//! Descriptor-relative, read-only access to an untrusted migration source tree.
use std::{fs::File, os::fd::OwnedFd, path::Path};

use crate::{StorageError, paths::Component};
use nix::{
    fcntl::{OFlag, open},
    sys::stat::{Mode, SFlag, fstat},
};

/// An open source directory capability. Renaming its path cannot redirect reads.
/// No operation creates entries or opens them for writing.
#[derive(Debug)]
pub struct SourceTree {
    root: OwnedFd,
}

/// A read-only regular-file handle and its length at open time.
#[derive(Debug)]
pub struct SourceFile {
    file: File,
    length: u64,
}

impl SourceFile {
    /// Returns the regular file's length observed by `fstat` after opening.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.length
    }
    /// Reports whether the file was empty at open time.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
    /// Transfers the read-only handle without resolving its pathname again.
    #[must_use]
    pub fn into_file(self) -> File {
        self.file
    }
}

impl SourceTree {
    /// Opens an existing root directory read-only, rejecting a symlink root.
    ///
    /// # Errors
    /// Returns a redacted storage error for an inaccessible or invalid root.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        let root = open(
            root.as_ref(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(source_error)?;
        Ok(Self { root })
    }

    /// Opens only UTF-8 relative paths of 1..=4096 bytes with nonempty, non-dot
    /// components, no NULs or backslashes. Every component must be symlink-free;
    /// the final entry must be regular. Linux also forbids mount crossings.
    ///
    /// # Errors
    /// Rejects invalid names, symlinks, non-regular entries and filesystem errors.
    pub fn open_file(&self, relative: &str) -> Result<SourceFile, StorageError> {
        if relative.is_empty() || relative.len() > 4096 {
            return Err(StorageError::InvalidName);
        }
        let components = relative
            .split('/')
            .map(Component::source)
            .collect::<Result<Vec<_>, _>>()?;
        // NONBLOCK prevents an untrusted FIFO from hanging before fstat rejects it.
        let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK;
        #[cfg(target_os = "linux")]
        let fd = {
            use nix::fcntl::{OpenHow, ResolveFlag, openat2};
            let _ = components;
            openat2(
                &self.root,
                relative,
                OpenHow::new().flags(flags).resolve(
                    ResolveFlag::RESOLVE_BENEATH
                        | ResolveFlag::RESOLVE_NO_SYMLINKS
                        | ResolveFlag::RESOLVE_NO_MAGICLINKS
                        | ResolveFlag::RESOLVE_NO_XDEV,
                ),
            )
            .map_err(source_error)?
        };
        #[cfg(not(target_os = "linux"))]
        let fd = {
            use nix::fcntl::openat;
            let mut directory = self
                .root
                .try_clone()
                .map_err(|e| crate::io_error("source clone", ".", e))?;
            let mut iter = components.iter().peekable();
            while let Some(component) = iter.next() {
                let mode = if iter.peek().is_some() {
                    flags | OFlag::O_DIRECTORY
                } else {
                    flags
                };
                directory = openat(&directory, component.as_str(), mode, Mode::empty())
                    .map_err(source_error)?;
            }
            directory
        };
        let stat = fstat(&fd).map_err(source_error)?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
            return Err(StorageError::InvalidName);
        }
        let length = u64::try_from(stat.st_size).map_err(|_| StorageError::InvalidName)?;
        Ok(SourceFile {
            file: File::from(fd),
            length,
        })
    }
}

fn source_error(error: nix::errno::Errno) -> StorageError {
    match error {
        nix::errno::Errno::ELOOP | nix::errno::Errno::ENOTDIR | nix::errno::Errno::EXDEV => {
            StorageError::InvalidName
        }
        _ => crate::io_error(
            "source read",
            ".",
            std::io::Error::from_raw_os_error(error as i32),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        os::unix::fs::symlink,
    };

    #[test]
    fn rejects_untrusted_paths_and_non_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested/雪 photo.jpg"), b"original").unwrap();
        symlink("nested", temp.path().join("link-dir")).unwrap();
        symlink("nested/雪 photo.jpg", temp.path().join("link-file")).unwrap();
        nix::unistd::mkfifo(&temp.path().join("fifo"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let tree = SourceTree::open(temp.path()).unwrap();
        for path in [
            "",
            "/nested/file",
            "../file",
            "nested/../file",
            "nested/./file",
            "nested//file",
            "nested/",
            "a\\b",
            "a\0b",
            "link-dir/雪 photo.jpg",
            "link-file",
            "fifo",
            "nested",
        ] {
            assert!(tree.open_file(path).is_err(), "accepted {path:?}");
        }
        assert!(tree.open_file(&"a".repeat(4097)).is_err());
        let file = tree.open_file("nested/雪 photo.jpg").unwrap();
        assert_eq!(file.len(), 8);
        let mut file = file.into_file();
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();
        assert_eq!(data, "original");
        assert!(file.write_all(b"changed").is_err());
    }

    #[test]
    fn root_descriptor_survives_path_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"old").unwrap();
        let tree = SourceTree::open(&root).unwrap();
        fs::rename(&root, temp.path().join("moved")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"replacement").unwrap();
        assert_eq!(tree.open_file("file").unwrap().len(), 3);
    }
}
