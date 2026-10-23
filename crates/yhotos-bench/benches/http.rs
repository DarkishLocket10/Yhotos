#![forbid(unsafe_code)]
//! Measures the public request stack, including per-IP token-bucket accounting.

use axum::{body::Body, extract::ConnectInfo, http::Request};
use criterion::Criterion;
use std::{hint::black_box, net::SocketAddr, time::Duration};
use tower::ServiceExt;
use yhotos_config::{Config, LogFormat, RateLimits, Upload};
use yhotos_http::{AppState, public_router};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut criterion = Criterion::default().configure_from_args();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let dir = tempfile::tempdir()?;
    let peer: SocketAddr = "127.0.0.1:1".parse()?;
    let config = Box::leak(Box::new(Config {
        ml_address: "127.0.0.1:1".into(),
        worker: yhotos_config::Worker::default(),
        map_tiles: yhotos_config::MapTiles::default(),
        public_listen: peer,
        admin_listen: Some(peer),
        database_url_admin: None,
        database_url_worker: None,
        admin_base_url: None,
        database_url_public: format!("postgres://{}@localhost/test", uuid::Uuid::now_v7()).into(),
        data_dir: dir.path().to_owned(),
        public_base_url: "https://example.test".parse()?,
        // Sixty-four generated hex digits are also valid base64 (48 bytes).
        session_key: format!(
            "{}{}",
            uuid::Uuid::now_v7().simple(),
            uuid::Uuid::now_v7().simple()
        )
        .into(),
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
    }));
    let _runtime_guard = runtime.enter();
    let app = public_router(AppState::new(
        config,
        yhotos_db::Db::connect_lazy(
            &config.database_url_public,
            yhotos_db::Role::Public,
            2,
            yhotos_db::DatabaseTls::Require,
        )?
        .pool()
        .clone(),
        yhotos_storage::Store::open(dir.path())?,
        None,
    ));
    criterion.bench_function("http/healthz/known_ip", |b| {
        b.iter(|| {
            let mut request = Request::new(Body::empty());
            *request.uri_mut() = axum::http::Uri::from_static("/healthz");
            request.extensions_mut().insert(ConnectInfo(peer));
            black_box(runtime.block_on(app.clone().oneshot(request)))
        });
    });
    criterion.final_summary();
    Ok(())
}
