use std::fs;
use std::path::Path;

use crate::paths::{Component, safe_join};
use crate::{StorageError, Store};

pub(crate) struct PublicationLock(fs::File);

impl Drop for PublicationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl Store {
    pub(crate) fn open_publish_lock(&self) -> Result<fs::File, StorageError> {
        let path = safe_join(&self.root, [Component::parse(".publish.lock")?]);
        match self.check_path(&path, true) {
            Ok(()) => {}
            Err(StorageError::NotFound) => {
                let mut options = fs::OpenOptions::new();
                options.read(true).write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o640);
                }
                match options.open(&path) {
                    Ok(file) => {
                        self.sync_parent(&path)?;
                        return Ok(file);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        self.check_path(&path, true)?;
                    }
                    Err(e) => return Err(self.error("create publication lock", &path, e)),
                }
            }
            Err(e) => return Err(e),
        }
        fs::File::open(&path).map_err(|e| self.error("open publication lock", &path, e))
    }

    pub(crate) fn lock_publication(
        &self,
        lock: impl Fn(&fs::File) -> std::io::Result<()>,
    ) -> Result<PublicationLock, StorageError> {
        // Reopen rather than try_clone: the lock is tied to the open file
        // description. Sharing one descriptor would defeat thread exclusion.
        // Dropping this independently owned guard releases its lock, including
        // when an operation errors or its async caller is cancelled.
        let file = self.open_publish_lock()?;
        loop {
            match lock(&file) {
                Ok(()) => return Ok(PublicationLock(file)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(crate::io_error("lock publication", ".publish.lock", e));
                }
            }
        }
    }

    pub(crate) fn check_path(&self, path: &Path, file: bool) -> Result<(), StorageError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| StorageError::InvalidName)?;
        let mut current = self.root.clone();
        let root_metadata =
            fs::symlink_metadata(&current).map_err(|e| self.error("inspect root", &current, e))?;
        if !root_metadata.is_dir() || root_metadata.is_symlink() {
            return Err(StorageError::InvalidName);
        }
        let mut components = relative.components().peekable();
        while let Some(part) = components.next() {
            let value = part.as_os_str().to_str().ok_or(StorageError::InvalidName)?;
            current = safe_join(&current, [Component::parse(value)?]);
            let meta =
                fs::symlink_metadata(&current).map_err(|e| self.error("inspect", &current, e))?;
            if meta.is_symlink()
                || if components.peek().is_none() && file {
                    !meta.is_file()
                } else {
                    !meta.is_dir()
                }
            {
                return Err(StorageError::InvalidName);
            }
        }
        Ok(())
    }

    pub(crate) fn ensure_dirs(&self, path: &Path) -> Result<(), StorageError> {
        self.check_path(&self.root, false)?;
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| StorageError::InvalidName)?;
        let mut current = self.root.clone();
        for part in relative.components() {
            current = safe_join(
                &current,
                [Component::parse(
                    part.as_os_str().to_str().ok_or(StorageError::InvalidName)?,
                )?],
            );
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o750);
            }
            match builder.create(&current) {
                Ok(()) => self.sync_parent(&current)?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(self.error("create directory", &current, e)),
            }
            self.check_path(&current, false)?;
        }
        Ok(())
    }

    // Keep the same fallible method on all platforms; only Linux syncs dirs.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::unused_self, clippy::unnecessary_wraps)
    )]
    pub(crate) fn sync_parent(&self, path: &Path) -> Result<(), StorageError> {
        #[cfg(not(target_os = "linux"))]
        let _ = path;
        #[cfg(target_os = "linux")]
        if let Some(parent) = path.parent() {
            fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| self.error("sync directory", parent, e))?;
        }
        Ok(())
    }
}
