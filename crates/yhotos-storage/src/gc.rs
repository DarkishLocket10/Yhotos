use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::paths::{Component, safe_join};
use crate::{Area, ContentHash, DerivativeKey, StorageError, Store, blocking};

/// Counts entries actually removed; originals and video are never collected.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    /// Unreferenced, valid derivative files removed.
    pub derivatives_removed: u64,
    /// Expired UUID temporary directories removed.
    pub tmp_dirs_removed: u64,
    /// Expired UUID temporary files removed, including derivative staging files.
    pub tmp_files_removed: u64,
}

impl Store {
    pub(crate) async fn collect(
        &self,
        keep: impl Fn(&ContentHash, &str) -> bool + Send + Sync + 'static,
        tmp_older_than: Duration,
        min_age: Duration,
    ) -> Result<GcReport, StorageError> {
        let mut report = GcReport::default();
        let keep = Arc::new(keep);
        let now = SystemTime::now();
        let mut staging = Vec::new();
        let base = safe_join(&self.root, [Component::area(Area::Derivatives)]);
        for first in self.entries(&base).await? {
            if !is_shard(&first) {
                continue;
            }
            for second in self.entries(&first).await? {
                if !is_shard(&second) {
                    continue;
                }
                for file in self.entries(&second).await? {
                    let Some(name) = file.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if uuid::Uuid::parse_str(name).is_ok() {
                        staging.push(file);
                        continue;
                    }
                    let Some((hash, variant, format, hdr)) = parse_derivative(name) else {
                        continue;
                    };
                    let key = DerivativeKey {
                        hash: &hash,
                        variant,
                        format,
                        hdr,
                    };
                    if self.path_derivative(&key)? != file {
                        continue;
                    }
                    let store = self.clone();
                    let checked = file.clone();
                    let Some(modified) =
                        missing_ok(blocking(move || store.derivative_modified(&checked)).await)?
                    else {
                        continue;
                    };
                    if now.duration_since(modified).unwrap_or_default() <= min_age {
                        continue;
                    }
                    if missing_ok(
                        self.collect_derivative(&file, hash, Arc::clone(&keep), min_age)
                            .await,
                    )? == Some(true)
                    {
                        report.derivatives_removed += 1;
                    }
                }
            }
        }
        self.collect_temporary(staging, now, min_age, None, &mut report)
            .await?;
        let tmp = safe_join(&self.root, [Component::tmp()]);
        self.collect_temporary(
            self.entries(&tmp).await?,
            now,
            tmp_older_than.max(min_age),
            Some(tmp_older_than),
            &mut report,
        )
        .await?;
        Ok(report)
    }

    fn derivative_modified(&self, path: &Path) -> Result<SystemTime, StorageError> {
        self.check_path(path, true)?;
        std::fs::symlink_metadata(path)
            .and_then(|meta| meta.modified())
            .map_err(|e| self.error("derivative age", path, e))
    }

    // This is the second phase of GC: the scan's mtime is only a filter.
    // Re-read it after locking and retain the lock through the keep predicate
    // and unlink. Publishers cannot refresh or publish between those steps.
    async fn collect_derivative(
        &self,
        path: &Path,
        hash: ContentHash,
        keep: Arc<impl Fn(&ContentHash, &str) -> bool + Send + Sync + 'static>,
        min_age: Duration,
    ) -> Result<bool, StorageError> {
        let store = self.clone();
        let path = path.to_path_buf();
        blocking(move || {
            // Never hand a held lock across blocking tasks: a waiting publisher
            // could occupy the last worker ahead of our queued unlink task.
            let _lock = store.lock_publication(std::fs::File::lock)?;
            let modified = store.derivative_modified(&path)?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(StorageError::InvalidName)?;
            if SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                <= min_age
                || keep(&hash, name)
            {
                return Ok(false);
            }
            store.check_path(&path, true)?;
            std::fs::remove_file(&path).map_err(|e| store.error("collect derivative", &path, e))?;
            store.sync_parent(&path)?;
            Ok(true)
        })
        .await
    }

    async fn collect_temporary(
        &self,
        entries: Vec<PathBuf>,
        now: SystemTime,
        file_age: Duration,
        directory_age: Option<Duration>,
        report: &mut GcReport,
    ) -> Result<(), StorageError> {
        for path in entries {
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if uuid::Uuid::parse_str(name).is_err() {
                continue;
            }
            let Some(meta) = missing_ok(
                tokio::fs::symlink_metadata(&path)
                    .await
                    .map_err(|e| self.error("inspect temporary", &path, e)),
            )?
            else {
                continue;
            };
            if meta.is_symlink() {
                return Err(StorageError::InvalidName);
            }
            let age = if meta.is_dir() {
                let Some(age) = directory_age else {
                    continue;
                };
                age
            } else {
                file_age
            };
            let modified = meta
                .modified()
                .map_err(|e| self.error("temporary age", &path, e))?;
            if now.duration_since(modified).unwrap_or_default() <= age {
                continue;
            }
            let store = self.clone();
            let removed = blocking(move || {
                let _lock = store.lock_publication(std::fs::File::lock)?;
                // Staging files can be renamed or refreshed since the scan.
                let meta = std::fs::symlink_metadata(&path)
                    .map_err(|e| store.error("inspect temporary", &path, e))?;
                let age = if meta.is_dir() {
                    let Some(age) = directory_age else {
                        return Ok(None);
                    };
                    age
                } else {
                    file_age
                };
                let modified = meta
                    .modified()
                    .map_err(|e| store.error("temporary age", &path, e))?;
                store.check_path(&path, !meta.is_dir())?;
                if SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    <= age
                {
                    return Ok(None);
                }
                if meta.is_dir() {
                    std::fs::remove_dir_all(&path)
                        .map_err(|e| store.error("collect temporary directory", &path, e))?;
                } else {
                    std::fs::remove_file(&path)
                        .map_err(|e| store.error("collect temporary file", &path, e))?;
                }
                store.sync_parent(&path)?;
                Ok(Some(meta.is_dir()))
            })
            .await;
            match missing_ok(removed)?.flatten() {
                Some(true) => report.tmp_dirs_removed += 1,
                Some(false) => report.tmp_files_removed += 1,
                None => {}
            }
        }
        Ok(())
    }

    async fn entries(&self, dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
        let store = self.clone();
        let checked = dir.to_path_buf();
        match blocking(move || store.check_path(&checked, false)).await {
            Err(StorageError::NotFound) => return Ok(Vec::new()),
            other => other?,
        }
        let Some(mut entries) = missing_ok(
            tokio::fs::read_dir(dir)
                .await
                .map_err(|e| self.error("list directory", dir, e)),
        )?
        else {
            return Ok(Vec::new());
        };
        let mut paths = Vec::new();
        while let Some(entry) = missing_ok(
            entries
                .next_entry()
                .await
                .map_err(|e| self.error("list entry", dir, e)),
        )?
        .flatten()
        {
            let name = entry.file_name();
            let component = Component::parse(name.to_str().ok_or(StorageError::InvalidName)?)?;
            paths.push(safe_join(dir, [component]));
        }
        Ok(paths)
    }
}

fn missing_ok<T>(result: Result<T, StorageError>) -> Result<Option<T>, StorageError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(StorageError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_shard(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
        s.len() == 2
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn parse_derivative(name: &str) -> Option<(ContentHash, &str, &str, bool)> {
    let mut parts = name.split('.');
    let hash = ContentHash::from_str(parts.next()?).ok()?;
    let variant = parts.next()?;
    let format = parts.next()?;
    crate::paths::validate_token(variant).ok()?;
    crate::paths::validate_token(format).ok()?;
    let hdr = match parts.next() {
        None => false,
        Some("hdr") => true,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((hash, variant, format, hdr))
}

impl Store {
    /// Unlinks one queued original only after the grace period and a fresh reference check,
    /// under the publication lock. Re-publication refreshes mtime before releasing that lock.
    /// The callback runs on a blocking worker and must fail closed on database errors.
    /// # Errors
    /// Rejects unsafe storage entries and reports redacted filesystem failures.
    pub async fn collect_original(
        &self,
        hash: ContentHash,
        min_age: Duration,
        keep: impl FnOnce() -> bool + Send + 'static,
    ) -> Result<bool, StorageError> {
        let store = self.clone();
        blocking(move || {
            let _lock = store.lock_publication(std::fs::File::lock)?;
            let path = store.path_original(&hash);
            if !path.exists() {
                return Ok(true);
            }
            store.check_path(&path, true)?;
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map_err(|e| store.error("original age", &path, e))?;
            if SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                <= min_age
                || keep()
            {
                return Ok(false);
            }
            std::fs::remove_file(&path).map_err(|e| store.error("collect original", &path, e))?;
            store.sync_parent(&path)?;
            Ok(true)
        })
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;

    use super::*;

    // Invoked by the regression below in a fresh process, with an independent
    // Store and lock descriptor. An ordinary test run has no child root.
    #[tokio::test]
    async fn republish_derivative_in_child() {
        let Some(root) = std::env::var_os("YHOTOS_GC_REPUBLISH_TEST_ROOT") else {
            return;
        };
        let store = Store::open(root).unwrap();
        let hash = ContentHash(*blake3::hash(b"encoded").as_bytes());
        let key = DerivativeKey {
            hash: &hash,
            variant: "small",
            format: "avif",
            hdr: false,
        };
        let dir = store.tmp_dir().unwrap();
        let src = dir.path().join("output");
        fs::write(&src, b"encoded").unwrap();
        store.put_derivative(&key, &src).await.unwrap();
        assert!(!src.exists());
    }

    #[tokio::test]
    async fn gc_rechecks_age_after_cross_process_republish_between_scan_and_delete() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let hash = ContentHash(*blake3::hash(b"encoded").as_bytes());
        let key = DerivativeKey {
            hash: &hash,
            variant: "small",
            format: "avif",
            hdr: false,
        };
        let path = store.path_derivative(&key).unwrap();
        store.ensure_dirs(path.parent().unwrap()).unwrap();
        fs::write(&path, b"encoded").unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();

        // Deterministically pause the production two-phase GC sequence after
        // its scan read an old mtime, before its locked final delete phase.
        let scanned = store.derivative_modified(&path).unwrap();
        assert_eq!(scanned, SystemTime::UNIX_EPOCH);
        let min_age = Duration::from_secs(3600);
        assert!(SystemTime::now().duration_since(scanned).unwrap() > min_age);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::tests::republish_derivative_in_child"])
            .env("YHOTOS_GC_REPUBLISH_TEST_ROOT", root.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");

        assert!(store.derivative_modified(&path).unwrap() > scanned);
        let removed = store
            .collect_derivative(
                &path,
                hash,
                Arc::new(|_: &ContentHash, _: &str| false),
                min_age,
            )
            .await
            .unwrap();
        assert!(!removed);
        assert_eq!(fs::read(&path).unwrap(), b"encoded");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn gc_keeps_exclusive_lock_through_reference_check_and_releases_per_entry() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let hash = ContentHash(*blake3::hash(b"encoded").as_bytes());
        let mut paths = Vec::new();
        for variant in ["small", "large"] {
            let path = store
                .path_derivative(&DerivativeKey {
                    hash: &hash,
                    variant,
                    format: "avif",
                    hdr: false,
                })
                .unwrap();
            store.ensure_dirs(path.parent().unwrap()).unwrap();
            fs::write(&path, b"encoded").unwrap();
            fs::File::open(&path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH)
                .unwrap();
            paths.push(path);
        }
        let publisher = store.clone();
        let keep = Arc::new(move |_: &ContentHash, name: &str| {
            // A publisher cannot enter while the final keep predicate runs.
            let contender = publisher.open_publish_lock().unwrap();
            assert!(matches!(
                contender.try_lock_shared(),
                Err(fs::TryLockError::WouldBlock)
            ));
            name.contains(".large.")
        });
        for path in &paths {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                store
                    .collect_derivative(path, hash, Arc::clone(&keep), Duration::ZERO)
                    .await
                    .unwrap(),
                name.contains(".small.")
            );
            // Both the retain and delete paths release before the next entry.
            let publisher = store.open_publish_lock().unwrap();
            publisher.try_lock_shared().unwrap();
            publisher.unlock().unwrap();
        }
        assert!(!paths.first().unwrap().exists());
        assert!(paths.last().unwrap().exists());
    }

    #[test]
    fn gc_with_one_blocking_thread_completes_with_competing_publisher() {
        use std::sync::mpsc;

        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let hash = ContentHash(*blake3::hash(b"encoded").as_bytes());
        let key = DerivativeKey {
            hash: &hash,
            variant: "small",
            format: "avif",
            hdr: false,
        };
        let path = store.path_derivative(&key).unwrap();
        store.ensure_dirs(path.parent().unwrap()).unwrap();
        fs::write(&path, b"encoded").unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        let dir = store.tmp_dir().unwrap();
        let source = dir.path().join("output");
        fs::write(&source, b"encoded").unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let publisher = store.clone();
        let destination = path.clone();
        let (published_tx, published_rx) = mpsc::channel();
        let (collected_tx, collected_rx) = mpsc::channel();
        runtime.spawn(async move {
            let result = store
                .gc(
                    move |_, _| {
                        let publisher = publisher.clone();
                        let source = source.clone();
                        let destination = destination.clone();
                        let published_tx = published_tx.clone();
                        // Queue exactly one publisher while GC holds the lock.
                        // Splitting GC into lock/unlink tasks would put this
                        // waiting publisher ahead of unlink on the sole worker.
                        handle.spawn_blocking(move || {
                            let removed = !destination.exists();
                            let result = publisher.move_derivative(&source, &destination);
                            let _ = published_tx.send((removed, result));
                        });
                        false
                    },
                    Duration::from_hours(24),
                    Duration::from_secs(3600),
                )
                .await;
            let _ = collected_tx.send(result);
        });

        // Timeouts and shutdown run outside the runtime so a blocking-pool
        // deadlock fails promptly instead of hanging the test suite.
        let collected = collected_rx.recv_timeout(Duration::from_secs(10));
        let outcome = published_rx.recv_timeout(Duration::from_secs(10));
        runtime.shutdown_timeout(Duration::from_secs(1));
        assert_eq!(
            collected.unwrap().unwrap(),
            GcReport {
                derivatives_removed: 1,
                ..GcReport::default()
            }
        );
        let (removed, result) = outcome.unwrap();
        assert!(removed, "publisher ran before GC released its entry lock");
        result.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"encoded");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(matches!(
            published_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn gc_waits_for_publication_of_an_old_source_paused_after_staging() {
        use std::sync::mpsc;

        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let hash = ContentHash(*blake3::hash(b"encoded").as_bytes());
        let key = DerivativeKey {
            hash: &hash,
            variant: "small",
            format: "avif",
            hdr: false,
        };
        let destination = store.path_derivative(&key).unwrap();
        let dir = store.tmp_dir().unwrap();
        let source = dir.path().join("output");
        fs::write(&source, b"encoded").unwrap();
        fs::File::open(&source)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        let (staged_tx, staged_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let publisher = store.clone();
        let dest = destination.clone();
        let publishing = std::thread::spawn(move || {
            publisher.move_derivative_with(&source, &dest, |from, to| {
                fs::rename(from, to)?;
                staged_tx.send(to.to_path_buf()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            })
        });
        let staged = staged_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // Renaming preserved the old mtime: GC's scan will select this file.
        assert_eq!(
            fs::metadata(&staged).unwrap().modified().unwrap(),
            SystemTime::UNIX_EPOCH
        );
        assert!(
            store
                .entries(staged.parent().unwrap())
                .await
                .unwrap()
                .contains(&staged)
        );
        let collector = Store::open(root.path()).unwrap();
        let contender = collector.open_publish_lock().unwrap();
        let lock_result = contender.try_lock();
        // Do not retain the probe lock if the regression is present.
        drop(contender);
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let collecting = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            started_tx.send(()).unwrap();
            let result = runtime.block_on(collector.gc(
                |_, _| false,
                Duration::from_hours(24),
                Duration::from_secs(3600),
            ));
            finished_tx.send(result).unwrap();
        });
        // Let GC run while the old staged inode is paused. It must wait for
        // publication; it may not remove the staging file before the refresh.
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let during_staging = finished_rx.recv_timeout(Duration::from_millis(200));
        let gc_waited = matches!(&during_staging, Err(mpsc::RecvTimeoutError::Timeout));
        resume_tx.send(()).unwrap();
        let outcome = publishing.join().unwrap();
        let report = match during_staging {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                finished_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            }
            Err(error) => panic!("collector disconnected: {error}"),
        };
        collecting.join().unwrap();

        assert!(matches!(lock_result, Err(fs::TryLockError::WouldBlock)));
        assert!(gc_waited, "GC finished while the old source was staged");
        outcome.unwrap();
        assert_eq!(report.unwrap(), GcReport::default());
        assert!(!staged.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"encoded");
        assert!(store.derivative_modified(&destination).unwrap() > SystemTime::UNIX_EPOCH);
        assert_eq!(
            fs::read_dir(destination.parent().unwrap()).unwrap().count(),
            1
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn temporary_snapshot_tolerates_concurrent_publish_and_directory_drop() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let (file, published) = store.new_temp().unwrap();
        drop(file);
        let dropped = store.tmp_dir().unwrap();
        let old_dir = store.tmp_dir().unwrap().keep();
        let (old_file, old_temp) = store.new_temp().unwrap();
        old_file
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        drop(old_file);
        fs::File::open(&old_dir)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();

        let entries = store.entries(&store.root.join("tmp")).await.unwrap();
        assert_eq!(entries.len(), 4);
        let other = store.clone();
        std::thread::spawn(move || {
            // Both disappear after enumeration and before metadata inspection.
            let hash = ContentHash::from_str(&"ab".repeat(32)).unwrap();
            other
                .publish(published, &other.path_original(&hash), false)
                .unwrap();
            drop(dropped);
        })
        .join()
        .unwrap();

        let mut report = GcReport::default();
        store
            .collect_temporary(
                entries,
                SystemTime::now(),
                Duration::from_secs(3600),
                Some(Duration::from_secs(3600)),
                &mut report,
            )
            .await
            .unwrap();
        assert_eq!(report.tmp_dirs_removed, 1);
        assert_eq!(report.tmp_files_removed, 1);
        assert_eq!(report.derivatives_removed, 0);
        assert!(!old_dir.exists());
        assert!(!old_temp.exists());
        assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    }
}
