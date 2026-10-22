#![allow(clippy::unwrap_used)]
use super::*;
use crate::{JobRow, QueueConfig};
use sqlx::PgPool;
use yhotos_core::{PhotoId, Slug, access::Visibility};
use yhotos_db::{albums, photos};

async fn photo(pool: &PgPool) -> PhotoId {
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("recovery").unwrap(),
        "Recovery",
        Visibility::Public,
    )
    .await
    .unwrap();
    photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Recovery".into(),
            original_hash: "a".repeat(64),
            original_name: "original".into(),
            original_bytes: 1,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await
    .unwrap()
    .id
}
async fn status(pool: &PgPool, id: PhotoId, expected: &str, reason: Option<&str>) {
    let row = sqlx::query!(
        "SELECT status::text AS \"status!\",failure_reason FROM photos WHERE id=$1",
        id.0
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(row.status, expected);
    assert_eq!(row.failure_reason.as_deref(), reason);
}
struct Hang;
#[async_trait::async_trait]
impl JobHandler for Hang {
    async fn handle(&self, _: &JobRow, _: &JobCtx) -> Result<(), JobError> {
        std::future::pending().await
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn outer_timeout_cleans_photo_for_retry_and_final_failure_and_preserves_ready(pool: PgPool) {
    let photo_id = photo(&pool).await;
    let queue = Queue::new(pool.clone());
    for (initial, max_attempts, expected, reason) in [
        ("processing", 3, "pending", None),
        ("processing", 1, "failed", Some("job timed out")),
        ("ready", 1, "ready", None),
    ] {
        sqlx::query!("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE photos SET status=$2::text::photo_status,failure_reason=NULL WHERE id=$1",
            photo_id.0,
            initial
        )
        .execute(&pool)
        .await
        .unwrap();
        let id = queue
            .enqueue(Job::Ingest { photo_id }, 0, None)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET max_attempts=$2 WHERE id=$1",
            id.0,
            max_attempts
        )
        .execute(&pool)
        .await
        .unwrap();
        let (row, token) = jobs::claim(&mut pool.acquire().await.unwrap(), "timeout", &["ingest"])
            .await
            .unwrap()
            .unwrap();
        queue
            .run_claim(
                &row,
                &token,
                &Hang,
                &JobCtx {
                    queue: queue.clone(),
                    shutdown: CancellationToken::new(),
                },
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        status(&pool, photo_id, expected, reason).await;
        let job = sqlx::query!(
            "SELECT finished_at,error,locked_by FROM jobs WHERE id=$1",
            id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(job.finished_at.is_some(), max_attempts == 1);
        assert_eq!(job.error.as_deref(), Some("job timed out"));
        assert!(job.locked_by.is_none());
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn final_stale_ingest_fails_photo_but_keeps_successful_publication(pool: PgPool) {
    let photo_id = photo(&pool).await;
    let queue = Queue::new(pool.clone());
    for initial in ["processing", "ready"] {
        sqlx::query!("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE photos SET status=$2::text::photo_status,failure_reason=NULL WHERE id=$1",
            photo_id.0,
            initial
        )
        .execute(&pool)
        .await
        .unwrap();
        let id = queue
            .enqueue(Job::Ingest { photo_id }, 0, None)
            .await
            .unwrap();
        sqlx::query!("UPDATE jobs SET attempts=max_attempts,locked_by='dead worker',locked_at=now()-interval '12 minutes' WHERE id=$1",id.0).execute(&pool).await.unwrap();
        queue.sweep_stale(&[JobKind::Ingest]).await.unwrap();
        let ready = initial == "ready";
        status(
            &pool,
            photo_id,
            if ready { "ready" } else { "failed" },
            if ready {
                None
            } else {
                Some("worker lock expired")
            },
        )
        .await;
        let row = sqlx::query!(
            "SELECT finished_at,locked_by,error FROM jobs WHERE id=$1",
            id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.finished_at.is_some());
        assert!(row.locked_by.is_none());
        assert_eq!(row.error.as_deref(), Some("worker lock expired"));
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn failure_is_sanitized_and_old_claim_cannot_change_photo(pool: PgPool) {
    let photo_id = photo(&pool).await;
    let queue = Queue::new(pool.clone());
    queue
        .enqueue(Job::Ingest { photo_id }, 0, None)
        .await
        .unwrap();
    let (_, token) = jobs::claim(&mut pool.acquire().await.unwrap(), "one", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    queue.release(std::iter::once(&token)).await.unwrap();
    let (_, new) = jobs::claim(&mut pool.acquire().await.unwrap(), "two", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    sqlx::query!(
        "UPDATE photos SET status='processing' WHERE id=$1",
        photo_id.0
    )
    .execute(&pool)
    .await
    .unwrap();
    queue
        .finish(&token, Err(JobError::Fatal("old worker".into())))
        .await
        .unwrap();
    status(&pool, photo_id, "processing", None).await;
    queue
        .finish(
            &new,
            Err(JobError::Fatal(
                "failed /data/private.jpg\nsecret details".into(),
            )),
        )
        .await
        .unwrap();
    status(&pool, photo_id, "failed", Some("job failed")).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn shutdown_interrupts_blocked_claim_without_leaking_a_claim(pool: PgPool) {
    let queue = Queue::with_config(
        pool.clone(),
        QueueConfig {
            concurrency: 1,
            poll_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_millis(200),
        },
    )
    .unwrap();
    let id = queue
        .enqueue(
            Job::Ingest {
                photo_id: PhotoId::new(),
            },
            0,
            None,
        )
        .await
        .unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query!("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let worker = tokio::spawn(async move {
        queue
            .run_worker("blocked database", &[JobKind::Ingest], Hang, cancel)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row = sqlx::query!("SELECT count(*) AS \"count!\" FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'UPDATE jobs SET locked_by%'").fetch_one(&pool).await.unwrap();
            if row.count > 0 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    let start = Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_millis(500), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    lock.rollback().await.unwrap();
    let job = sqlx::query!("SELECT attempts,locked_by FROM jobs WHERE id=$1", id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(job.attempts, 0);
    assert!(job.locked_by.is_none());
}

#[test]
fn diagnostics_are_bounded_and_path_free() {
    assert_eq!(sanitize_reason("/private/photo.jpg\nsecret"), "job failed");
    assert_eq!(sanitize_reason("C:\\private\\photo.jpg"), "job failed");
    assert_eq!(sanitize_reason("timeout\n/private/photo"), "timeout");
    assert_eq!(sanitize_reason(&"x".repeat(500)).len(), 200);
}

struct Started(tokio::sync::mpsc::UnboundedSender<()>);
#[async_trait::async_trait]
impl JobHandler for Started {
    async fn handle(&self, _: &JobRow, _: &JobCtx) -> Result<(), JobError> {
        self.0.send(()).unwrap();
        std::future::pending().await
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn shutdown_release_obeys_same_deadline_when_database_is_blocked(pool: PgPool) {
    let queue = Queue::with_config(
        pool.clone(),
        QueueConfig {
            concurrency: 1,
            poll_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_millis(200),
        },
    )
    .unwrap();
    let id = queue
        .enqueue(
            Job::Ingest {
                photo_id: PhotoId::new(),
            },
            0,
            None,
        )
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(async move {
        queue
            .run_worker("blocked release", &[JobKind::Ingest], Started(tx), cancel)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query!("SELECT id FROM jobs WHERE id=$1 FOR UPDATE", id.0)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let start = Instant::now();
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_millis(600), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(JobsError::ShutdownTimeout)));
    assert!(start.elapsed() < Duration::from_millis(600));
    lock.rollback().await.unwrap();
}
