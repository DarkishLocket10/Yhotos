#![forbid(unsafe_code)]
//! Bbox parsing, calendar rendering, and a 10k-photo warm cluster query.
use base64::{Engine, engine::general_purpose::STANDARD};
use criterion::Criterion;
use http_body_util::BodyExt;
use std::{error::Error, hint::black_box, time::Instant};
use tower::ServiceExt;
use yhotos_core::{
    PhotoId, ShortId,
    access::{Principal, ShareTarget},
};
use yhotos_db::{
    calendar::{CalendarPage, Selection},
    map::{self, Bounds},
};
use yhotos_http::pages::{PageContext, render_calendar, render_place};

fn fixture() -> Result<CalendarPage, Box<dyn Error>> {
    let album = uuid::Uuid::now_v7();
    let photos=(0..60).map(|_| {
        let id=PhotoId::new();
        serde_json::json!({"posted":true,"photo":{"id":id,"short_id":ShortId::from_photo_id(id),"album_id":album,"title":"Fixture photograph","description":"Calendar benchmark","original_hash":"a".repeat(64),"original_name":"photo.jpeg","kind":"image","width":1500,"height":1000,"dominant_colors":[],"derivatives":[{"variant":"small","format":"jpeg","hdr":false,"hash":"b".repeat(64),"w":640,"h":427}]}})
    }).collect::<Vec<_>>();
    Ok(serde_json::from_value(
        serde_json::json!({"buckets":[],"photos":photos,"total":60}),
    )?)
}
fn database(c: &mut Criterion, url: &str) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _runtime_context = runtime.enter();
    let pool = runtime.block_on(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(url),
    )?;
    let mut conn = runtime.block_on(pool.acquire())?;
    let place = uuid::Uuid::now_v7();
    let album = yhotos_core::AlbumId::new();
    runtime.block_on(async {
        sqlx::query!("CREATE TEMP TABLE places (LIKE public.places INCLUDING ALL)").execute(&mut *conn).await?;
        sqlx::query!("INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Place benchmark','CA',43.7,-79.4)",place).execute(&mut *conn).await?;
        sqlx::query!("CREATE TEMP TABLE albums (LIKE public.albums INCLUDING ALL)").execute(&mut *conn).await?;
        sqlx::query!("CREATE TEMP TABLE photos (LIKE public.photos INCLUDING ALL)").execute(&mut *conn).await?;
        sqlx::query!("CREATE TEMP TABLE derivatives (LIKE public.derivatives INCLUDING ALL)").execute(&mut *conn).await?;
        sqlx::query!("INSERT INTO albums(id,slug,title) VALUES($1,'map-bench','Map benchmark')",album as _).execute(&mut *conn).await?;
        let ids=(0..10_000).map(|_|PhotoId::new()).collect::<Vec<_>>();
        let shorts=ids.iter().map(|id|ShortId::from_photo_id(*id).to_string()).collect::<Vec<_>>();
        let ids=ids.iter().map(|id|id.0).collect::<Vec<_>>();
        sqlx::query!(r#"INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,gps_lat,gps_lon)
          SELECT id,short,$3,'Map benchmark',$4,'photo.jpeg',1,'image/jpeg','image',-80+mod(n,160),-170+mod(n,340)
          FROM unnest($1::uuid[],$2::text[]) WITH ORDINALITY AS t(id,short,n)"#,&ids,&shorts,album as _,"a".repeat(64)).execute(&mut *conn).await?;
        sqlx::query!("ANALYZE photos").execute(&mut *conn).await?;
        sqlx::query!("ANALYZE albums").execute(&mut *conn).await?;
        Ok::<_,sqlx::Error>(())
    })?;
    let bounds = Bounds::parse("-180,-85,180,85").ok_or("invalid benchmark bounds")?;
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = Instant::now();
        let clusters =
            runtime.block_on(map::clusters(&mut conn, &Principal::Anonymous, bounds, 8))?;
        if clusters.iter().map(|c| c.count).sum::<u64>() != 10_000 {
            return Err("benchmark visibility count mismatch".into());
        }
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "map/clusters/10k p50={:?} p99={:?} (1000 samples)",
        samples.get(500),
        samples.get(990)
    );
    c.bench_function("map/clusters/10k", |b| {
        b.iter(|| {
            black_box(runtime.block_on(map::clusters(&mut conn, &Principal::Anonymous, bounds, 8)))
        });
    });
    account_share_clusters(c, &runtime, &mut conn, album, bounds)?;
    drop(conn);
    pool_latency(&runtime, &pool)?;
    let dir = tempfile::tempdir()?;
    let config = benchmark_config(url, dir.path())?;
    let app = yhotos_http::public_router(yhotos_http::AppState::new(
        config,
        pool.clone(),
        yhotos_storage::Store::open(dir.path())?,
        None,
    ));
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = Instant::now();
        runtime.block_on(map_request(&app))?;
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "map/tiles_endpoint/10k p50={:?} p99={:?} (1000 samples, in-process full response)",
        samples.get(500),
        samples.get(990)
    );
    c.bench_function("map/tiles_endpoint/10k", |b| {
        b.iter(|| black_box(runtime.block_on(map_request(&app))));
    });
    let mut conn = runtime.block_on(pool.acquire())?;
    runtime.block_on(async {
        sqlx::query!("UPDATE photos SET place_id=$1", place)
            .execute(&mut *conn)
            .await?;
        map::point_photos(&mut conn, &Principal::Anonymous, -169.0, -79.0, 1)
            .await?
            .ok_or("missing chooser fixture")?;
        yhotos_db::places::page(&mut conn, &Principal::Anonymous, place, 1)
            .await?
            .ok_or("missing place fixture")?;
        Ok::<_, Box<dyn Error>>(())
    })?;
    location_benches(c, &runtime, &mut conn, bounds, place);
    Ok(())
}
fn account_share_clusters(
    c: &mut Criterion,
    runtime: &tokio::runtime::Runtime,
    conn: &mut sqlx::PgConnection,
    album: yhotos_core::AlbumId,
    bounds: Bounds,
) -> Result<(), Box<dyn Error>> {
    let account = yhotos_core::UserId::new();
    let bindings = yhotos_db::access_sql::visible_photos(&Principal::User {
        id: account,
        groups: &[],
        is_admin: false,
    })
    .with_links([ShareTarget::Album(album)]);
    let clusters = runtime.block_on(map::clusters_for(conn, &bindings, bounds, 8))?;
    if clusters.iter().map(|c| c.count).sum::<u64>() != 10_000 {
        return Err("benchmark account/share union count mismatch".into());
    }
    c.bench_function("map/clusters/account_plus_share/10k", |b| {
        b.iter(|| black_box(runtime.block_on(map::clusters_for(conn, &bindings, bounds, 8))));
    });
    Ok(())
}
fn benchmark_config(
    url: &str,
    data: &std::path::Path,
) -> Result<&'static yhotos_config::Config, Box<dyn Error>> {
    Ok(Box::leak(Box::new(yhotos_config::Config {
        worker: yhotos_config::Worker::default(),
        map_tiles: yhotos_config::MapTiles::default(),
        public_listen: std::net::SocketAddr::from(([127, 0, 0, 1], 18015)),
        admin_listen: None,
        database_url_public: url.to_owned().into(),
        database_url_admin: None,
        database_url_worker: None,
        public_base_url: "https://gallery.example".parse()?,
        admin_base_url: None,
        data_dir: data.to_owned(),
        session_key: STANDARD
            .encode(format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7()))
            .into(),
        trust_proxy: false,
        rate_limits: yhotos_config::RateLimits {
            cheap_per_min: u32::MAX,
            expensive_per_min: u32::MAX,
            auth_per_min: u32::MAX,
        },
        zip: yhotos_config::Zip::default(),
        global_stream_cap: 64,
        write_timeout: std::time::Duration::from_secs(30),
        transfer_deadline: std::time::Duration::from_mins(30),
        upload: yhotos_config::Upload {
            max_bytes: 1,
            chunk_bytes: 1,
        },
        log_format: yhotos_config::LogFormat::Json,
    })))
}
fn location_benches(
    c: &mut Criterion,
    runtime: &tokio::runtime::Runtime,
    conn: &mut sqlx::PgConnection,
    bounds: Bounds,
    place: uuid::Uuid,
) {
    c.bench_function("map/collocated_chooser/10k", |b| {
        b.iter(|| {
            black_box(runtime.block_on(map::point_photos(
                conn,
                &Principal::Anonymous,
                -169.0,
                -79.0,
                1,
            )))
        });
    });
    c.bench_function("map/locations/10k/zoom15", |b| {
        b.iter(|| {
            black_box(runtime.block_on(map::clusters(conn, &Principal::Anonymous, bounds, 15)))
        });
    });
    c.bench_function("places/page/10k", |b| {
        b.iter(|| {
            black_box(runtime.block_on(yhotos_db::places::page(
                conn,
                &Principal::Anonymous,
                place,
                1,
            )))
        });
    });
}
fn pool_latency(
    runtime: &tokio::runtime::Runtime,
    pool: &sqlx::PgPool,
) -> Result<(), Box<dyn Error>> {
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = Instant::now();
        let conn = runtime.block_on(pool.acquire())?;
        drop(conn);
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "map/pool_acquire p50={:?} p99={:?} (1000 samples, includes pool validation)",
        samples.get(500),
        samples.get(990)
    );
    Ok(())
}
async fn map_request(app: &axum::Router) -> Result<(), Box<dyn Error>> {
    let mut request = axum::http::Request::builder()
        .uri("/map/tiles.json?bbox=-180,-85,180,85&zoom=8")
        .body(axum::body::Body::empty())?;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            18016,
        ))));
    let response = app.clone().oneshot(request).await?;
    if response.status() != axum::http::StatusCode::OK {
        return Err("map benchmark request failed".into());
    }
    if response
        .headers()
        .get("cache-tag")
        .and_then(|h| h.to_str().ok())
        != Some("map,visibility")
        || response
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|h| h.to_str().ok())
            != Some("public, max-age=300")
    {
        return Err("map benchmark cache contract failed".into());
    }
    black_box(response.into_body().collect().await?);
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let mut c = Criterion::default().configure_from_args();
    c.bench_function("map/bbox_parse", |b| {
        b.iter(|| black_box(Bounds::parse(black_box("-80,43,-79,44"))));
    });
    let context = PageContext {
        nonce: "benchmark-nonce",
        stylesheet: "/static/app.css",
        script: "/static/thumbhash.js",
        public_base_url: "https://gallery.example",
    };
    let page = fixture()?;
    let selection =
        Selection::new(Some(2026), Some(1), None).ok_or("invalid benchmark selection")?;
    render_calendar(&page, selection, 1, &context)?;
    c.bench_function("calendar/render/60", |b| {
        b.iter(|| black_box(render_calendar(black_box(&page), selection, 1, &context)));
    });
    let place_id = uuid::Uuid::now_v7();
    let place = yhotos_db::places::PlacePage {
        name: "Fixture city".into(),
        admin1: Some("Fixture region".into()),
        country: "CA".into(),
        total: 60,
        photos: page.photos.into_iter().map(|p| p.photo).collect(),
    };
    render_place(&place, place_id, 1, &context)?;
    c.bench_function("places/render/60", |b| {
        b.iter(|| black_box(render_place(black_box(&place), place_id, 1, &context)));
    });
    if let Ok(url) = std::env::var("DATABASE_URL") {
        database(&mut c, &url)?;
    }
    c.final_summary();
    Ok(())
}
