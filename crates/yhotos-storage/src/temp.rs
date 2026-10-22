use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::paths::{Component, safe_join};
use crate::{StorageError, Store};

/// A store-owned UUID temporary directory, recursively removed on drop unless kept.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
    kept: bool,
}

impl TempDir {
    /// Returns the location reserved for this upload or archive job.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Transfers cleanup responsibility to the caller or the garbage collector.
    #[must_use]
    pub fn keep(mut self) -> PathBuf {
        self.kept = true;
        self.path.clone()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl Store {
    pub(crate) fn create_tmp_dir(&self) -> Result<TempDir, StorageError> {
        let parent = safe_join(&self.root, [Component::tmp()]);
        self.ensure_dirs(&parent)?;
        let path = safe_join(
            &parent,
            [Component::parse(&uuid::Uuid::now_v7().to_string())?],
        );
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o750);
        }
        builder
            .create(&path)
            .map_err(|e| self.error("create temporary directory", &path, e))?;
        self.sync_parent(&path)?;
        Ok(TempDir { path, kept: false })
    }

    pub(crate) fn new_temp(&self) -> Result<(fs::File, tempfile::TempPath), StorageError> {
        let dir = safe_join(&self.root, [Component::tmp()]);
        self.ensure_dirs(&dir)?;
        self.temp_in(&dir)
    }

    fn temp_in(&self, dir: &Path) -> Result<(fs::File, tempfile::TempPath), StorageError> {
        self.check_path(dir, false)?;
        let path = safe_join(dir, [Component::parse(&uuid::Uuid::now_v7().to_string())?]);
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o640);
        }
        // TempPath owns cleanup even if the enclosing future is cancelled.
        let temp = tempfile::TempPath::try_from_path(&path)
            .map_err(|e| self.error("reserve temporary", &path, e))?;
        match options.open(&path) {
            Ok(file) => Ok((file, temp)),
            Err(e) => {
                // An O_EXCL collision belongs to someone else; do not unlink it.
                let _ = temp.keep();
                Err(self.error("create temporary", &path, e))
            }
        }
    }

    pub(crate) fn publish(
        &self,
        temp: tempfile::TempPath,
        dest: &Path,
        refresh_staged: bool,
    ) -> Result<bool, StorageError> {
        let _lock = self.lock_publication(fs::File::lock_shared)?;
        self.publish_locked(temp, dest, refresh_staged)
    }

    // Callers hold the shared publication flock through placement and refresh.
    fn publish_locked(
        &self,
        temp: tempfile::TempPath,
        dest: &Path,
        refresh_staged: bool,
    ) -> Result<bool, StorageError> {
        let parent = dest.parent().ok_or(StorageError::InvalidName)?;
        self.ensure_dirs(parent)?;
        // Refresh immediately before publication under the same lock GC uses
        // for its final decision. A long copy or lock wait must not consume the
        // new derivative's grace period before it is visible.
        // Originals already have a fresh streaming-write mtime and are synced.
        if refresh_staged {
            self.check_path(&temp, true)?;
            let staged = fs::OpenOptions::new()
                .write(true)
                .open(&temp)
                .map_err(|e| self.error("open staged publication", &temp, e))?;
            staged
                .set_modified(SystemTime::now())
                .and_then(|()| staged.sync_data())
                .map_err(|e| self.error("refresh staged publication", &temp, e))?;
        }
        // tempfile uses renameat2(RENAME_NOREPLACE) on Linux, with a no-clobber
        // hard-link fallback. Checking existence before plain rename would race.
        let existed = match temp.persist_noclobber(dest) {
            Ok(()) => false,
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
                self.check_path(dest, true)?;
                // Reusing an orphan is a new publication too. Refresh the
                // retained inode before discarding the duplicate so GC gives
                // the caller a full grace period to commit its reference.
                let destination = fs::OpenOptions::new()
                    .write(true)
                    .open(dest)
                    .map_err(|e| self.error("open duplicate", dest, e))?;
                destination
                    .set_modified(SystemTime::now())
                    .map_err(|e| self.error("set duplicate publication time", dest, e))?;
                destination
                    .sync_all()
                    .map_err(|e| self.error("sync duplicate publication time", dest, e))?;
                discard_duplicate(e.path, self)?;
                true
            }
            Err(e) => return Err(self.error("publish", dest, e.error)),
        };
        self.sync_parent(dest)?;
        Ok(existed)
    }

    pub(crate) fn move_derivative(&self, src: &Path, dest: &Path) -> Result<(), StorageError> {
        self.move_derivative_with(src, dest, |from, to| fs::rename(from, to))
    }

    pub(crate) fn move_derivative_with(
        &self,
        src: &Path,
        dest: &Path,
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), StorageError> {
        let temp_root = safe_join(&self.root, [Component::tmp()]);
        if !src.starts_with(&temp_root) {
            return Err(StorageError::InvalidName);
        }
        // A rename preserves an old codec output's mtime. Protect the source
        // and staging file before moving it, through publication and refresh,
        // so GC cannot collect it during that otherwise unprotected interval.
        let _lock = self.lock_publication(fs::File::lock_shared)?;
        // Reject symlinks before canonicalization can hide them. The canonical
        // root is trusted configuration; only its descendants are storage names.
        self.check_path(src, true)?;
        let src = fs::canonicalize(src).map_err(|e| self.error("resolve codec output", src, e))?;
        let temp_root = fs::canonicalize(&temp_root)
            .map_err(|e| self.error("resolve temporary directory", &temp_root, e))?;
        if !src.starts_with(&temp_root) {
            return Err(StorageError::InvalidName);
        }
        let src = src.as_path();
        // Apply private permissions and sync before the inode enters an area.
        let source = fs::OpenOptions::new()
            .write(true)
            .open(src)
            .map_err(|e| self.error("open codec output", Path::new("<codec-output>"), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            source
                .set_permissions(fs::Permissions::from_mode(0o640))
                .map_err(|e| self.error("set output permissions", dest, e))?;
        }
        source
            .sync_data()
            .map_err(|e| self.error("sync codec output", dest, e))?;
        drop(source);
        let parent = dest.parent().ok_or(StorageError::InvalidName)?;
        self.ensure_dirs(parent)?;
        let (staging, temp) = self.temp_in(parent)?;
        drop(staging);
        let copied = match rename(src, &temp) {
            Ok(()) => false,
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                copy_source(src, &temp).map_err(|e| self.error("copy codec output", dest, e))?;
                let copy = fs::OpenOptions::new()
                    .write(true)
                    .open(&temp)
                    .map_err(|e| self.error("open staged output", dest, e))?;
                copy.sync_data()
                    .map_err(|e| self.error("sync copied output", dest, e))?;
                true
            }
            Err(e) => return Err(self.error("stage codec output", dest, e)),
        };
        self.publish_locked(temp, dest, true)?;
        if copied {
            fs::remove_file(src)
                .map_err(|e| self.error("remove codec output", Path::new("<codec-output>"), e))?;
        }
        self.sync_parent(src)?;
        Ok(())
    }
}

fn discard_duplicate(temp: tempfile::TempPath, store: &Store) -> Result<(), StorageError> {
    let path = temp.to_path_buf();
    temp.close()
        .map_err(|e| store.error("discard duplicate", &path, e))
}

fn copy_source(src: &Path, dest: &Path) -> io::Result<()> {
    let mut src = fs::File::open(src)?;
    let mut dest = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(dest)?;
    io::copy(&mut src, &mut dest)?;
    Ok(())
}
