//! Restart-safe temporary uploads. Only typed UUIDs cross the storage boundary.
use crate::{
    StorageError, Store, blocking,
    paths::{Component, safe_join},
};
use std::{fmt, fs, path::PathBuf, str::FromStr, time::SystemTime};

/// Canonical lowercase UUID v4 naming a durable temporary upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TempId(uuid::Uuid);
impl FromStr for TempId {
    type Err = StorageError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = uuid::Uuid::parse_str(s).map_err(|_| StorageError::InvalidName)?;
        if id.get_version_num() != 4
            || id.get_variant() != uuid::Variant::RFC4122
            || id.to_string() != s
        {
            return Err(StorageError::InvalidName);
        }
        Ok(Self(id))
    }
}
impl fmt::Display for TempId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Store {
    fn upload_dir(&self, id: &TempId) -> Result<PathBuf, StorageError> {
        Ok(safe_join(
            &self.root,
            [Component::tmp(), Component::parse(&id.to_string())?],
        ))
    }
    fn upload_data(&self, id: &TempId) -> Result<PathBuf, StorageError> {
        Ok(safe_join(
            &self.upload_dir(id)?,
            [Component::parse("data")?],
        ))
    }
    /// Creates private `tmp/<uuid-v4>/data` exclusively and syncs its directory.
    /// # Errors
    /// Rejects unsafe tree entries and redacts filesystem failures.
    pub async fn create_upload(&self) -> Result<(TempId, tokio::fs::File), StorageError> {
        let store = self.clone();
        let (id, file) = blocking(move || {
            let id = TempId(uuid::Uuid::new_v4());
            let dir = store.upload_dir(&id)?;
            store.ensure_dirs(&dir)?;
            let path = store.upload_data(&id)?;
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o640);
            }
            let file = options
                .open(&path)
                .map_err(|e| store.error("create upload", &path, e))?;
            file.lock()
                .map_err(|e| store.error("lock upload", &path, e))?;
            store.sync_parent(&path)?;
            Ok((id, file))
        })
        .await?;
        Ok((id, tokio::fs::File::from_std(file)))
    }
    /// Reopens a regular upload for explicit seek/write; never uses `O_APPEND`.
    /// The handle holds an exclusive OS lock until all pending IO completes.
    /// Drop a previous handle before reopening the same upload.
    /// # Errors
    /// Rejects symlinks in every component and missing or non-regular files.
    pub async fn open_upload(&self, id: &TempId) -> Result<tokio::fs::File, StorageError> {
        let path = self.upload_data(id)?;
        let store = self.clone();
        let file = blocking(move || {
            store.check_path(&path, true)?;
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| store.error("open upload", &path, e))?;
            // Tokio's in-flight IO owns the descriptor too. Keep the OS lock
            // until all writes finish, even if the request future is cancelled
            // and its PostgreSQL row lock has already been rolled back.
            file.lock()
                .map_err(|e| store.error("lock upload", &path, e))?;
            Ok(file)
        })
        .await?;
        Ok(tokio::fs::File::from_std(file))
    }
    /// Returns the physical length after validating the complete path.
    /// # Errors
    /// Returns safe storage errors for unavailable or unsafe entries.
    pub async fn upload_len(&self, id: &TempId) -> Result<u64, StorageError> {
        self.open_upload(id)
            .await?
            .metadata()
            .await
            .map(|m| m.len())
            .map_err(|e| crate::io_error("inspect upload", "tmp", e))
    }
    /// Returns a checked trusted path for a codec-host request.
    /// # Errors
    /// Rejects missing files, symlinks and non-regular entries.
    pub fn upload_path_for_host(&self, id: &TempId) -> Result<PathBuf, StorageError> {
        let path = self.upload_data(id)?;
        self.check_path(&path, true)?;
        Ok(path)
    }
    /// Removes an upload without following symlinks; absence is idempotent.
    /// # Errors
    /// Rejects unsafe entries and redacts removal failures.
    pub async fn remove_upload(&self, id: &TempId) -> Result<(), StorageError> {
        let store = self.clone();
        let id = *id;
        blocking(move || {
            let dir = store.upload_dir(&id)?;
            match store.check_path(&dir, false) {
                Err(StorageError::NotFound) => return Ok(()),
                other => other?,
            }
            let path = store.upload_data(&id)?;
            match store.check_path(&path, true) {
                Err(StorageError::NotFound) => {}
                Ok(()) => {
                    fs::remove_file(&path).map_err(|e| store.error("remove upload", &path, e))?;
                }
                Err(e) => return Err(e),
            }
            fs::remove_dir(&dir).map_err(|e| store.error("remove upload directory", &dir, e))?;
            store.sync_parent(&dir)
        })
        .await
    }
    /// Lists only safe durable upload directories and their creation times.
    /// # Errors
    /// Rejects unsafe entries and redacts enumeration failures.
    pub async fn list_uploads(&self) -> Result<Vec<(TempId, SystemTime)>, StorageError> {
        let store = self.clone();
        blocking(move || {
            let root = safe_join(&store.root, [Component::tmp()]);
            match store.check_path(&root, false) {
                Err(StorageError::NotFound) => return Ok(Vec::new()),
                other => other?,
            }
            let entries = fs::read_dir(&root).map_err(|e| store.error("list uploads", &root, e))?;
            let mut result = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| store.error("list uploads", &root, e))?;
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<TempId>().ok())
                else {
                    continue;
                };
                let dir = store.upload_dir(&id)?;
                store.check_path(&dir, false)?;
                let meta = fs::symlink_metadata(&dir)
                    .map_err(|e| store.error("inspect upload", &dir, e))?;
                result.push((
                    id,
                    meta.created()
                        .or_else(|_| meta.modified())
                        .map_err(|e| store.error("upload age", &dir, e))?,
                ));
            }
            Ok(result)
        })
        .await
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[tokio::test]
    async fn durable_ids_reject_traversal_and_reopen_after_restart() {
        for bad in [
            "../data",
            "/tmp/file",
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert!(bad.parse::<TempId>().is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (id, mut file) = store.create_upload().await.unwrap();
        file.write_all(b"persist").await.unwrap();
        file.flush().await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.upload_len(&id).await.unwrap(), 7);
        assert_eq!(store.list_uploads().await.unwrap().len(), 1);
        store.remove_upload(&id).await.unwrap();
        assert_eq!(store.list_uploads().await.unwrap(), []);
    }
    #[tokio::test]
    async fn reopening_waits_until_the_previous_writer_releases_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (id, file) = store.create_upload().await.unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            started_tx.send(()).unwrap();
            let reopened = runtime.block_on(store.open_upload(&id)).unwrap();
            done_tx.send(()).unwrap();
            drop(reopened);
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(file);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        thread.join().unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_file_and_directory_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (id, file) = store.create_upload().await.unwrap();
        drop(file);
        let path = store.upload_data(&id).unwrap();
        fs::remove_file(&path).unwrap();
        symlink("/etc/passwd", &path).unwrap();
        assert!(store.open_upload(&id).await.is_err());
        assert!(store.upload_path_for_host(&id).is_err());
        assert!(store.remove_upload(&id).await.is_err());
        fs::remove_file(&path).unwrap();
        let upload = store.upload_dir(&id).unwrap();
        fs::remove_dir(&upload).unwrap();
        symlink(dir.path(), &upload).unwrap();
        assert!(store.open_upload(&id).await.is_err());
        assert!(store.list_uploads().await.is_err());
    }
}
