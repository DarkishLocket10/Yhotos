#![forbid(unsafe_code)]
//! Warm album query plus HTML rendering as album size grows, with 20% video.
use base64::{Engine, engine::general_purpose::STANDARD};
use criterion::Criterion;
use http_body_util::BodyExt;
use std::{error::Error, hint::black_box, time::Instant};
use tower::ServiceExt;
use yhotos_core::{PhotoId, ShortId, Slug, access::Principal};
use yhotos_db::{albums, photos::PageSort};
use yhotos_http::pages::{PageContext, render_album};

#[allow(clippy::too_many_lines)] // Keep identical query/render and HTTP samples beside the fixture.
fn main() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _entered = runtime.enter();
    let url = std::env::var("DATABASE_URL")?;
    let pool = runtime.block_on(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url),
    )?;
    let mut conn = runtime.block_on(pool.acquire())?;
    runtime.block_on(async {
        sqlx::query!("CREATE TEMP TABLE albums (LIKE public.albums INCLUDING ALL)")
            .execute(&mut *conn)
            .await?;
        sqlx::query!("CREATE TEMP TABLE photos (LIKE public.photos INCLUDING ALL)")
            .execute(&mut *conn)
            .await?;
        sqlx::query!("CREATE TEMP TABLE derivatives (LIKE public.derivatives INCLUDING ALL)")
            .execute(&mut *conn)
            .await?;
        sqlx::query!(
            "CREATE TEMP TABLE video_renditions (LIKE public.video_renditions INCLUDING ALL)"
        )
        .execute(&mut *conn)
        .await?;
        Ok::<_, sqlx::Error>(())
    })?;
    let album = uuid::Uuid::now_v7();
    runtime.block_on(
        sqlx::query!(
            "INSERT INTO albums(id,slug,title) VALUES($1,'pagination-bench','Album benchmark')",
            album
        )
        .execute(&mut *conn),
    )?;
    let path = [Slug::new("pagination-bench")?];
    let context = PageContext {
        nonce: "benchmark",
        stylesheet: "/static/app.css",
        script: "/static/thumbhash.js",
        public_base_url: "https://gallery.example",
    };
    let mut c = Criterion::default().configure_from_args();
    let dir = tempfile::tempdir()?;
    let app = yhotos_http::public_router(yhotos_http::AppState::new(
        config(&url, dir.path())?,
        pool.clone(),
        yhotos_storage::Store::open(dir.path())?,
        None,
    ));
    for count in [60, 600, 4925] {
        let photos = (0..count).map(|_| PhotoId::new()).collect::<Vec<_>>();
        let ids = photos.iter().map(|id| id.0).collect::<Vec<_>>();
        let shorts = photos
            .iter()
            .map(|id| ShortId::from_photo_id(*id).to_string())
            .collect::<Vec<_>>();
        runtime.block_on(async {
            sqlx::query!("TRUNCATE pg_temp.photos,pg_temp.derivatives,pg_temp.video_renditions").execute(&mut *conn).await?;
            sqlx::query!(r#"INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,width,height,sort_order)
              SELECT id,short,$3,'Album benchmark',$4,'fixture.mp4',1,'video/mp4',
                CASE WHEN n%5=0 THEN 'video'::media_kind ELSE 'image'::media_kind END,1920,1080,n::int
              FROM unnest($1::uuid[],$2::text[]) WITH ORDINALITY t(id,short,n)"#,&ids,&shorts,album,"a".repeat(64)).execute(&mut *conn).await?;
            sqlx::query!(r#"INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes)
              SELECT gen_random_uuid(),p.id,d.variant,'jpeg',$1,d.width,d.width*9/16,1000
              FROM photos p CROSS JOIN (VALUES ('small',640),('medium',1280),('large',2048)) d(variant,width)"#,"b".repeat(64)).execute(&mut *conn).await?;
            sqlx::query!(r#"INSERT INTO video_renditions(id,photo_id,codec,hash,width,height,bytes,duration_ms)
              SELECT gen_random_uuid(),p.id,v.codec,$1,CASE WHEN v.codec='h264' THEN 1080 ELSE 1920 END,CASE WHEN v.codec='h264' THEN 608 ELSE 1080 END,1000,1200
              FROM photos p CROSS JOIN (VALUES ('av1'),('h264')) v(codec) WHERE p.kind='video'"#,"c".repeat(64)).execute(&mut *conn).await?;
            sqlx::query!("ANALYZE photos").execute(&mut *conn).await?;
            sqlx::query!("ANALYZE derivatives").execute(&mut *conn).await?;
            sqlx::query!("ANALYZE video_renditions").execute(&mut *conn).await?;
            Ok::<_, sqlx::Error>(())
        })?;
        let read = |conn: &mut sqlx::PgConnection| -> Result<_, Box<dyn Error>> {
            let page = runtime
                .block_on(albums::page(
                    conn,
                    &Principal::Anonymous,
                    &path,
                    1,
                    PageSort::Manual,
                ))?
                .ok_or("missing album")?;
            if page.total != u64::try_from(count)? || page.photos.len() != 60 {
                return Err("incorrect album pagination".into());
            }
            Ok(render_album(&page, 1, PageSort::Manual, &context)?)
        };
        for _ in 0..20 {
            black_box(read(&mut conn)?);
        }
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            black_box(read(&mut conn)?);
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        eprintln!(
            "album_pagination/{count} p50={:?} p99={:?} (1000 warm query + HTML render samples, 60-photo page)",
            samples.get(500),
            samples.get(990)
        );
        c.bench_function(&format!("album_pagination/{count}"), |b| {
            b.iter(|| black_box(read(&mut conn)));
        });
        drop(conn);
        let request = || runtime.block_on(album_request(&app));
        for _ in 0..20 {
            request()?;
        }
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            request()?;
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        eprintln!(
            "album_http/{count} p50={:?} p99={:?} (1000 warm in-process public-router TTFB samples)",
            samples.get(500),
            samples.get(990)
        );
        c.bench_function(&format!("album_http/{count}"), |b| {
            b.iter(|| black_box(request()));
        });
        conn = runtime.block_on(pool.acquire())?;
    }
    c.final_summary();
    Ok(())
}

async fn album_request(app: &axum::Router) -> Result<(), Box<dyn Error>> {
    let mut request = axum::http::Request::builder()
        .uri("/a/pagination-bench?sort=manual")
        .body(axum::body::Body::empty())?;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            18021,
        ))));
    let response = app.clone().oneshot(request).await?;
    if response.status() != axum::http::StatusCode::OK {
        return Err("album request failed".into());
    }
    let mut body = response.into_body();
    black_box(body.frame().await.ok_or("missing album body")??);
    Ok(())
}

fn config(
    url: &str,
    data: &std::path::Path,
) -> Result<&'static yhotos_config::Config, Box<dyn Error>> {
    Ok(Box::leak(Box::new(yhotos_config::Config {
        worker: yhotos_config::Worker::default(),
        map_tiles: yhotos_config::MapTiles::default(),
        public_listen: std::net::SocketAddr::from(([127, 0, 0, 1], 18021)),
        admin_listen: None,
        database_url_public: url.to_owned().into(),
        database_url_admin: None,
        database_url_worker: None,
        public_base_url: "https://gallery.example".parse()?,
        ml_address: "127.0.0.1:1".into(),
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
