#![forbid(unsafe_code)]
//! Full-router first-byte latency and streaming throughput. Requires migrated
//! dev Postgres via `DATABASE_URL`. Run with `YHOTOS_BENCH_DIR` on tmpfs (e.g. /tmp
//! inside a container with --tmpfs /tmp). Setup and cleanup are outside timing.

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use criterion::{Criterion, Throughput};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::{
    error::Error,
    net::SocketAddr,
    time::{Duration, Instant},
};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_config::{Config, LogFormat, RateLimits, Upload};
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{DatabaseTls, Db, Role, albums, photos};
use yhotos_http::{AppState, public_router};
use yhotos_storage::{DerivativeKey, Store};

type BenchResult<T> = Result<T, Box<dyn Error>>;
const FILE_BYTES: usize = 4 * 1024 * 1024;

async fn response(app: &Router, path: &str) -> BenchResult<axum::response::Response> {
    let mut request = Request::builder().uri(path).body(Body::empty())?;
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?));
    let response = app.clone().oneshot(request).await?;
    if response.status() != StatusCode::OK {
        return Err("file benchmark did not return 200".into());
    }
    Ok(response)
}
async fn first_byte(app: &Router, path: &str) -> BenchResult<()> {
    let mut body = response(app, path).await?.into_body();
    let frame = body.frame().await.ok_or("empty benchmark body")??;
    if frame.data_ref().is_none_or(axum::body::Bytes::is_empty) {
        return Err("missing first byte".into());
    }
    Ok(())
}
async fn stream(app: &Router, path: &str) -> BenchResult<()> {
    let mut body = response(app, path).await?.into_body();
    let mut bytes = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            bytes += data.len();
        }
    }
    if bytes != FILE_BYTES {
        return Err("incomplete benchmark body".into());
    }
    Ok(())
}
fn config(dir: &std::path::Path) -> BenchResult<&'static Config> {
    let peer = "127.0.0.1:1".parse()?;
    Ok(Box::leak(Box::new(Config {
        worker: yhotos_config::Worker::default(),
        public_listen: peer,
        admin_listen: Some(peer),
        database_url: std::env::var("DATABASE_URL")?.into(),
        data_dir: dir.to_owned(),
        public_base_url: "https://example.test".parse()?,
        session_key: format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple()).into(),
        trust_proxy: false,
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

async fn setup(pool: &PgPool, store: &Store) -> BenchResult<(yhotos_core::AlbumId, String)> {
    let mut conn = pool.acquire().await?;
    let slug = Slug::new(&format!("bench-{}", Uuid::now_v7().simple()))?;
    let album =
        albums::create(&mut conn, None, &slug, "File benchmark", Visibility::Public).await?;
    let data = vec![73_u8; FILE_BYTES];
    let put = store
        .put_original(data.as_slice(), u64::try_from(data.len())?)
        .await?;
    let hash = put.hash.to_string();
    let photo = photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Benchmark".into(),
            original_hash: hash.clone(),
            original_name: "benchmark.jpeg".into(),
            original_bytes: i64::try_from(FILE_BYTES)?,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await?;
    let bytes = i64::try_from(FILE_BYTES)?;
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,640,480,$4)",Uuid::now_v7(),photo.id.0,hash,bytes).execute(&mut *conn).await?;
    sqlx::query!("INSERT INTO video_renditions(id,photo_id,codec,hash,width,height,bytes,duration_ms,hls) VALUES ($1,$2,'h264',$3,640,480,$4,1000,false)",Uuid::now_v7(),photo.id.0,hash,bytes).execute(&mut *conn).await?;
    let temp = store.tmp_dir()?;
    let source = temp.path().join("benchmark");
    tokio::fs::write(&source, &data).await?;
    store
        .put_derivative(
            &DerivativeKey {
                hash: &put.hash,
                variant: "small",
                format: "jpeg",
                hdr: false,
            },
            &source,
        )
        .await?;
    let video = store.path_video(&put.hash, "h264")?;
    tokio::fs::create_dir_all(video.parent().ok_or("missing video parent")?).await?;
    tokio::fs::write(video, data).await?;
    Ok((album.id, hash))
}

fn percentiles(
    runtime: &tokio::runtime::Runtime,
    app: &Router,
    state: &AppState,
    path: &str,
) -> BenchResult<()> {
    for cold in [false, true] {
        let mut samples = Vec::with_capacity(2000);
        for _ in 0..2000 {
            if cold {
                state.files.invalidate_all();
            }
            let start = Instant::now();
            runtime.block_on(first_byte(app, path))?;
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        let p50 = samples.get(1000).ok_or("missing p50")?;
        let p99 = samples.get(1980).ok_or("missing p99")?;
        eprintln!(
            "files/{} p50={p50:?} p99={p99:?} (2000 requests, first body chunk)",
            if cold { "cold_postgres" } else { "warm_moka" }
        );
        if *p99 >= Duration::from_millis(5) {
            return Err("derivative p99 exceeded 5 ms".into());
        }
    }
    Ok(())
}

fn main() -> BenchResult<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let root =
        std::env::var_os("YHOTOS_BENCH_DIR").ok_or("set YHOTOS_BENCH_DIR to a tmpfs directory")?;
    let dir = tempfile::tempdir_in(root)?;
    let config = config(dir.path())?;
    let store = Store::open(dir.path())?;
    let admin = runtime.block_on(Db::connect(
        &config.database_url,
        Role::Admin,
        2,
        DatabaseTls::Disable,
    ))?;
    let public = runtime.block_on(Db::connect(
        &config.database_url,
        Role::Public,
        2,
        DatabaseTls::Disable,
    ))?;
    let (album, hash) = runtime.block_on(setup(admin.pool(), &store))?;
    let path = format!("/i/{hash}/small.jpeg");
    let original_path = format!("/o/{hash}/benchmark.jpeg");
    let video_path = format!("/v/{hash}/h264.mp4");
    let state = AppState::new(config, public.pool().clone(), store);
    let app = public_router(state.clone());
    runtime.block_on(first_byte(&app, &path))?;
    percentiles(&runtime, &app, &state, &path)?;
    let mut criterion = Criterion::default().configure_from_args();
    let mut failure = None;
    for cold in [false, true] {
        criterion.bench_function(
            if cold {
                "files/cold_postgres/first_byte"
            } else {
                "files/warm_moka/first_byte"
            },
            |b| {
                b.iter_custom(|iterations| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iterations {
                        if cold {
                            state.files.invalidate_all();
                        }
                        let start = Instant::now();
                        if let Err(error) = runtime.block_on(first_byte(&app, &path)) {
                            failure = Some(error);
                            break;
                        }
                        total += start.elapsed();
                    }
                    total
                });
            },
        );
    }
    for (name, path) in [
        ("files/original/first_byte", &original_path),
        ("files/video/first_byte", &video_path),
    ] {
        runtime.block_on(first_byte(&app, path))?;
        criterion.bench_function(name, |b| {
            b.iter(|| {
                if let Err(error) = runtime.block_on(first_byte(&app, path)) {
                    failure = Some(error);
                }
            });
        });
    }
    let mut group = criterion.benchmark_group("files/stream");
    group.throughput(Throughput::Bytes(u64::try_from(FILE_BYTES)?));
    for (name, path) in [
        ("warm_4mib", &path),
        ("original_4mib", &original_path),
        ("video_4mib", &video_path),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                if let Err(error) = runtime.block_on(stream(&app, path)) {
                    failure = Some(error);
                }
            });
        });
    }
    group.finish();
    criterion.final_summary();
    // The seeded rows are isolated by a fresh album and removed after timing.
    runtime.block_on(async {
        sqlx::query!("DELETE FROM photos WHERE album_id=$1", album.0)
            .execute(admin.pool())
            .await?;
        sqlx::query!("DELETE FROM albums WHERE id=$1", album.0)
            .execute(admin.pool())
            .await?;
        Ok::<_, sqlx::Error>(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
