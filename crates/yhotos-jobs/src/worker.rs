use crate::{Job, JobCtx, JobError, JobHandler, JobKind, JobsError, Queue};
use sqlx::postgres::PgListener;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio::{
    task::JoinSet,
    time::{MissedTickBehavior, interval},
};
use tokio_util::sync::CancellationToken;
use yhotos_core::JobId;
use yhotos_db::jobs::{self, ClaimToken};

/// Bounded worker settings. GC and rollup each retain a single slot per worker.
#[derive(Clone, Debug)]
pub struct QueueConfig {
    /// Maximum simultaneous jobs, default half the available CPUs (at least one).
    pub concurrency: usize,
    /// Fallback polling when a notification is missed, default three seconds.
    pub poll_interval: Duration,
    /// Total shutdown budget, including database waits, drain and claim release (at most thirty seconds).
    pub shutdown_grace: Duration,
}
impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            concurrency: std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)),
            poll_interval: Duration::from_secs(3),
            shutdown_grace: Duration::from_secs(29),
        }
    }
}
/// Exponential delay after a claim attempt: 30 seconds times 2^attempts, capped at one hour.
#[must_use]
pub fn retry_backoff(attempts: i32) -> Duration {
    Duration::from_secs(
        30_u64
            .saturating_mul(1_u64 << attempts.clamp(0, 7))
            .min(3600),
    )
}
impl Queue {
    /// Listens before claiming, bounds concurrency, and drains then releases claims on shutdown.
    ///
    /// # Errors
    /// Returns database/listener failures or unexpected task termination, after releasing local claims.
    pub async fn run_worker(
        &self,
        worker_id: &str,
        kinds: &[JobKind],
        handler: impl JobHandler,
        shutdown: CancellationToken,
    ) -> Result<(), JobsError> {
        if worker_id.is_empty() || worker_id.len() > 128 || kinds.is_empty() {
            return Err(JobsError::InvalidJob);
        }
        let handler = Arc::new(handler);
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let mut active = HashMap::<JobId, (ClaimToken, JobKind)>::new();
        let mut poll = interval(self.config.poll_interval);
        let mut sweep = interval(Duration::from_secs(60));
        let mut depth = interval(Duration::from_secs(10));
        for ticker in [&mut poll, &mut sweep, &mut depth] {
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        }
        let work = async {
            let mut listener = PgListener::connect_with(&self.pool).await?;
            listener.listen("jobs").await?;
            loop {
                while tasks.len() < self.config.concurrency && !shutdown.is_cancelled() {
                    let available: Vec<&str> = kinds
                        .iter()
                        .filter(|kind| {
                            !matches!(kind, JobKind::Gc | JobKind::Rollup)
                                || !active.values().any(|(_, k)| k == *kind)
                        })
                        .map(|k| k.as_str())
                        .collect();
                    if available.is_empty() {
                        break;
                    }
                    // A unique owner per claim preserves fencing when cancellation or a
                    // placeholder restores its attempt allowance, even if names are reused.
                    let claim_owner = claim_owner(worker_id);
                    // Cancellation rolls an unacknowledged claim back. Record the
                    // token before COMMIT so even a lost commit reply is releasable.
                    let mut tx = self.pool.begin().await?;
                    let claimed = jobs::claim(&mut tx, &claim_owner, &available).await?;
                    let Some((row, token)) = claimed else {
                        break;
                    };
                    let kind = kinds
                        .iter()
                        .copied()
                        .find(|k| k.as_str() == row.kind)
                        .ok_or(JobsError::InvalidJob)?;
                    active.insert(row.id, (token.clone(), kind));
                    tx.commit().await?;
                    let parsed = Job::from_row(&row);
                    let kind = if let Ok(job) = parsed {
                        job.kind()
                    } else {
                        self.finish(&token, Err(JobError::Fatal("invalid job payload".into())))
                            .await?;
                        active.remove(&row.id);
                        continue;
                    };
                    metrics::counter!("yhotos_jobs_claimed_total", "kind" => kind.as_str())
                        .increment(1);
                    let handler = Arc::clone(&handler);
                    let queue = self.clone();
                    let ctx = JobCtx {
                        queue: self.clone(),
                        shutdown: cancel.clone(),
                    };
                    tasks.spawn(async move {
                        let finished = queue
                            .run_claim(&row, &token, handler.as_ref(), &ctx, kind.timeout())
                            .await;
                        (row.id, finished)
                    });
                }
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    result = tasks.join_next(), if !tasks.is_empty() => {
                        let (id, result) = result.ok_or(JobsError::Task)?.map_err(|_| JobsError::Task)?;
                        result?;
                        active.remove(&id);
                    },
                    notification = listener.recv() => { notification?; },
                    _ = poll.tick() => {},
                    _ = sweep.tick() => { self.sweep_stale(kinds).await?; },
                    _ = depth.tick() => {
                        for kind in kinds {
                            let name = kind.as_str();
                            let row = sqlx::query!("SELECT count(*) AS \"count!\" FROM jobs WHERE kind=$1 AND finished_at IS NULL AND locked_at IS NULL", name).fetch_one(&self.pool).await?;
                            #[allow(clippy::cast_precision_loss)]
                            metrics::gauge!("yhotos_jobs_queue_depth", "kind" => name).set(row.count as f64);
                        }
                    },
                }
            }
            Ok::<(), JobsError>(())
        };
        // This select covers *all* waits, including listener startup, pool
        // acquisition, claim COMMIT, sweep and depth queries inside select arms.
        let outcome = tokio::select! {
            biased;
            () = shutdown.cancelled() => Ok(()),
            result = work => result,
        };
        let cleanup = self.drain_and_release(tasks, active, &cancel).await;
        outcome?;
        cleanup
    }
    async fn drain_and_release(
        &self,
        mut tasks: JoinSet<(JobId, Result<(), JobsError>)>,
        mut active: HashMap<JobId, (ClaimToken, JobKind)>,
        cancel: &CancellationToken,
    ) -> Result<(), JobsError> {
        let deadline = tokio::time::Instant::now() + self.config.shutdown_grace;
        let release_budget = (self.config.shutdown_grace / 2).min(Duration::from_secs(1));
        let drain_deadline = deadline - release_budget;
        // Drain even on listener/database errors. A task's durable result is checked before
        // forgetting its token, so failed finalization can always be released below.
        let drain = async {
            while let Some(result) = tasks.join_next().await {
                let (id, result) = result.map_err(|_| JobsError::Task)?;
                result?;
                active.remove(&id);
            }
            Ok::<(), JobsError>(())
        };
        let drained = tokio::time::timeout_at(drain_deadline, drain).await;
        cancel.cancel();
        tasks.abort_all();
        let release = async {
            while tasks.join_next().await.is_some() {}
            // One fenced batch, rather than a fresh unbounded wait per claim.
            self.release(active.values().map(|(token, _)| token)).await
        };
        tokio::time::timeout_at(deadline, release)
            .await
            .map_err(|_| JobsError::ShutdownTimeout)??;
        if let Ok(result) = drained {
            result?;
        }
        Ok(())
    }
    async fn run_claim(
        &self,
        row: &crate::JobRow,
        token: &ClaimToken,
        handler: &impl JobHandler,
        ctx: &JobCtx,
        timeout: Duration,
    ) -> Result<(), JobsError> {
        let kind = Job::from_row(row)?.kind();
        let start = Instant::now();
        let result = match tokio::time::timeout(timeout, handler.handle(row, ctx)).await {
            Ok(result) => result,
            Err(_) => Err(JobError::Retry {
                after: retry_backoff(row.attempts),
                reason: "job timed out".into(),
            }),
        };
        let success = result.is_ok();
        self.finish(token, result).await?;
        metrics::counter!(if success { "yhotos_jobs_completed_total" } else { "yhotos_jobs_failed_total" }, "kind" => kind.as_str()).increment(1);
        metrics::histogram!("yhotos_jobs_duration_seconds", "kind" => kind.as_str())
            .record(start.elapsed().as_secs_f64());
        Ok(())
    }
    async fn finish(
        &self,
        token: &ClaimToken,
        result: Result<(), JobError>,
    ) -> Result<(), JobsError> {
        let mut conn = self.pool.acquire().await?;
        match result {
            Ok(()) => {
                jobs::complete(&mut conn, token).await?;
            }
            Err(error) => {
                let (after, reason, fatal) = match error {
                    JobError::Retry { after, reason } => {
                        (after.max(retry_backoff(token.attempt)), reason, false)
                    }
                    JobError::Fatal(reason) => (Duration::ZERO, reason, true),
                };
                let reason = sanitize_reason(&reason);
                // Placeholder jobs must survive indefinitely until their handlers land.
                if !fatal && reason == "not implemented" {
                    sqlx::query!("UPDATE jobs SET error=$4,run_after=now()+make_interval(secs=>$5::double precision),attempts=attempts-1,locked_at=NULL,locked_by=NULL WHERE id=$1 AND locked_by=$2 AND attempts=$3 AND finished_at IS NULL", token.job_id.0, token.worker_id, token.attempt, reason, after.max(Duration::from_secs(3600)).as_secs_f64()).execute(&mut *conn).await?;
                } else {
                    // The CTE is one atomic statement. Only a matching claim can
                    // change its photo; successful publication always wins.
                    sqlx::query!(r#"
                        WITH changed AS (
                            UPDATE jobs SET error=$4,
                                run_after=now()+make_interval(secs=>$5::double precision),
                                finished_at=CASE WHEN $6 OR attempts>=max_attempts THEN now() ELSE NULL END,
                                locked_at=NULL,locked_by=NULL
                            WHERE id=$1 AND locked_by=$2 AND attempts=$3 AND finished_at IS NULL
                            RETURNING kind,payload,finished_at
                        )
                        UPDATE photos SET status=CASE WHEN changed.finished_at IS NULL THEN 'pending'::photo_status ELSE 'failed'::photo_status END,
                            failure_reason=CASE WHEN changed.finished_at IS NULL THEN NULL ELSE $4 END
                        FROM changed WHERE changed.kind='ingest'
                            AND photos.id=CASE WHEN pg_input_is_valid(changed.payload->>'photo_id','uuid') THEN (changed.payload->>'photo_id')::uuid END
                            AND photos.status<>'ready' AND photos.deleted_at IS NULL
                    "#, token.job_id.0,token.worker_id,token.attempt,reason,after.as_secs_f64(),fatal).execute(&mut *conn).await?;
                }
            }
        }
        Ok(())
    }
    async fn release<'a>(
        &self,
        tokens: impl Iterator<Item = &'a ClaimToken>,
    ) -> Result<(), JobsError> {
        let mut ids = Vec::new();
        let mut owners = Vec::new();
        let mut attempts = Vec::new();
        for token in tokens {
            ids.push(token.job_id.0);
            owners.push(token.worker_id.clone());
            attempts.push(token.attempt);
        }
        if !ids.is_empty() {
            sqlx::query!("UPDATE jobs SET locked_at=NULL,locked_by=NULL,attempts=jobs.attempts-1 FROM UNNEST($1::uuid[],$2::text[],$3::int[]) AS claim(id,owner,attempt) WHERE jobs.id=claim.id AND jobs.locked_by=claim.owner AND jobs.attempts=claim.attempt AND jobs.finished_at IS NULL",&ids,&owners,&attempts).execute(&self.pool).await?;
        }
        Ok(())
    }
    /// Releases expired claims using each kind's timeout plus one minute; exhausted jobs become terminal.
    ///
    /// # Errors
    /// Returns database failures.
    pub async fn sweep_stale(&self, kinds: &[JobKind]) -> Result<(), JobsError> {
        for kind in kinds {
            let cutoff = OffsetDateTime::now_utc() - kind.timeout() - Duration::from_secs(60);
            let name = kind.as_str();
            sqlx::query!(r#"
                WITH changed AS (
                    UPDATE jobs SET locked_at=NULL,locked_by=NULL,
                        run_after=now()+make_interval(secs=>least(3600,30*power(2,least(attempts,7)))),
                        finished_at=CASE WHEN attempts>=max_attempts THEN now() ELSE NULL END,
                        error='worker lock expired'
                    WHERE kind=$1 AND locked_at<$2 AND finished_at IS NULL
                    RETURNING kind,payload,finished_at
                )
                UPDATE photos SET status=CASE WHEN changed.finished_at IS NULL THEN 'pending'::photo_status ELSE 'failed'::photo_status END,
                    failure_reason=CASE WHEN changed.finished_at IS NULL THEN NULL ELSE 'worker lock expired' END
                FROM changed WHERE changed.kind='ingest'
                    AND photos.id=CASE WHEN pg_input_is_valid(changed.payload->>'photo_id','uuid') THEN (changed.payload->>'photo_id')::uuid END
                    AND photos.status<>'ready' AND photos.deleted_at IS NULL
            "#,name,cutoff).execute(&self.pool).await?;
        }
        Ok(())
    }
}

fn claim_owner(worker_id: &str) -> String {
    let mut owner = worker_id.to_owned();
    while owner.len() > 90 {
        owner.pop();
    }
    owner.push(':');
    owner.push_str(&uuid::Uuid::now_v7().to_string());
    owner
}

// Handler diagnostics are untrusted too: preserve only plain first-line text.
fn sanitize_reason(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or("");
    if line.is_empty() || line.chars().any(|c| !c.is_alphabetic() && c != ' ') {
        return "job failed".into();
    }
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests;
