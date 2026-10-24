#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)] // Benchmark assertions and CPU arithmetic.
//! Production public-router ZIP streaming versus the original-file HTTP baseline.
//! Requires migrated dev Postgres via `DATABASE_URL`. The 64 MiB local file is
//! warmed before timing; setup/cleanup are excluded. Bodies are drained frame by
//! frame without collecting them, including the ZIP producer's bounded channel.
//! Server-only CPU is measured over loopback with a separate curl consumer.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use criterion::{Criterion, Throughput};
use http_body_util::BodyExt;
use std::{error::Error, net::SocketAddr, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_config::{Config, LogFormat, RateLimits, Upload};
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{DatabaseTls, Db, Role, albums, photos};
use yhotos_http::{AppState, public_router};
use yhotos_storage::Store;
#[path = "support/zip_budget.rs"]
mod zip_budget;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

type BenchResult<T> = Result<T, Box<dyn Error>>;
const FILE_BYTES: u64 = 64 * 1024 * 1024;
const CPU_STREAMS: u32 = 32;
const BUDGET_BYTES_PER_SEC: u64 = 200_000_000;

async fn drain(app: &Router, path: &str, zip: bool) -> BenchResult<()> {
    let request = Request::builder()
        .uri(path)
        .extension(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?))
        .body(Body::empty())?;
    let response = app.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        if zip { "application/zip" } else { "image/jpeg" }
    );
    if zip {
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    }
    let mut body = response.into_body();
    let mut received = 0_u64;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            received += u64::try_from(data.len())?;
            std::hint::black_box(data);
        }
    }
    // One entry: local header + name + descriptor + ZIP64 central record + name
    // + ZIP64 end records. Check all bytes through EOF, including the directory.
    let name = "001-Benchmark.jpeg";
    let expected = FILE_BYTES
        + if zip {
            30 + name.len() as u64 + 16 + 46 + name.len() as u64 + 28 + 98
        } else {
            0
        };
    assert_eq!(received, expected, "incomplete HTTP response body");
    Ok(())
}

// Hyper polls requests and response bodies on runtime workers. Keep the draining
// consumer there too, rather than on Criterion's external block_on thread.
fn stream(runtime: &tokio::runtime::Runtime, app: &Router, path: &str, zip: bool) {
    let app = app.clone();
    let path = path.to_owned();
    runtime
        .block_on(async {
            tokio::spawn(async move { drain(&app, &path, zip).await.unwrap() }).await
        })
        .unwrap();
}

fn config(dir: &std::path::Path) -> BenchResult<&'static Config> {
    let peer = "127.0.0.1:1".parse()?;
    Ok(Box::leak(Box::new(Config {
        worker: yhotos_config::Worker::default(),
        map_tiles: yhotos_config::MapTiles::default(),
        public_listen: peer,
        admin_listen: None,
        database_url_admin: None,
        database_url_worker: None,
        admin_base_url: None,
        database_url_public: std::env::var("DATABASE_URL")?.into(),
        data_dir: dir.to_owned(),
        public_base_url: "https://example.test".parse()?,
        ml_address: "127.0.0.1:1".into(),
        session_key: format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple()).into(),
        trust_proxy: false,
        zip: yhotos_config::Zip::default(),
        global_stream_cap: 64,
        write_timeout: Duration::from_secs(30),
        transfer_deadline: Duration::from_mins(30),
        rate_limits: RateLimits {
            cheap_per_min: u32::MAX,
            expensive_per_min: u32::MAX,
            auth_per_min: u32::MAX,
        },
        upload: Upload {
            max_bytes: 2 * 1024 * 1024 * 1024,
            chunk_bytes: 8 * 1024 * 1024,
        },
        log_format: LogFormat::Json,
    })))
}

#[cfg(unix)]
fn cpu_measurement(
    runtime: &tokio::runtime::Runtime,
    app: &Router,
    path: &str,
    zip: bool,
) -> BenchResult<()> {
    use nix::sys::{
        resource::{UsageWho, getrusage},
        time::TimeValLike,
    };
    let cpu = || -> BenchResult<i64> {
        let usage = getrusage(UsageWho::RUSAGE_SELF)?;
        Ok(usage.user_time().num_microseconds() + usage.system_time().num_microseconds())
    };
    let before = cpu()?;
    let start = std::time::Instant::now();
    for _ in 0..CPU_STREAMS {
        stream(runtime, app, path, zip);
    }
    let wall = start.elapsed().as_secs_f64();
    let used = (cpu()? - before) as f64 / 1_000_000.0;
    let bytes = FILE_BYTES * u64::from(CPU_STREAMS);
    eprintln!(
        "zip_http_stream/{} unthrottled: {} bytes, wall={wall:.6}s, process CPU={used:.6}s, throughput={:.3} MB/s, CPU={:.3}% of one core, CPU normalized to 200 MB/s={:.3}%",
        if zip { "store_zip64" } else { "plain_original" },
        bytes,
        bytes as f64 / wall / 1_000_000.0,
        used / wall * 100.0,
        used / bytes as f64 * BUDGET_BYTES_PER_SEC as f64 * 100.0,
    );
    Ok(())
}

#[cfg(unix)]
fn server_cpu(address: SocketAddr, path: &str, zip: bool) -> BenchResult<()> {
    use nix::sys::{
        resource::{UsageWho, getrusage},
        time::TimeValLike,
    };
    let cpu = || -> BenchResult<(i64, i64)> {
        let usage = getrusage(UsageWho::RUSAGE_SELF)?;
        Ok((
            usage.user_time().num_microseconds(),
            usage.system_time().num_microseconds(),
        ))
    };
    let before = cpu()?;
    let start = std::time::Instant::now();
    let mut bytes = 0_u64;
    for _ in 0..CPU_STREAMS {
        let output = std::process::Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--noproxy",
                "*",
                "--output",
                "/dev/null",
                "--write-out",
                "%{size_download}",
                "--limit-rate",
                "200000000",
            ])
            .arg(format!("http://{address}{path}"))
            .output()?;
        assert!(
            output.status.success(),
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let received: u64 = std::str::from_utf8(&output.stdout)?.parse()?;
        assert!(if zip {
            received > FILE_BYTES
        } else {
            received == FILE_BYTES
        });
        bytes += FILE_BYTES;
    }
    let wall = start.elapsed().as_secs_f64();
    let after = cpu()?;
    let user_cpu = (after.0 - before.0) as f64 / 1_000_000.0;
    let system_cpu = (after.1 - before.1) as f64 / 1_000_000.0;
    let used = user_cpu + system_cpu;
    eprintln!(
        "zip_http_server/{} (separate curl paced at 200 MB/s): {bytes} bytes, wall={wall:.6}s, server CPU={used:.6}s (user={user_cpu:.6}s, system={system_cpu:.6}s), throughput={:.3} MB/s, CPU={:.3}% of one core, CPU normalized to 200 MB/s={:.3}%",
        if zip { "store_zip64" } else { "plain_original" },
        bytes as f64 / wall / 1_000_000.0,
        used / wall * 100.0,
        used / bytes as f64 * BUDGET_BYTES_PER_SEC as f64 * 100.0,
    );
    if zip
        && !zip_budget::within_budget(
            bytes,
            Duration::from_secs_f64(wall),
            Duration::from_secs_f64(used),
        )
    {
        return Err(std::io::Error::other(
            "ZIP budget exceeded: requires >=200 MB/s and <20% of one server CPU core",
        )
        .into());
    }
    Ok(())
}

#[cfg(unix)]
fn loopback_cpu(
    runtime: &tokio::runtime::Runtime,
    app: &Router,
    original: &str,
    zip: &str,
) -> BenchResult<()> {
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let address = listener.local_addr()?;
    let router = app.clone();
    let server = runtime.spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    let measurement = (|| {
        server_cpu(address, original, false)?;
        server_cpu(address, zip, true)
    })();
    server.abort();
    measurement
}

fn main() -> BenchResult<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let root = std::env::var_os("YHOTOS_BENCH_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let dir = tempfile::tempdir_in(root)?;
    let config = config(dir.path())?;
    let store = Store::open(dir.path())?;
    let admin = runtime.block_on(Db::connect(
        &config.database_url_public,
        Role::Admin,
        2,
        DatabaseTls::Disable,
    ))?;
    let public = runtime.block_on(Db::connect(
        &config.database_url_public,
        Role::Public,
        2,
        DatabaseTls::Disable,
    ))?;
    let (album, hash) = runtime.block_on(async {
        let data = vec![123_u8; usize::try_from(FILE_BYTES)?];
        let put = store.put_original(data.as_slice(), FILE_BYTES).await?;
        let mut conn = admin.pool().acquire().await?;
        let album = albums::create(
            &mut conn,
            None,
            &Slug::new(&format!("bench-zip-{}", Uuid::now_v7().simple()))?,
            "ZIP benchmark",
            Visibility::Public,
        )
        .await?;
        photos::create(
            &mut conn,
            &photos::NewPhoto {
                album_id: album.id,
                title: "Benchmark".into(),
                original_hash: put.hash.to_string(),
                original_name: "benchmark.jpeg".into(),
                original_bytes: i64::try_from(FILE_BYTES)?,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        )
        .await?;
        Ok::<_, Box<dyn Error>>((album.id, put.hash))
    })?;
    let app = public_router(AppState::new(config, public.pool().clone(), store, None));
    let original = format!("/o/{hash}/benchmark.jpeg");
    let zip = format!("/zip?album={album}&size=original");
    let result = (|| -> BenchResult<()> {
        for (path, is_zip) in [(&original, false), (&zip, true)] {
            runtime.block_on(drain(&app, path, is_zip))?;
        }
        let mut criterion = Criterion::default().configure_from_args();
        let mut group = criterion.benchmark_group("zip_http_stream");
        group.sample_size(10);
        group.throughput(Throughput::Bytes(FILE_BYTES));
        for (name, path, is_zip) in [
            ("plain_original", &original, false),
            ("store_zip64", &zip, true),
        ] {
            group.bench_function(name, |b| {
                b.iter(|| stream(&runtime, &app, path, is_zip));
            });
        }
        group.finish();
        criterion.final_summary();
        #[cfg(unix)]
        {
            for (path, is_zip) in [(&original, false), (&zip, true)] {
                cpu_measurement(&runtime, &app, path, is_zip)?;
            }
            loopback_cpu(&runtime, &app, &original, &zip)?;
        }
        Ok(())
    })();
    runtime.block_on(async {
        sqlx::query!("DELETE FROM events WHERE album_id=$1", album.0)
            .execute(admin.pool())
            .await?;
        sqlx::query!("DELETE FROM photos WHERE album_id=$1", album.0)
            .execute(admin.pool())
            .await?;
        sqlx::query!("DELETE FROM albums WHERE id=$1", album.0)
            .execute(admin.pool())
            .await?;
        Ok::<_, sqlx::Error>(())
    })?;
    result
}
