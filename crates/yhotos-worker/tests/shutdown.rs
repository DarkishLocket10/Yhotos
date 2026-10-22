#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]
//! SIGTERM reaches the production binary while an ingest is running.
use base64::Engine;
use sqlx::{ConnectOptions, PgPool};
use std::{path::Path, time::Duration};
use yhotos_db::{albums, photos};
use yhotos_jobs::{Job, Queue};

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn sigterm_drains_and_releases_a_running_ingest(pool: PgPool) {
    sigterm_during_ingest(pool, Block::Host).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn sigterm_exits_within_thirty_seconds_with_a_blocked_photo_update(pool: PgPool) {
    sigterm_during_ingest(pool, Block::PhotoUpdate).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn sigterm_exits_within_thirty_seconds_with_storage_publication_blocked(pool: PgPool) {
    sigterm_during_ingest(pool, Block::Publication).await;
}

enum Block {
    Host,
    PhotoUpdate,
    Publication,
}

async fn sigterm_during_ingest(pool: PgPool, block: Block) {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let store = yhotos_storage::Store::open(root.path()).unwrap();
    let original = store.put_original(&b"original"[..], 64).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &yhotos_core::Slug::new("signal").unwrap(),
        "Signal",
        yhotos_core::access::Visibility::Public,
    )
    .await
    .unwrap();
    let photo = photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Signal".into(),
            original_hash: original.hash.to_string(),
            original_name: "original".into(),
            original_bytes: 8,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let mut lock = pool.begin().await.unwrap();
    if matches!(block, Block::PhotoUpdate) {
        sqlx::query!("SELECT id FROM photos WHERE id=$1 FOR UPDATE", photo.id.0)
            .fetch_one(&mut *lock)
            .await
            .unwrap();
    }
    // A trusted fake host holds stdin open for the entire shutdown grace period.
    let host = root.path().join("slow-host");
    let publication_lock = if matches!(block, Block::Publication) {
        // Hold the cross-process exclusive lock until the production worker has
        // exited. Its storage spawn_blocking task will wait for a shared lock.
        let file = std::fs::File::open(root.path().join(".publish.lock")).unwrap();
        file.lock().unwrap();
        std::fs::write(root.path().join("output"), b"verified").unwrap();
        let report = serde_json::json!({
            "identity":{"kind":"Jpeg","width":1,"height":1,"orientation":1,
                "bits":8,"pages":1,"has_gain_map":false,"icc_desc":null},
            "metadata":{"make":null,"model":null,"lens":null,"focal_mm":null,
                "aperture":null,"shutter":null,"iso":null,"gps":null,
                "taken_at":null,"offset_time_original":null},
            "thumbhash":base64::engine::general_purpose::STANDARD.encode([0;5]),
            "dominant_colors":[[0,0,0]],
            "outputs":[{"name":"thumb.avif","bytes":8,"width":1,"height":1,
                "blake3":blake3::hash(b"verified").to_hex().as_str()}]
        });
        std::fs::write(root.path().join("report"), report.to_string()).unwrap();
        std::fs::write(&host, b"#!/bin/sh\nroot=${0%/*}\ncat >/dev/null\nfor out in \"$root\"/tmp/*; do cp \"$root/output\" \"$out/thumb.avif\"; done\ncat \"$root/report\"\n").unwrap();
        Some(file)
    } else {
        std::fs::write(&host, b"#!/bin/sh\nexec /bin/sleep 120\n").unwrap();
        None
    };
    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
    let job = Queue::new(pool.clone())
        .enqueue(
            Job::Ingest {
                photo_id: photo.id,
                original_hash: original.hash,
            },
            0,
            None,
        )
        .await
        .unwrap();
    let mut child = spawn_worker(&pool, root.path(), &host);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let running = if matches!(block, Block::Publication) {
                // Both the host output and pipeline's verified snapshot must be
                // complete. Publication is the next await and cannot complete
                // while our exclusive lock remains held.
                std::fs::read_dir(root.path().join("tmp"))
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| std::fs::metadata(entry.path().join("thumb.avif"))
                        .is_ok_and(|meta| meta.len() == 8))
                    .count() == 2
            } else if matches!(block, Block::PhotoUpdate) {
                // Observe the production UPDATE waiting on our held row lock.
                // Keep the lock until after exit: cancelled SQLx connections
                // can still be returning to the pool while the server waits.
                sqlx::query!("SELECT count(*) AS \"count!\" FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE $1", "UPDATE photos SET status='processing'%")
                    .fetch_one(&pool).await.unwrap().count > 0
            } else {
                sqlx::query!(
                    "SELECT status::text AS \"status!\" FROM photos WHERE id=$1",
                    photo.id.0
                )
                .fetch_one(&pool)
                .await
                .unwrap()
                .status == "processing"
            };
            if running {
                break;
            }
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    if publication_lock.is_some() {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(child.try_wait().unwrap().is_none());
    }
    let start = std::time::Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id().unwrap()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(start.elapsed() < Duration::from_secs(30));
    drop(publication_lock);
    lock.rollback().await.unwrap();
    let row = sqlx::query!(
        "SELECT attempts,locked_by,finished_at FROM jobs WHERE id=$1",
        job.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.attempts, 0);
    assert!(row.locked_by.is_none());
    assert!(row.finished_at.is_none());
    assert!(
        std::fs::read_dir(root.path().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}

fn spawn_worker(pool: &PgPool, root: &Path, host: &Path) -> tokio::process::Child {
    let url = pool.connect_options().to_url_lossy();
    let key = base64::engine::general_purpose::STANDARD.encode(format!(
        "{}{}",
        uuid::Uuid::now_v7(),
        uuid::Uuid::now_v7()
    ));
    tokio::process::Command::new(env!("CARGO_BIN_EXE_yhotos-worker"))
        .env_clear()
        .env("YHOTOS_DATABASE_URL", url.as_str())
        .env("YHOTOS_SESSION_KEY", key)
        .env("YHOTOS_PUBLIC_BASE_URL", "https://example.test")
        .env("YHOTOS_ADMIN_LISTEN", "127.0.0.1:8081")
        .env("YHOTOS_DATA_DIR", root)
        .env("YHOTOS_WORKER__CONCURRENCY", "1")
        .env("YHOTOS_WORKER__CODEC_HOST_PATH", host)
        .env("YHOTOS_WORKER__LOCAL_PLAINTEXT_DATABASE", "true")
        .env("YHOTOS_WORKER__METRICS_LISTEN", "127.0.0.1:0")
        .env(
            "YHOTOS_WORKER__VARIANT_PLAN",
            "{variants=[{name=thumb,long_edge=1,square=false,formats=[avif],scale2x=false}]}",
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}
