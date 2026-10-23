#![forbid(unsafe_code)]
//! Database-backed, full-router mutation latency for small and large albums.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use criterion::Criterion;
use http_body_util::BodyExt;
use std::{
    error::Error,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_auth::{Token, TokenKind};
use yhotos_config::{Config, LogFormat, RateLimits, Upload};
use yhotos_core::{
    AlbumId, PhotoId, ShortId, Slug,
    access::{ShareTarget, Visibility},
};
use yhotos_db::{
    DatabaseTls, Db, Role, albums,
    share_links::{self, LinkFlags, NewShareLink},
    users,
};
use yhotos_http::{AppState, auth::Auth, public_router};
use yhotos_storage::Store;
static NEXT_IP: AtomicU32 = AtomicU32::new(0);

type BenchResult<T> = Result<T, Box<dyn Error>>;

async fn mutate(
    app: &Router,
    short: &ShortId,
    cookie: &str,
    method: &str,
    action: &str,
) -> BenchResult<()> {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/v1/photos/{short}/{action}"))
        .header(header::COOKIE, cookie)
        .header("x-requested-with", "yhotos")
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())?;
    // Keep the production 60/minute bucket active without benchmarking 429s.
    // A bounded, round-robin set also avoids growing the rate-limit cache.
    let [_, _, high, low] = (NEXT_IP.fetch_add(1, Ordering::Relaxed) % 4096).to_be_bytes();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((
            Ipv4Addr::new(127, 1, high, low),
            1,
        ))));
    let response = app.clone().oneshot(request).await?;
    if response.status() != StatusCode::OK {
        return Err(format!(
            "mutation benchmark {method}/{action} failed with {}",
            response.status()
        )
        .into());
    }
    let body = response.into_body().collect().await?.to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    if value.get("liked").is_none() || value.get("flagged").is_none() {
        return Err("invalid mutation response".into());
    }
    if (action == "like"
        && value.get("liked").and_then(serde_json::Value::as_bool) != Some(method == "POST"))
        || (action == "flag"
            && value.get("flagged").and_then(serde_json::Value::as_bool) != Some(true))
    {
        return Err("mutation did not reach its requested state".into());
    }
    Ok(())
}
fn config(dir: &std::path::Path, url: secrecy::SecretString) -> BenchResult<&'static Config> {
    Ok(Box::leak(Box::new(Config {
        ml_address: "127.0.0.1:1".into(),
        public_listen: "127.0.0.1:1".parse()?,
        admin_listen: None,
        database_url_public: url,
        database_url_admin: None,
        database_url_worker: None,
        worker: yhotos_config::Worker::default(),
        map_tiles: yhotos_config::MapTiles::default(),
        admin_base_url: None,
        data_dir: dir.to_owned(),
        public_base_url: "https://example.test".parse()?,
        session_key: STANDARD
            .encode(Token::generate().hash(TokenKind::Session).as_bytes())
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
            max_bytes: 1,
            chunk_bytes: 1,
        },
        log_format: LogFormat::Json,
    })))
}
async fn seed(
    db: &Db,
    count: usize,
) -> BenchResult<(yhotos_core::UserId, AlbumId, ShortId, String)> {
    let mut conn = db.pool().acquire().await?;
    let password = yhotos_auth::generate_temp_password();
    let phc = yhotos_auth::hash_password(&password)?;
    let user = users::create(
        &mut conn,
        &Uuid::now_v7().to_string(),
        &phc.as_str().into(),
        false,
    )
    .await?;
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new(&format!("mutation-bench-{}", Uuid::now_v7().simple()))?,
        "Mutation benchmark",
        Visibility::Restricted,
    )
    .await?;
    let ids: Vec<_> = (0..count).map(|_| PhotoId::new()).collect();
    let shorts: Vec<_> = ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect();
    let uuids: Vec<_> = ids.iter().map(|id| id.0).collect();
    // One bound bulk insert avoids measuring fixture generation.
    sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,width,height) SELECT id,short,$3,short,repeat('a',64),'bench.jpeg',1,'image/jpeg','image',6000,4000 FROM unnest($1::uuid[],$2::text[]) AS p(id,short)", &uuids, &shorts, album.id as _).execute(&mut *conn).await?;
    let short = shorts.first().ok_or("empty fixture")?.parse()?;
    let token = Token::generate();
    let link = share_links::create(
        &mut conn,
        &NewShareLink {
            token_hash: *token.hash(TokenKind::ShareLink).as_bytes(),
            target: ShareTarget::Album(album.id),
            passphrase_hash: None,
            expires_at: None,
            flags: LinkFlags {
                view: true,
                download: false,
                like: true,
                flag: true,
            },
            created_by: user.id,
        },
    )
    .await?;
    let session = Token::generate();
    let hash = *session.hash(TokenKind::Session).as_bytes();
    share_links::create_session(&mut conn, &link, &hash, &hash, "other")
        .await?
        .ok_or("benchmark session failed")?;
    Ok((
        user.id,
        album.id,
        short,
        format!("__Host-yhotos-share={}", session.encode(TokenKind::Session)),
    ))
}
async fn cleanup(db: &Db, user: yhotos_core::UserId, album: AlbumId) -> BenchResult<()> {
    sqlx::query!("DELETE FROM share_links WHERE created_by=$1", user as _)
        .execute(db.pool())
        .await?;
    sqlx::query!("DELETE FROM photos WHERE album_id=$1", album as _)
        .execute(db.pool())
        .await?;
    sqlx::query!("DELETE FROM albums WHERE id=$1", album as _)
        .execute(db.pool())
        .await?;
    sqlx::query!("DELETE FROM users WHERE id=$1", user as _)
        .execute(db.pool())
        .await?;
    Ok(())
}
#[allow(
    clippy::too_many_lines,
    reason = "keep fixture lifecycle and timed measurement together"
)]
fn main() -> BenchResult<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let dir = tempfile::tempdir()?;
    let url = std::env::var("DATABASE_URL")?.into();
    let config = config(dir.path(), url)?;
    let db = runtime.block_on(Db::connect(
        &config.database_url_public,
        Role::Admin,
        4,
        DatabaseTls::Disable,
    ))?;
    let public = Arc::new(runtime.block_on(Db::connect(
        &config.database_url_public,
        Role::Public,
        4,
        DatabaseTls::Disable,
    ))?);
    let auth = runtime.block_on(Auth::new(config, public.clone(), None))?;
    let app = public_router(AppState::new(
        config,
        public.pool().clone(),
        Store::open(dir.path())?,
        Some(auth),
    ));
    let mut criterion = Criterion::default().configure_from_args();
    for count in [60, 10_000] {
        let (user, album, short, cookie) = runtime.block_on(seed(&db, count))?;
        let result = (|| -> BenchResult<()> {
            for (method, action) in [("POST", "like"), ("DELETE", "like"), ("POST", "flag")] {
                for retry in [false, true] {
                    let name = if retry {
                        format!("interactions/{method}/{action}/{count}")
                    } else {
                        format!("interactions/write/{method}/{action}/{count}")
                    };
                    // Preparation uses the real inverse operation outside timing. Flag
                    // deletion is fixture maintenance through a bound SQLx statement.
                    let measure = || -> BenchResult<Duration> {
                        if action == "flag" {
                            let mut conn = runtime.block_on(db.pool().acquire())?;
                            let page = runtime
                                .block_on(yhotos_db::photos::get_by_short_id(&mut conn, &short))?
                                .ok_or("missing photo")?;
                            runtime.block_on(
                                sqlx::query!("DELETE FROM flags WHERE photo_id=$1", page.id as _)
                                    .execute(&mut *conn),
                            )?;
                            if retry {
                                runtime.block_on(mutate(&app, &short, &cookie, method, action))?;
                            }
                        } else {
                            let prepare = if (method == "POST") == retry {
                                "POST"
                            } else {
                                "DELETE"
                            };
                            runtime.block_on(mutate(&app, &short, &cookie, prepare, action))?;
                        }
                        let start = Instant::now();
                        runtime.block_on(mutate(&app, &short, &cookie, method, action))?;
                        Ok(start.elapsed())
                    };
                    for _ in 0..20 {
                        measure()?;
                    }
                    let mut samples = (0..1000)
                        .map(|_| measure())
                        .collect::<BenchResult<Vec<_>>>()?;
                    samples.sort_unstable();
                    let p99 = *samples.get(990).ok_or("missing samples")?;
                    eprintln!(
                        "{name} p50={:?} p99={p99:?} (1000 samples)",
                        samples.get(500)
                    );
                    if !retry
                        && std::env::var_os("YHOTOS_ENFORCE_TOWER_BUDGETS").is_some()
                        && p99 >= Duration::from_millis(10)
                    {
                        return Err(format!("{name} exceeds 10 ms p99 budget").into());
                    }
                    let mut failure = None;
                    criterion.bench_function(&name, |b| {
                        b.iter_custom(|iterations| {
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                match measure() {
                                    Ok(elapsed) => total += elapsed,
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            total
                        });
                    });
                    if let Some(error) = failure {
                        return Err(error);
                    }
                }
            }
            Ok(())
        })();
        runtime.block_on(cleanup(&db, user, album))?;
        result?;
    }
    criterion.final_summary();
    Ok(())
}
