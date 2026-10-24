#![forbid(unsafe_code)]
//! Full-router share landing and scoped first-byte cost, with disposable DB rows.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use criterion::Criterion;
use http_body_util::BodyExt;
use secrecy::SecretString;
use std::{error::Error, net::SocketAddr, sync::Arc, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;
use yhotos_auth::{Token, TokenKind};
use yhotos_config::{Config, LogFormat, RateLimits, Upload};
use yhotos_core::{
    Slug,
    access::{ShareTarget, Visibility},
};
use yhotos_db::{
    DatabaseTls, Db, Role, albums, photos,
    share_links::{self, LinkFlags, NewShareLink},
    users,
};
use yhotos_http::{AppState, auth::Auth, public_router};
use yhotos_storage::{DerivativeKey, Store};
type BenchResult<T> = Result<T, Box<dyn Error>>;
async fn request(app: &Router, path: &str, cookie: &str) -> BenchResult<axum::response::Response> {
    let mut r = Request::builder()
        .uri(path)
        .header(header::COOKIE, cookie)
        .body(Body::empty())?;
    r.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?));
    let response = app.clone().oneshot(r).await?;
    if response.status() != StatusCode::OK {
        return Err("share benchmark request failed".into());
    }
    Ok(response)
}
async fn first_byte(app: &Router, path: &str, cookie: &str) -> BenchResult<()> {
    let mut body = request(app, path, cookie).await?.into_body();
    let frame = body.frame().await.ok_or("empty share response")??;
    if frame.data_ref().is_none_or(axum::body::Bytes::is_empty) {
        return Err("no first byte".into());
    }
    Ok(())
}
fn config(dir: &std::path::Path, url: SecretString) -> BenchResult<&'static Config> {
    let peer = "127.0.0.1:1".parse()?;
    let key = Token::generate().hash(TokenKind::Session);
    Ok(Box::leak(Box::new(Config {
        ml_address: "127.0.0.1:1".into(),
        map_tiles: yhotos_config::MapTiles::default(),
        public_listen: peer,
        admin_listen: None,
        database_url_public: url,
        database_url_admin: None,
        database_url_worker: None,
        worker: yhotos_config::Worker::default(),
        admin_base_url: None,
        data_dir: dir.to_owned(),
        public_base_url: "https://example.test".parse()?,
        session_key: STANDARD.encode(key.as_bytes()).into(),
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
            max_bytes: 1,
            chunk_bytes: 1,
        },
        log_format: LogFormat::Json,
    })))
}
async fn cleanup(
    admin: &Db,
    user: yhotos_core::UserId,
    album: yhotos_core::AlbumId,
) -> BenchResult<()> {
    sqlx::query!("DELETE FROM share_links WHERE created_by=$1", user as _)
        .execute(admin.pool())
        .await?;
    sqlx::query!("DELETE FROM photos WHERE album_id=$1", album as _)
        .execute(admin.pool())
        .await?;
    sqlx::query!("DELETE FROM albums WHERE id=$1", album as _)
        .execute(admin.pool())
        .await?;
    sqlx::query!("DELETE FROM users WHERE id=$1", user as _)
        .execute(admin.pool())
        .await?;
    Ok(())
}
fn main() -> BenchResult<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let dir = tempfile::tempdir()?;
    let url = SecretString::from(std::env::var("DATABASE_URL")?);
    let config = config(dir.path(), url.clone())?;
    let admin = runtime.block_on(Db::connect(&url, Role::Admin, 2, DatabaseTls::Disable))?;
    let public =
        Arc::new(runtime.block_on(Db::connect(&url, Role::Public, 2, DatabaseTls::Disable))?);
    let store = Store::open(dir.path())?;
    let (user,album,path,image)=runtime.block_on(async {
        let mut c=admin.pool().acquire().await?;
        let password=yhotos_auth::generate_temp_password();let phc=yhotos_auth::hash_password(&password)?;
        let user=users::create(&mut c,&Uuid::now_v7().to_string(),&SecretString::from(phc.as_str()),true).await?;
        let album=albums::create(&mut c,None,&Slug::new(&format!("share-bench-{}",Uuid::now_v7().simple()))?,"Share benchmark",Visibility::Restricted).await?;
        let data=vec![73;4096];let put=store.put_original(data.as_slice(),4096).await?;let hash=put.hash.to_string();
        let photo=photos::create(&mut c,&photos::NewPhoto{album_id:album.id,title:"Benchmark".into(),original_hash:hash.clone(),original_name:"bench.jpeg".into(),original_bytes:4096,mime:"image/jpeg".into(),kind:photos::MediaKind::Image}).await?;
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES($1,$2,'small','jpeg',false,$3,640,480,4096)",Uuid::now_v7(),photo.id as _,hash).execute(&mut *c).await?;
        let tmp=store.tmp_dir()?;let source=tmp.path().join("derivative");tokio::fs::write(&source,data).await?;
        store.put_derivative(&DerivativeKey{hash:&put.hash,variant:"small",format:"jpeg",hdr:false},&source).await?;
        let token=Token::generate();share_links::create(&mut c,&NewShareLink{token_hash:*token.hash(TokenKind::ShareLink).as_bytes(),target:ShareTarget::Album(album.id),passphrase_hash:None,expires_at:None,flags:LinkFlags{view:true,..LinkFlags::default()},created_by:user.id}).await?;
        Ok::<_,Box<dyn Error>>((user.id,album.id,format!("/s/{}",token.encode(TokenKind::ShareLink)),format!("/i/{hash}/small.jpeg")))
    })?;
    let auth = runtime.block_on(Auth::new(config, public.clone(), None))?;
    let state = AppState::new(config, public.pool().clone(), store, Some(auth));
    let app = public_router(state.clone());
    // Scope resolution must be independent of the landing's page-cache key.
    let default_path = path.clone();
    let path = format!("{path}?sort=name");
    let response = runtime.block_on(request(&app, &path, ""))?;
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|c| c.to_str().map(|s| s.split(';').next().unwrap_or_default()))
        .collect::<Result<Vec<_>, _>>()?
        .join("; ");
    drop(response);
    // Both sort keys share one cached grant set; repeats only insert an open.
    runtime.block_on(first_byte(&app, &default_path, &cookie))?;
    runtime.block_on(first_byte(&app, &image, &cookie))?;
    let mut criterion = Criterion::default().configure_from_args();
    let mut failure = None;
    for (name, path, cold, cold_session) in [
        (
            "share/landing_warm_default_sort_including_open_write",
            &default_path,
            false,
            false,
        ),
        (
            "share/landing_warm_including_open_write",
            &path,
            false,
            false,
        ),
        (
            "share/landing_cold_with_scope_resolution_and_open_write",
            &path,
            true,
            true,
        ),
        ("share/image_warm_first_byte", &image, false, false),
        (
            "share/image_cold_file_lookup_first_byte",
            &image,
            true,
            false,
        ),
        (
            "share/image_cold_session_and_file_first_byte",
            &image,
            true,
            true,
        ),
    ] {
        criterion.bench_function(name, |b| {
            b.iter(|| {
                if cold_session && let Some(auth) = &state.auth {
                    auth.invalidate(Uuid::nil());
                }
                if cold && !path.starts_with("/s/") {
                    state.files.invalidate_all();
                }
                if let Err(error) = runtime.block_on(first_byte(&app, path, &cookie)) {
                    failure = Some(error);
                }
            });
        });
    }
    criterion.final_summary();
    runtime.block_on(cleanup(&admin, user, album))?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
