//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use yhotos_core::JobId;

/// Durable queue state with typed identity, UTC scheduling and an atomic claim counter.
#[derive(Clone, Debug)]
pub struct JobRow {
    /// Stored id.
    pub id: JobId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored kind.
    pub kind: String,
    /// Stored payload.
    pub payload: serde_json::Value,
    /// Stored priority.
    pub priority: i32,
    /// Stored run after.
    pub run_after: OffsetDateTime,
    /// Stored attempts.
    pub attempts: i32,
    /// Stored max attempts.
    pub max_attempts: i32,
    /// Stored locked by.
    pub locked_by: Option<String>,
    /// Stored locked at.
    pub locked_at: Option<OffsetDateTime>,
    /// Stored finished at.
    pub finished_at: Option<OffsetDateTime>,
    /// Stored error.
    pub error: Option<String>,
}

/// Queue states derived from completion, lock and error columns.
#[derive(Clone, Copy, Debug)]
pub enum JobStatus {
    /// Unfinished and unclaimed, including scheduled retries.
    Pending,
    /// Claimed by a worker.
    Running,
    /// Completed successfully.
    Complete,
    /// Terminal failure after retry exhaustion.
    Failed,
}
/// Enqueues and notifies listeners atomically; delivery occurs on commit.
/// Payloads and errors must never contain secrets or full request bodies.
///
/// # Errors
/// Returns validation or database failures without partial enqueue.
pub async fn enqueue(
    conn: &mut PgConnection,
    kind: &str,
    payload: &serde_json::Value,
    priority: i32,
    run_after: OffsetDateTime,
) -> Result<JobRow, DbError> {
    use sqlx::Connection;
    let mut tx = conn.begin().await?;
    let job = sqlx::query_as!(JobRow, r#"INSERT INTO jobs(id,kind,payload,priority,run_after) VALUES ($1,$2,$3,$4,$5) RETURNING id as "id: _", created_at, updated_at, kind, payload, priority, run_after, attempts, max_attempts, locked_by, locked_at, finished_at, error"#, JobId::new() as _, kind, payload, priority, run_after).fetch_one(&mut *tx).await?;
    sqlx::query!("SELECT pg_notify('jobs','')")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(job)
}

/// Atomically claims the highest-priority due job of a supported kind with SKIP LOCKED; exhausted
/// jobs are excluded.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn claim(
    conn: &mut PgConnection,
    worker_id: &str,
    kinds: &[&str],
) -> Result<Option<JobRow>, DbError> {
    if worker_id.is_empty() || worker_id.len() > 128 {
        return Err(DbError::InvalidInput);
    }
    Ok(
        sqlx::query_file_as!(JobRow, "queries/claim.sql", worker_id, kinds as &[&str])
            .fetch_optional(conn)
            .await?,
    )
}

/// Finishes a currently locked job successfully and clears its lock and prior error.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn complete(conn: &mut PgConnection, id: JobId) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"UPDATE jobs SET finished_at=now(),locked_at=NULL,locked_by=NULL,error=NULL WHERE id=$1 AND locked_at IS NOT NULL AND finished_at IS NULL"#, id as _).execute(conn).await?.rows_affected())
}

/// Releases a locked job for a delayed retry, or records terminal failure at the attempt limit.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn fail(
    conn: &mut PgConnection,
    id: JobId,
    error: &str,
    retry_in: std::time::Duration,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"UPDATE jobs SET error=$2,run_after=now()+make_interval(secs=>$3::double precision),finished_at=CASE WHEN attempts>=max_attempts THEN now() ELSE NULL END,locked_at=NULL,locked_by=NULL WHERE id=$1 AND locked_at IS NOT NULL AND finished_at IS NULL"#, id as _, error, retry_in.as_secs_f64()).execute(conn).await?.rows_affected())
}

/// Releases locks older than the UTC cutoff, marking exhausted jobs as terminal failures.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn release_stale(
    conn: &mut PgConnection,
    older_than: OffsetDateTime,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"UPDATE jobs SET locked_at=NULL,locked_by=NULL,finished_at=CASE WHEN attempts>=max_attempts THEN now() ELSE NULL END,error=CASE WHEN attempts>=max_attempts THEN 'worker lock expired after final attempt' ELSE error END WHERE locked_at < $1 AND finished_at IS NULL"#, older_than).execute(conn).await?.rows_affected())
}

/// Returns a zero-based page of up to fifty jobs in the requested derived state.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn list(
    conn: &mut PgConnection,
    status: JobStatus,
    page: u32,
) -> Result<Vec<JobRow>, DbError> {
    let status = match status {
        JobStatus::Pending => "pending",
        JobStatus::Running => "running",
        JobStatus::Complete => "complete",
        JobStatus::Failed => "failed",
    };
    Ok(sqlx::query_as!(JobRow,r#"SELECT id as "id: _", created_at, updated_at, kind, payload, priority, run_after, attempts, max_attempts, locked_by, locked_at, finished_at, error FROM jobs WHERE CASE $1::text WHEN 'pending' THEN finished_at IS NULL AND locked_at IS NULL WHEN 'running' THEN finished_at IS NULL AND locked_at IS NOT NULL WHEN 'complete' THEN finished_at IS NOT NULL AND error IS NULL WHEN 'failed' THEN finished_at IS NOT NULL AND error IS NOT NULL ELSE false END ORDER BY created_at DESC,id LIMIT 50 OFFSET $2"#, status, i64::from(page)*50).fetch_all(conn).await?)
}
