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
    sigterm_during_ingest(pool, false).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn sigterm_exits_within_thirty_seconds_with_a_blocked_photo_update(pool: PgPool) {
    sigterm_during_ingest(pool, true).await;
}

async fn sigterm_during_ingest(pool: PgPool, block_update: bool) {
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
    if block_update {
        sqlx::query!("SELECT id FROM photos WHERE id=$1 FOR UPDATE", photo.id.0)
            .fetch_one(&mut *lock)
            .await
            .unwrap();
    }
    // A trusted fake host holds stdin open for the entire shutdown grace period.
    let host = root.path().join("slow-host");
    std::fs::write(&host, b"#!/bin/sh\nexec /bin/sleep 120\n").unwrap();
    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
    let job = Queue::new(pool.clone())
        .enqueue(Job::Ingest { photo_id: photo.id }, 0, None)
        .await
        .unwrap();
    let mut child = spawn_worker(&pool, root.path(), &host);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let running = if block_update {
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}
