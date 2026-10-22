#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn rejects_unsafe_components() {
    for bad in [
        "..",
        ".",
        "/absolute",
        "",
        "a/b",
        "a\\b",
        "bad%20",
        "a\0b",
        "é",
        "a\nb",
        "C:drive",
        &"x".repeat(129),
    ] {
        assert!(Component::parse(bad).is_err(), "accepted {bad:?}");
    }
    assert!(Component::parse(&"x".repeat(128)).is_ok());
    assert!(Component::parse("Allowed-Name_1.2").is_ok());
}

#[test]
fn rejects_invalid_hashes() {
    for bad in [
        "A".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
        "é".repeat(32),
        String::new(),
    ] {
        assert!(matches!(
            bad.parse::<ContentHash>(),
            Err(StorageError::InvalidHash)
        ));
    }
    let valid = "0123456789abcdef".repeat(4);
    assert_eq!(valid.parse::<ContentHash>().unwrap().to_string(), valid);
}

fn setup() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let store = Store::open(root.path()).unwrap();
    (root, store)
}

fn hash(bytes: &[u8]) -> ContentHash {
    ContentHash(*blake3::hash(bytes).as_bytes())
}

#[test]
fn validates_keys_and_layout() {
    let (_root, store) = setup();
    let hash: ContentHash = "0123456789abcdef".repeat(4).parse().unwrap();
    assert_eq!(
        store.path_original(&hash),
        store.root.join(format!("originals/01/23/{hash}"))
    );
    let key = DerivativeKey {
        hash: &hash,
        variant: "medium",
        format: "avif",
        hdr: true,
    };
    assert_eq!(
        store.path_derivative(&key).unwrap(),
        store
            .root
            .join(format!("derivatives/01/23/{hash}.medium.avif.hdr"))
    );
    assert_eq!(
        store.path_video(&hash, "h264").unwrap(),
        store.root.join(format!("video/01/23/{hash}.h264.mp4"))
    );
    for bad in [
        "..",
        "",
        "/absolute",
        "UPPER",
        "bad-name",
        "bad.format",
        "é",
        &"a".repeat(17),
    ] {
        assert!(
            store
                .path_derivative(&DerivativeKey {
                    variant: bad,
                    ..key
                })
                .is_err()
        );
        assert!(
            store
                .path_derivative(&DerivativeKey { format: bad, ..key })
                .is_err()
        );
        assert!(store.path_video(&hash, bad).is_err());
    }
}

#[test]
fn root_must_exist_and_be_directory() {
    let (root, _) = setup();
    assert!(matches!(
        Store::open(root.path().join("missing")),
        Err(StorageError::NotFound)
    ));
    let file = root.path().join("file");
    fs::write(&file, []).unwrap();
    assert!(matches!(Store::open(file), Err(StorageError::InvalidName)));
}

#[cfg(unix)]
#[tokio::test]
async fn opens_and_reads_read_only_root_without_creating_lock_or_entries() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    // Prepare existing content without opening a Store or initializing its lock.
    let fixture = Store {
        root: fs::canonicalize(root.path()).unwrap(),
    };
    let hash = hash(b"existing original");
    let path = fixture.path_original(&hash);
    fixture.ensure_dirs(path.parent().unwrap()).unwrap();
    fs::write(&path, b"existing original").unwrap();
    let permissions = fs::metadata(root.path()).unwrap().permissions();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o550)).unwrap();

    let opened = match Store::open(root.path()) {
        Ok(store) => store.open_read(Area::Originals, &hash, None).await,
        Err(error) => Err(error),
    };
    // Restore write access even when open fails, so the fixture can be removed.
    fs::set_permissions(root.path(), permissions).unwrap();
    let mut file = opened.unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, b"existing original");
    assert!(!root.path().join(".publish.lock").exists());
    let entries: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["originals"]);
}

#[test]
fn publication_lock_probe_in_child() {
    let Some(root) = std::env::var_os("YHOTOS_FLOCK_TEST_ROOT") else {
        return;
    };
    let state = std::env::var("YHOTOS_FLOCK_TEST_STATE").unwrap();
    let store = Store::open(root).unwrap();
    for shared in [true, false] {
        let file = store.open_publish_lock().unwrap();
        let (result, blocked) = if shared {
            (file.try_lock_shared(), state == "exclusive")
        } else {
            (file.try_lock(), state != "unlocked")
        };
        if blocked {
            assert!(matches!(result, Err(fs::TryLockError::WouldBlock)));
        } else {
            result.unwrap();
            file.unlock().unwrap();
        }
    }
}

#[test]
fn publication_locks_coordinate_clones_stores_and_processes() {
    let (_root, store) = setup();
    let clone = store.clone();
    let reopened = Store::open(&store.root).unwrap();
    let probe = |state| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::publication_lock_probe_in_child"])
            .env("YHOTOS_FLOCK_TEST_ROOT", &store.root)
            .env("YHOTOS_FLOCK_TEST_STATE", state)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    let first = store.lock_publication(fs::File::lock_shared).unwrap();
    let second = clone.lock_publication(fs::File::lock_shared).unwrap();
    let third = reopened.lock_publication(fs::File::lock_shared).unwrap();
    drop(first);
    drop(third);
    probe("shared");
    let contender = store.open_publish_lock().unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(fs::TryLockError::WouldBlock)
    ));
    drop(second);
    let exclusive = reopened.lock_publication(fs::File::lock).unwrap();
    probe("exclusive");
    drop(exclusive);
    probe("unlocked");
}

#[cfg(unix)]
#[test]
fn publication_lock_rejects_symlinks_and_is_private() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (_root, store) = setup();
    let path = store.root.join(".publish.lock");
    assert!(!path.exists());
    drop(store.open_publish_lock().unwrap());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    fs::remove_file(&path).unwrap();
    symlink(store.root.join("missing"), &path).unwrap();
    assert!(matches!(
        Store::open(&store.root).unwrap().open_publish_lock(),
        Err(StorageError::InvalidName)
    ));
    assert!(matches!(
        store.lock_publication(fs::File::lock_shared),
        Err(StorageError::InvalidName)
    ));
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        Store::open(&store.root).unwrap().open_publish_lock(),
        Err(StorageError::InvalidName)
    ));
}

#[tokio::test]
async fn originals_dedupe_and_stream_seek() {
    use tokio::io::AsyncSeekExt;
    let (_root, store) = setup();
    assert!(!store.root.join(".publish.lock").exists());
    let bytes = vec![0x5a; BUFFER_BYTES * 3 + 7];
    let a = store
        .put_original(bytes.as_slice(), bytes.len() as u64)
        .await
        .unwrap();
    assert!(store.root.join(".publish.lock").is_file());
    let b = store
        .put_original(bytes.as_slice(), bytes.len() as u64)
        .await
        .unwrap();
    assert_eq!(a.hash, hash(&bytes));
    assert_eq!(a.hash, b.hash);
    assert!(!a.existed);
    assert!(b.existed);
    assert_eq!(a.bytes, bytes.len() as u64);
    assert_eq!(
        fs::read_dir(store.path_original(&a.hash).parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    let mut file = store
        .open_read(Area::Originals, &a.hash, None)
        .await
        .unwrap();
    file.seek(io::SeekFrom::Start(bytes.len() as u64 - 7))
        .await
        .unwrap();
    let mut tail = [0; 7];
    file.read_exact(&mut tail).await.unwrap();
    assert_eq!(tail, [0x5a; 7]);
    assert!(store.exists(Area::Originals, &a.hash, None).await);
    store.remove(Area::Originals, &a.hash, None).await.unwrap();
    assert!(!store.exists(Area::Originals, &a.hash, None).await);
    assert!(matches!(
        store.remove(Area::Originals, &a.hash, None).await,
        Err(StorageError::NotFound)
    ));
}

#[tokio::test]
async fn duplicate_original_refreshes_publication_time() {
    let (_root, store) = setup();
    let bytes = b"original";
    let first = store.put_original(&bytes[..], 8).await.unwrap();
    let path = store.path_original(&first.hash);
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();

    let duplicate = store.put_original(&bytes[..], 8).await.unwrap();
    assert!(duplicate.existed);
    assert_eq!(duplicate.hash, first.hash);
    assert!(
        fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            < Duration::from_secs(3600)
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
}

#[tokio::test]
async fn size_limit_cleans_temporary_file() {
    let (_root, store) = setup();
    for (bytes, max) in [
        (vec![1; BUFFER_BYTES + 1], BUFFER_BYTES as u64),
        (vec![1], 0),
    ] {
        assert!(
            matches!(store.put_original(bytes.as_slice(), max).await, Err(StorageError::TooLarge { max: m }) if m == max)
        );
        assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    }
    let empty = store.put_original(&b""[..], 0).await.unwrap();
    assert_eq!(empty.hash, hash(&[]));
    assert_eq!(empty.bytes, 0);
}

#[tokio::test]
async fn concurrent_originals_publish_once() {
    let (_root, store) = setup();
    let other = store.clone();
    let first = tokio::spawn(async move {
        other
            .put_original(&vec![8; BUFFER_BYTES * 3][..], u64::MAX)
            .await
    });
    let other = store.clone();
    let second = tokio::spawn(async move {
        other
            .put_original(&vec![8; BUFFER_BYTES * 3][..], u64::MAX)
            .await
    });
    let (a, b) = (
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap(),
    );
    assert_eq!(a.hash, b.hash);
    assert_ne!(a.existed, b.existed);
    assert_eq!(
        fs::read_dir(store.path_original(&a.hash).parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
}

#[tokio::test]
async fn final_write_failure_never_publishes_and_cleans_temporary_file() {
    let (_root, store) = setup();
    let (file, temp) = store.new_temp().unwrap();
    drop(file);
    // Tokio accepts the final chunk into its buffer before the read-only
    // descriptor fails in the blocking write. The flush must surface that error.
    let file = fs::File::open(&temp).unwrap();
    let bytes = b"final chunk";
    let error = store
        .put_original_into(&bytes[..], bytes.len() as u64, file, temp)
        .await
        .unwrap_err();
    assert!(matches!(&error, StorageError::Io(_)));
    assert!(error.to_string().contains("flush temporary"));
    assert!(!store.path_original(&hash(bytes)).exists());
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    let put = store
        .put_original(&bytes[..], bytes.len() as u64)
        .await
        .unwrap();
    assert!(!put.existed);
    assert_eq!(fs::read(store.path_original(&put.hash)).unwrap(), bytes);
}

#[tokio::test]
async fn derivative_ingestion_accepts_spaces_and_unicode_in_trusted_root() {
    let (parent, _) = setup();
    for name in ["photo library", "写真", "photos été"] {
        let root = parent.path().join(name);
        fs::create_dir(&root).unwrap();
        let store = Store::open(&root).unwrap();
        let dir = store.tmp_dir().unwrap();
        let source = dir.path().join("output");
        fs::write(&source, b"encoded").unwrap();
        let hash = hash(b"encoded");
        let key = DerivativeKey {
            hash: &hash,
            variant: "small",
            format: "avif",
            hdr: false,
        };
        store.put_derivative(&key, &source).await.unwrap();
        assert!(!source.exists());
        assert_eq!(
            fs::read(store.path_derivative(&key).unwrap()).unwrap(),
            b"encoded"
        );

        fs::write(&source, b"encoded").unwrap();
        let traversal = dir
            .path()
            .join("..")
            .join(dir.path().file_name().unwrap())
            .join("output");
        assert!(store.put_derivative(&key, &traversal).await.is_err());
        let invalid = dir.path().join("bad name");
        fs::write(&invalid, b"encoded").unwrap();
        assert!(store.put_derivative(&key, &invalid).await.is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&source, &link).unwrap();
            assert!(store.put_derivative(&key, &link).await.is_err());
        }
        assert!(source.exists());
    }
}

#[tokio::test]
async fn derivative_ingestion_rejects_published_sources_without_mutation() {
    let (_root, store) = setup();
    let put = store.put_original(&b"encoded"[..], 7).await.unwrap();
    let key = DerivativeKey {
        hash: &put.hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    let original = store.path_original(&put.hash);
    let derivative = store
        .path_derivative(&DerivativeKey {
            variant: "large",
            ..key
        })
        .unwrap();
    let video = store.path_video(&put.hash, "h264").unwrap();
    for src in [&original, &derivative, &video] {
        store.ensure_dirs(src.parent().unwrap()).unwrap();
        fs::write(src, b"encoded").unwrap();
        let before = fs::metadata(src).unwrap();
        assert!(matches!(
            store.put_derivative(&key, src).await,
            Err(StorageError::InvalidName)
        ));
        let after = fs::metadata(src).unwrap();
        assert_eq!(fs::read(src).unwrap(), b"encoded");
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(before.permissions(), after.permissions());
        assert!(!store.path_derivative(&key).unwrap().exists());
    }
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
}

#[tokio::test]
async fn derivative_ingestion_rejects_sources_outside_the_root() {
    let (_root, store) = setup();
    let (_outside_root, outside) = setup();
    let src = outside.root.join("output");
    fs::write(&src, b"encoded").unwrap();
    let hash = hash(b"encoded");
    let key = DerivativeKey {
        hash: &hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    assert!(matches!(
        store.put_derivative(&key, &src).await,
        Err(StorageError::InvalidName)
    ));
    assert_eq!(fs::read(&src).unwrap(), b"encoded");
    assert!(!store.path_derivative(&key).unwrap().exists());
}

#[tokio::test]
async fn derivative_move_temp_lifecycle_and_permissions() {
    let (_root, store) = setup();
    let dir = store.tmp_dir().unwrap();
    let path = dir.path().to_path_buf();
    assert!(uuid::Uuid::parse_str(path.file_name().unwrap().to_str().unwrap()).is_ok());
    let src = dir.path().join("output");
    fs::write(&src, b"encoded").unwrap();
    let hash = hash(b"encoded");
    let key = DerivativeKey {
        hash: &hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    store.put_derivative(&key, &src).await.unwrap();
    assert!(!src.exists());
    assert!(store.exists(Area::Derivatives, &hash, Some(&key)).await);
    assert_eq!(
        fs::read(store.path_derivative(&key).unwrap()).unwrap(),
        b"encoded"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dest = store.path_derivative(&key).unwrap();
        assert_eq!(
            fs::metadata(dest).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
    drop(dir);
    assert!(!path.exists());
    let kept = store.tmp_dir().unwrap().keep();
    assert!(kept.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_symlinks_at_every_component() {
    use std::os::unix::fs::symlink;
    for depth in 0..4 {
        let (_root, store) = setup();
        let put = store.put_original(&b"bytes"[..], 5).await.unwrap();
        let file = store.path_original(&put.hash);
        let components: Vec<_> = file
            .strip_prefix(&store.root)
            .unwrap()
            .components()
            .collect();
        let path: PathBuf = store
            .root
            .join(components.iter().take(depth + 1).collect::<PathBuf>());
        let moved = store.root.join("moved");
        fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert!(matches!(
            store.open_read(Area::Originals, &put.hash, None).await,
            Err(StorageError::InvalidName)
        ));
        assert!(matches!(
            store.remove(Area::Originals, &put.hash, None).await,
            Err(StorageError::InvalidName)
        ));
        assert!(store.put_original(&b"bytes"[..], 5).await.is_err());
        assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn gc_keeps_references_and_removes_only_expired_temporary_entries() {
    let (_root, store) = setup();
    let hash = hash(b"original");
    let old = store.tmp_dir().unwrap().keep();
    fs::File::create(old.join("partial"))
        .unwrap()
        .set_times(fs::FileTimes::new())
        .unwrap();
    let old_file = store
        .root
        .join("tmp")
        .join(uuid::Uuid::now_v7().to_string());
    fs::File::create(&old_file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    fs::File::open(&old)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    let recent = store.tmp_dir().unwrap().keep();
    let source = recent.join("output");
    for variant in ["small", "large"] {
        fs::write(&source, b"encoded").unwrap();
        store
            .put_derivative(
                &DerivativeKey {
                    hash: &hash,
                    variant,
                    format: "avif",
                    hdr: false,
                },
                &source,
            )
            .await
            .unwrap();
        let path = store
            .path_derivative(&DerivativeKey {
                hash: &hash,
                variant,
                format: "avif",
                hdr: false,
            })
            .unwrap();
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
    }
    let report = store
        .gc(
            move |h, name| h == &hash && name.contains(".large."),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    assert_eq!(
        report,
        GcReport {
            derivatives_removed: 1,
            tmp_dirs_removed: 1,
            tmp_files_removed: 1
        }
    );
    assert!(!old.exists());
    assert!(!old_file.exists());
    assert!(recent.exists());
    let large = DerivativeKey {
        hash: &hash,
        variant: "large",
        format: "avif",
        hdr: false,
    };
    assert!(store.exists(Area::Derivatives, &hash, Some(&large)).await);
    assert!(
        !store
            .exists(
                Area::Derivatives,
                &hash,
                Some(&DerivativeKey {
                    variant: "small",
                    ..large
                })
            )
            .await
    );
}

#[tokio::test]
async fn gc_retains_unreferenced_derivatives_until_min_age() {
    let (_root, store) = setup();
    let dir = store.tmp_dir().unwrap();
    let src = dir.path().join("output");
    fs::write(&src, b"encoded").unwrap();
    // A codec output may have waited before publication. Its old mtime must
    // not make a newly published derivative eligible for collection.
    fs::File::open(&src)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    let hash = hash(b"encoded");
    let key = DerivativeKey {
        hash: &hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    store.put_derivative(&key, &src).await.unwrap();
    let path = store.path_derivative(&key).unwrap();
    let tmp_older_than = Duration::from_hours(24);
    let min_age = Duration::from_secs(3600);
    let report = store
        .gc(|_, _| false, tmp_older_than, min_age)
        .await
        .unwrap();
    assert_eq!(report, GcReport::default());
    assert_eq!(fs::read(&path).unwrap(), b"encoded");

    // A future mtime must also be retained if the wall clock moves backwards.
    let file = fs::File::open(&path).unwrap();
    file.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now() + min_age))
        .unwrap();
    let report = store
        .gc(|_, _| false, tmp_older_than, min_age)
        .await
        .unwrap();
    assert_eq!(report, GcReport::default());
    assert!(path.exists());

    file.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    let report = store
        .gc(|_, _| false, tmp_older_than, min_age)
        .await
        .unwrap();
    assert_eq!(
        report,
        GcReport {
            derivatives_removed: 1,
            ..GcReport::default()
        }
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn duplicate_derivative_restarts_gc_min_age() {
    let (_root, store) = setup();
    let dir = store.tmp_dir().unwrap();
    let src = dir.path().join("output");
    let hash = hash(b"encoded");
    let key = DerivativeKey {
        hash: &hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    fs::write(&src, b"encoded").unwrap();
    store.put_derivative(&key, &src).await.unwrap();
    let path = store.path_derivative(&key).unwrap();
    // An old orphan may be reused by a retry before its row is committed.
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    fs::write(&src, b"encoded").unwrap();
    store.put_derivative(&key, &src).await.unwrap();
    assert!(!src.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

    let report = store
        .gc(
            |_, _| false,
            Duration::from_hours(24),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    assert_eq!(report, GcReport::default());
    assert_eq!(fs::read(&path).unwrap(), b"encoded");
}

#[tokio::test]
async fn gc_ages_temporary_directories_independently_of_staging_files() {
    let (_root, store) = setup();
    let dir = store.tmp_dir().unwrap().keep();
    let (file, staging) = store.new_temp().unwrap();
    let two_hours_ago = std::time::SystemTime::now() - Duration::from_hours(2);
    let times = fs::FileTimes::new().set_modified(two_hours_ago);
    fs::File::open(&dir).unwrap().set_times(times).unwrap();
    file.set_times(times).unwrap();
    drop(file);
    let report = store
        .gc(
            |_, _| false,
            Duration::from_secs(3600),
            Duration::from_hours(3),
        )
        .await
        .unwrap();
    assert_eq!(
        report,
        GcReport {
            tmp_dirs_removed: 1,
            ..GcReport::default()
        }
    );
    assert!(!dir.exists());
    assert!(staging.exists());
}

#[tokio::test]
async fn gc_expires_abandoned_derivative_staging_files() {
    let (_root, store) = setup();
    let hash = hash(b"original");
    let key = DerivativeKey {
        hash: &hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    let dest = store.path_derivative(&key).unwrap();
    let shard = dest.parent().unwrap();
    store.ensure_dirs(shard).unwrap();
    let old = shard.join(uuid::Uuid::now_v7().to_string());
    let recent = shard.join(uuid::Uuid::now_v7().to_string());
    let unknown = shard.join("unrecognized");
    for path in [&old, &recent, &unknown, &dest] {
        fs::write(path, b"staged bytes").unwrap();
    }
    for path in [&old, &unknown] {
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
    }
    assert!(!store.root.join(".publish.lock").exists());
    let report = store
        .gc(|_, _| false, Duration::ZERO, Duration::from_secs(3600))
        .await
        .unwrap();
    assert!(store.root.join(".publish.lock").is_file());
    assert_eq!(report.tmp_files_removed, 1);
    assert_eq!(report.derivatives_removed, 0);
    assert_eq!(report.tmp_dirs_removed, 0);
    assert!(!old.exists());
    assert!(recent.exists());
    assert!(unknown.exists());
    assert!(dest.exists());
}

#[tokio::test]
async fn gc_tolerates_derivative_removal_after_inspection() {
    let (_root, store) = setup();
    let hash = hash(b"original");
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
        paths.push(path);
    }
    let inspected_paths = paths.clone();
    let report = store
        .gc(
            move |_, name| {
                if name.contains(".small.") {
                    let path = inspected_paths
                        .iter()
                        .find(|path| path.file_name().unwrap() == name)
                        .unwrap()
                        .clone();
                    // Run the competing deletion after GC's inspection and before its
                    // own removal, without relying on filesystem iteration order.
                    std::thread::spawn(move || fs::remove_file(path).unwrap())
                        .join()
                        .unwrap();
                }
                false
            },
            Duration::ZERO,
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert_eq!(report.derivatives_removed, 1);
    assert!(paths.iter().all(|path| !path.exists()));
}

#[test]
fn io_errors_redact_paths_and_original_messages() {
    let (_root, store) = setup();
    let error = store.error(
        "write",
        &store.root.join("tmp/output"),
        io::Error::other("private /secret/path"),
    );
    let text = format!("{error:?}: {error}");
    assert!(text.contains("write at tmp/output"));
    assert!(!text.contains("secret"));
    assert!(!text.contains(&store.root.to_string_lossy().to_string()));
}

#[tokio::test]
async fn input_error_and_cancellation_clean_temporary_files() {
    use std::pin::Pin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    struct Fail;
    impl AsyncRead for Fail {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("untrusted stream details")))
        }
    }
    struct Wait {
        polls: Arc<AtomicUsize>,
    }
    impl AsyncRead for Wait {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Poll::Pending
        }
    }
    let (_root, store) = setup();
    let error = store.put_original(Fail, 10).await.unwrap_err();
    assert!(!error.to_string().contains("untrusted stream details"));
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    let polls = Arc::new(AtomicUsize::new(0));
    let worker_store = store.clone();
    let reader = Wait {
        polls: polls.clone(),
    };
    let task = tokio::spawn(async move { worker_store.put_original(reader, 10).await });
    while polls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
}

#[test]
fn derivative_cross_device_fallback_copies_syncs_and_removes_source() {
    let (_root, store) = setup();
    let dir = store.tmp_dir().unwrap();
    let src = dir.path().join("output");
    let bytes = vec![7; BUFFER_BYTES * 2 + 19];
    fs::write(&src, &bytes).unwrap();
    let hash = hash(&bytes);
    let key = DerivativeKey {
        hash: &hash,
        variant: "large",
        format: "jxl",
        hdr: true,
    };
    let dest = store.path_derivative(&key).unwrap();
    store
        .move_derivative_with(
            &src,
            &dest,
            |_, _| Err(io::ErrorKind::CrossesDevices.into()),
        )
        .unwrap();
    assert!(!src.exists());
    assert_eq!(fs::read(&dest).unwrap(), bytes);
    assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 1);
}

#[tokio::test]
async fn typed_selectors_reject_missing_or_mismatched_keys() {
    let (_root, store) = setup();
    let a = hash(b"a");
    let b = hash(b"b");
    let key = DerivativeKey {
        hash: &a,
        variant: "h264",
        format: "mp4",
        hdr: false,
    };
    assert!(matches!(
        store.open_read(Area::Derivatives, &a, None).await,
        Err(StorageError::InvalidName)
    ));
    assert!(matches!(
        store.open_read(Area::Derivatives, &b, Some(&key)).await,
        Err(StorageError::InvalidName)
    ));
    assert!(matches!(
        store.open_read(Area::Originals, &a, Some(&key)).await,
        Err(StorageError::InvalidName)
    ));
    assert!(matches!(
        store
            .open_read(Area::Video, &a, Some(&DerivativeKey { hdr: true, ..key }))
            .await,
        Err(StorageError::InvalidName)
    ));
    let video = store.path_video(&a, "h264").unwrap();
    store.ensure_dirs(video.parent().unwrap()).unwrap();
    fs::write(&video, b"video").unwrap();
    assert!(store.exists(Area::Video, &a, Some(&key)).await);
    store.remove(Area::Video, &a, Some(&key)).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlinked_temp_and_codec_output_and_gc_entries() {
    use std::os::unix::fs::symlink;
    let (_root, store) = setup();
    let other = store.root.join("other");
    fs::create_dir(&other).unwrap();
    let tmp = store.root.join("tmp");
    symlink(&other, &tmp).unwrap();
    assert!(matches!(
        store.put_original(&b"test"[..], 4).await,
        Err(StorageError::InvalidName)
    ));
    assert!(matches!(store.tmp_dir(), Err(StorageError::InvalidName)));
    assert!(matches!(
        store.gc(|_, _| true, Duration::ZERO, Duration::ZERO).await,
        Err(StorageError::InvalidName)
    ));
    fs::remove_file(&tmp).unwrap();
    let dir = store.tmp_dir().unwrap();
    let real = dir.path().join("real");
    fs::write(&real, b"output").unwrap();
    let src = dir.path().join("link");
    symlink(&real, &src).unwrap();
    let hash = hash(b"output");
    let key = DerivativeKey {
        hash: &hash,
        variant: "small",
        format: "avif",
        hdr: false,
    };
    assert!(store.put_derivative(&key, &src).await.is_err());
    assert!(real.exists());
    assert!(!store.path_derivative(&key).unwrap().exists());
    fs::remove_file(&src).unwrap();
    store.put_derivative(&key, &real).await.unwrap();
    let dest = store.path_derivative(&key).unwrap();
    fs::remove_file(&dest).unwrap();
    symlink(dir.path(), &dest).unwrap();
    assert!(matches!(
        store.gc(|_, _| false, Duration::ZERO, Duration::ZERO).await,
        Err(StorageError::InvalidName)
    ));
    assert!(dir.path().exists());
}

#[cfg(unix)]
#[tokio::test]
async fn original_files_and_all_created_directories_have_restricted_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, store) = setup();
    let put = store.put_original(&b"permissions"[..], 11).await.unwrap();
    let path = store.path_original(&put.hash);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let mut dir = path.parent().unwrap();
    while dir != store.root {
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o750
        );
        dir = dir.parent().unwrap();
    }
    assert_eq!(
        fs::metadata(store.root.join("tmp"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
}
