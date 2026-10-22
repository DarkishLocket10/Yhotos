#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]
//! Real PostgreSQL tests of dedupe, ownership, wakeups, retries and shutdown.
use sqlx::PgPool;
use std::{collections::HashSet, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use yhotos_core::PhotoId;
use yhotos_db::jobs;
use yhotos_jobs::{
    Job, JobCtx, JobError, JobHandler, JobKind, JobRow, Queue, QueueConfig, retry_backoff,
};

fn ingest() -> Job {
    Job::Ingest {
        photo_id: PhotoId::new(),
        original_hash: "a".repeat(64).parse().unwrap(),
    }
}
fn queue(pool: PgPool) -> Queue {
    Queue::with_config(
        pool,
        QueueConfig {
            concurrency: 1,
            poll_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(1),
        },
    )
    .unwrap()
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn concurrent_enqueue_dedupes_until_finished(pool: PgPool) {
    let q = Queue::new(pool.clone());
    let job = ingest();
    let (a, b) = tokio::join!(
        q.enqueue(job.clone(), 0, None),
        q.enqueue(job.clone(), 0, None)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    let mut conn = pool.acquire().await.unwrap();
    let (_, token) = jobs::claim(&mut conn, "a", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    assert!(jobs::complete(&mut conn, &token).await.unwrap());
    assert_ne!(token.job_id, q.enqueue(job, 0, None).await.unwrap());
    for job in [Job::Gc, Job::LegacyRedirectRebuild] {
        let id = q.enqueue(job.clone(), 0, None).await.unwrap();
        let row = jobs::list(&mut conn, jobs::JobStatus::Pending, 0)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(Job::from_row(&row).unwrap(), job);
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn claim_orders_due_jobs_by_priority(pool: PgPool) {
    let q = Queue::new(pool.clone());
    let future = q
        .enqueue(
            ingest(),
            100,
            Some(OffsetDateTime::now_utc() + Duration::from_secs(3600)),
        )
        .await
        .unwrap();
    let low = q.enqueue(ingest(), 1, None).await.unwrap();
    let high = q.enqueue(ingest(), 10, None).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    for expected in [high, low] {
        let (r, t) = jobs::claim(&mut conn, "priority", &["ingest"])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.id, expected);
        jobs::complete(&mut conn, &t).await.unwrap();
    }
    assert!(
        jobs::claim(&mut conn, "priority", &["ingest"])
            .await
            .unwrap()
            .is_none()
    );
    let row = sqlx::query!("SELECT locked_by FROM jobs WHERE id=$1", future.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(row.locked_by.is_none());
}
struct Record {
    tx: mpsc::UnboundedSender<yhotos_core::JobId>,
    seen: Arc<Mutex<HashSet<yhotos_core::JobId>>>,
}
#[async_trait::async_trait]
impl JobHandler for Record {
    async fn handle(&self, row: &JobRow, _: &JobCtx) -> Result<(), JobError> {
        assert!(self.seen.lock().await.insert(row.id));
        self.tx.send(row.id).unwrap();
        Ok(())
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn workers_never_double_claim_and_notify_wakes_under_100ms(pool: PgPool) {
    let q = queue(pool.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let cancel = CancellationToken::new();
    let mut handles = Vec::new();
    for worker in ["one", "two"] {
        let q = q.clone();
        let cancel = cancel.clone();
        let handler = Record {
            tx: tx.clone(),
            seen: seen.clone(),
        };
        handles.push(tokio::spawn(async move {
            q.run_worker(worker, &[JobKind::Ingest], handler, cancel)
                .await
        }));
    }
    // A handled warm-up establishes that the listener is installed. Later work
    // arrives while the workers have no work and their fallback poll is 30s away.
    q.enqueue(ingest(), 0, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let id = q.enqueue(ingest(), 0, None).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap(),
        id
    );
    for _ in 0..10 {
        q.enqueue(ingest(), 0, None).await.unwrap();
    }
    for _ in 0..10 {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    cancel.cancel();
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert_eq!(seen.lock().await.len(), 12);
}
struct Outcome {
    tx: mpsc::UnboundedSender<()>,
    fatal: bool,
}
#[async_trait::async_trait]
impl JobHandler for Outcome {
    async fn handle(&self, _: &JobRow, _: &JobCtx) -> Result<(), JobError> {
        self.tx.send(()).unwrap();
        if self.fatal {
            Err(JobError::Fatal("rejected".into()))
        } else {
            Err(JobError::Retry {
                after: Duration::from_secs(1),
                reason: "retry test".into(),
            })
        }
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn retry_and_fatal_outcomes_are_durable(pool: PgPool) {
    let q = queue(pool.clone());
    for fatal in [false, true] {
        let id = q.enqueue(ingest(), 0, None).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let qc = q.clone();
        let c = cancel.clone();
        let before = OffsetDateTime::now_utc();
        let h = tokio::spawn(async move {
            qc.run_worker("outcome", &[JobKind::Ingest], Outcome { tx, fatal }, c)
                .await
        });
        rx.recv().await.unwrap();
        cancel.cancel();
        h.await.unwrap().unwrap();
        let row = sqlx::query!(
            "SELECT attempts,run_after,finished_at,error,locked_by FROM jobs WHERE id=$1",
            id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.attempts, 1);
        assert_eq!(row.finished_at.is_some(), fatal);
        assert!(row.locked_by.is_none());
        if !fatal {
            assert!(row.run_after >= before + Duration::from_secs(60));
        }
    }
}
struct Block(mpsc::UnboundedSender<()>);
#[async_trait::async_trait]
impl JobHandler for Block {
    async fn handle(&self, _: &JobRow, _: &JobCtx) -> Result<(), JobError> {
        self.0.send(()).unwrap();
        std::future::pending().await
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn shutdown_releases_running_claim_without_consuming_attempt(pool: PgPool) {
    let q = Queue::with_config(
        pool.clone(),
        QueueConfig {
            concurrency: 1,
            poll_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_millis(200),
        },
    )
    .unwrap();
    let id = q.enqueue(ingest(), 0, None).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let h = tokio::spawn(async move {
        q.run_worker("blocked", &[JobKind::Ingest], Block(tx), c)
            .await
    });
    rx.recv().await.unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let row = sqlx::query!(
        "SELECT attempts,run_after,finished_at,error,locked_by FROM jobs WHERE id=$1",
        id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.attempts, 0);
    assert!(row.locked_by.is_none());
    assert!(row.finished_at.is_none());
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn stale_release_is_kind_specific_and_fences_old_attempts(pool: PgPool) {
    let q = Queue::new(pool.clone());
    let id = q.enqueue(ingest(), 0, None).await.unwrap();
    let video = q
        .enqueue(
            Job::Zip {
                request_id: uuid::Uuid::now_v7(),
            },
            0,
            None,
        )
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let (_, token) = jobs::claim(&mut conn, "stale", &["ingest"])
        .await
        .unwrap()
        .unwrap();
    jobs::claim(&mut conn, "zip", &["zip"])
        .await
        .unwrap()
        .unwrap();
    sqlx::query!(
        "UPDATE jobs SET locked_at=now()-interval '12 minutes' WHERE id=ANY($1)",
        &[id.0, video.0]
    )
    .execute(&pool)
    .await
    .unwrap();
    q.sweep_stale(&[JobKind::Ingest, JobKind::Zip])
        .await
        .unwrap();
    assert!(!jobs::complete(&mut conn, &token).await.unwrap());
    assert!(
        sqlx::query!("SELECT locked_by FROM jobs WHERE id=$1", id.0)
            .fetch_one(&pool)
            .await
            .unwrap()
            .locked_by
            .is_none()
    );
    assert!(
        sqlx::query!("SELECT locked_by FROM jobs WHERE id=$1", video.0)
            .fetch_one(&pool)
            .await
            .unwrap()
            .locked_by
            .is_some()
    );
}
#[test]
fn backoff_is_capped_and_attempt_based() {
    for (attempt, seconds) in [
        (0, 30),
        (1, 60),
        (2, 120),
        (3, 240),
        (7, 3600),
        (i32::MAX, 3600),
    ] {
        assert_eq!(retry_backoff(attempt), Duration::from_secs(seconds));
    }
}
