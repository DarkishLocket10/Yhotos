#![forbid(unsafe_code)]
//! CPU job consumer with SIGTERM/SIGINT cancellation and fenced claim release.
mod geo;
use anyhow::{Context, Result};
use std::time::Duration;
use yhotos_jobs::{JobKind, Queue, QueueConfig};

fn main() -> std::process::ExitCode {
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        eprintln!("worker runtime startup failed");
        return std::process::ExitCode::FAILURE;
    };
    let mut deadline = None;
    let result = runtime.block_on(run(&mut deadline));
    // Aborting an async storage call cannot stop its spawn_blocking closure.
    // Runtime drop waits forever for those closures (for example flock during
    // publication). Include runtime shutdown in the signal's original budget.
    let remaining = deadline.map_or(Duration::ZERO, |end: std::time::Instant| {
        end.saturating_duration_since(std::time::Instant::now())
    });
    runtime.shutdown_timeout(remaining);
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run(process_deadline: &mut Option<std::time::Instant>) -> Result<()> {
    let config = yhotos_config::Config::from_env()?;
    let database_url = config
        .database_url_worker
        .as_ref()
        .context("YHOTOS_DATABASE_URL_WORKER required for worker startup")?;
    install_metrics(config.worker.metrics_listen)?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .json()
        .try_init()
        .map_err(|_| anyhow::anyhow!("tracing startup failed"))?;
    let executable = config
        .worker
        .codec_host_path
        .canonicalize()
        .context("codec executable unavailable")?;
    anyhow::ensure!(
        executable.is_file(),
        "codec executable must be a regular file"
    );
    let db = yhotos_db::Db::connect(
        database_url,
        yhotos_db::Role::Worker,
        u32::from(config.worker.concurrency) + 4,
        if config.worker.local_plaintext_database {
            yhotos_db::DatabaseTls::Disable
        } else {
            yhotos_db::DatabaseTls::Require
        },
    )
    .await
    .context("worker database unavailable")?;
    let ingest = build_ingest(&config, &db, executable)?;
    let queue_config = QueueConfig {
        concurrency: usize::from(config.worker.concurrency),
        ..QueueConfig::default()
    };
    let shutdown_grace = queue_config.shutdown_grace;
    let queue = Queue::with_config(db.pool().clone(), queue_config)?;
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("SIGTERM handler unavailable")?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("SIGINT handler unavailable")?;
    let worker = format!("worker-{}", uuid::Uuid::now_v7());
    let signal = async {
        tokio::select! { _=term.recv()=>{}, _=interrupt.recv()=>{} }
    };
    let run = queue.run_worker_until(
        &worker,
        &[
            JobKind::Ingest,
            JobKind::MlEmbed,
            JobKind::MlFaces,
            JobKind::MlOcr,
            JobKind::MlTags,
            JobKind::GeoReverse,
            JobKind::VideoTranscode,
            JobKind::Gc,
        ],
        geo::Handler {
            ingest,
            pool: db.pool().clone(),
        },
        async {
            shutdown
                .await
                .unwrap_or_else(|_| tokio::time::Instant::now())
        },
    );
    tokio::pin!(run);
    let sweep_schedule = schedule_upload_sweep(&queue);
    tokio::pin!(sweep_schedule);
    let (result, deadline) = tokio::select! {
        ()=&mut sweep_schedule => return Err(anyhow::anyhow!("upload sweep scheduler stopped")),
        result=&mut run => (result, tokio::time::Instant::now() + shutdown_grace),
        ()=signal => {
            // One deadline starts at the signal, including queue cleanup and
            // SQLx's return/close waits for connections with cancelled queries.
            let deadline = tokio::time::Instant::now() + shutdown_grace;
            let _ = stop.send(deadline);
            let result = tokio::time::timeout_at(deadline, &mut run)
                .await
                .unwrap_or(Err(yhotos_jobs::JobsError::ShutdownTimeout));
            (result, deadline)
        },
    };
    *process_deadline = Some(deadline.into_std());
    let _ = tokio::time::timeout_at(deadline, db.pool().close()).await;
    result?;
    Ok(())
}

fn build_ingest(
    config: &yhotos_config::Config,
    db: &yhotos_db::Db,
    executable: std::path::PathBuf,
) -> Result<yhotos_pipeline::Ingest> {
    let store =
        yhotos_storage::Store::open(&config.data_dir).context("worker storage unavailable")?;
    let mut ingest = yhotos_pipeline::Ingest::new(store, db.pool().clone(), executable);
    if let Some(plan) = config.worker.variant_plan.clone() {
        ingest.plan = serde_json::from_value(plan)
            .map_err(|_| anyhow::anyhow!("invalid worker variant plan"))?;
    }
    ingest.plan.validate()?;
    ingest.codec_timeout = Duration::from_secs(u64::from(config.worker.codec_timeout));
    ingest.concurrency_per_host = config.worker.vips_concurrency;
    ingest.default_timezone = jiff::tz::TimeZone::get(&config.worker.default_timezone)
        .map_err(|_| anyhow::anyhow!("invalid worker default timezone"))?;
    Ok(ingest)
}

async fn schedule_upload_sweep(queue: &Queue) {
    let mut interval = tokio::time::interval(Duration::from_hours(1));
    loop {
        interval.tick().await;
        if queue.enqueue(yhotos_jobs::Job::Gc, 0, None).await.is_err() {
            tracing::warn!("upload sweep scheduling failed");
        }
    }
}

fn install_metrics(address: std::net::SocketAddr) -> Result<()> {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(address)
        .set_buckets(&[
            0.001, 0.01, 0.1, 1.0, 10.0, 60.0, 300.0, 600.0, 1800.0, 3600.0,
        ])
        .map_err(|_| anyhow::anyhow!("metrics bucket configuration failed"))?
        .install()
        .map_err(|_| anyhow::anyhow!("metrics listener startup failed"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn prometheus_listener_exports_kind_labels_and_histograms() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        super::install_metrics(address).unwrap();
        metrics::counter!("yhotos_jobs_claimed_total","kind"=>"ingest").increment(1);
        metrics::histogram!("yhotos_jobs_duration_seconds","kind"=>"ingest").record(0.5);
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            socket.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.contains("yhotos_jobs_claimed_total{kind=\"ingest\"} 1"));
        assert!(response.contains("yhotos_jobs_duration_seconds_bucket{kind=\"ingest\""));
    }
}
