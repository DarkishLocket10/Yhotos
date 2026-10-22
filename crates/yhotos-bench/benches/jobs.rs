#![forbid(unsafe_code)]
//! Real Postgres claim latency; needs `DATABASE_URL` pointing at migrated dev Postgres.

use criterion::Criterion;
use secrecy::SecretString;
use std::{
    error::Error,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use yhotos_db::{Db, Role, jobs};

fn main() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _runtime_context = runtime.enter();
    let url: SecretString = std::env::var("DATABASE_URL")?.into();
    let db = runtime.block_on(Db::connect(&url, Role::Admin, 2))?;
    runtime.block_on(db.migrate())?;
    let mut conn = runtime.block_on(db.pool().acquire())?;
    let kind = format!("bench.{}", uuid::Uuid::now_v7());
    let mut measured = Vec::with_capacity(1000);
    // Seed outside the timed region; a unique kind isolates concurrent dev jobs.
    for _ in 0..1000 {
        let job = runtime.block_on(jobs::enqueue(
            &mut conn,
            &kind,
            &serde_json::json!({}),
            0,
            OffsetDateTime::now_utc(),
        ))?;
        let start = Instant::now();
        let claimed = runtime.block_on(jobs::claim(&mut conn, "benchmark", &[&kind]))?;
        measured.push(start.elapsed());
        if claimed.as_ref().map(|j| j.id) != Some(job.id) {
            return Err("benchmark claim did not return its seeded job".into());
        }
        runtime.block_on(jobs::complete(&mut conn, job.id))?;
    }
    measured.sort_unstable();
    let p50 = *measured.get(measured.len() / 2).ok_or("missing samples")?;
    eprintln!("jobs::claim p50={p50:?}, samples={}", measured.len());
    let mut criterion = Criterion::default().configure_from_args();
    let mut failure = None;
    criterion.bench_function("jobs/claim", |b| {
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                let result = runtime.block_on(async {
                    let job = jobs::enqueue(
                        &mut conn,
                        &kind,
                        &serde_json::json!({}),
                        0,
                        OffsetDateTime::now_utc(),
                    )
                    .await?;
                    let start = Instant::now();
                    let claimed = jobs::claim(&mut conn, "benchmark", &[&kind]).await?;
                    let elapsed = start.elapsed();
                    if claimed.as_ref().map(|j| j.id) != Some(job.id) {
                        return Err(yhotos_db::DbError::InvalidInput);
                    }
                    jobs::complete(&mut conn, job.id).await?;
                    Ok(elapsed)
                });
                match result {
                    Ok(elapsed) => total += elapsed,
                    Err(err) => {
                        failure = Some(err);
                        break;
                    }
                }
            }
            total
        });
    });
    criterion.final_summary();
    if let Some(err) = failure {
        return Err(err.into());
    }
    drop(conn);
    runtime.block_on(db.pool().close());
    if p50 >= Duration::from_millis(1) {
        return Err("claim p50 exceeded 1 ms".into());
    }
    Ok(())
}
