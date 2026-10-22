#![forbid(unsafe_code)]
//! Local-only fixture server for the public-page browser and Lighthouse checks.
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::net::SocketAddr;
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{DatabaseTls, Db, Role, albums, photos};
use yhotos_storage::{ContentHash, DerivativeKey, Store};

#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let data = tempfile::tempdir()?;
    let store = Store::open(data.path())?;
    let db = Db::connect(&url.clone().into(), Role::Admin, 4, DatabaseTls::Disable).await?;
    let mut conn = db.pool().acquire().await?;
    let slug = Slug::new("gallery-fixture")?;
    let album = match albums::get_by_path(&mut conn, std::slice::from_ref(&slug)).await? {
        Some(album) => album,
        None => {
            albums::create(
                &mut conn,
                None,
                &slug,
                "Fixture gallery",
                Visibility::Public,
            )
            .await?
        }
    };
    let originals = include_bytes!("fixtures/large.jpeg");
    let put = store
        .put_original(originals.as_slice(), originals.len() as u64)
        .await?;
    let mut representations = Vec::new();
    for (variant, width, bytes) in [
        (
            "small",
            640,
            include_bytes!("fixtures/small.jpeg").as_slice(),
        ),
        (
            "medium",
            1280,
            include_bytes!("fixtures/medium.jpeg").as_slice(),
        ),
        ("large", 2048, originals.as_slice()),
    ] {
        let hash: ContentHash = blake3::hash(bytes).to_hex().as_str().parse()?;
        let source_dir = store.tmp_dir()?;
        let source = source_dir.path().join("fixture.jpeg");
        std::fs::write(&source, bytes)?;
        store
            .put_derivative(
                &DerivativeKey {
                    hash: &hash,
                    variant,
                    format: "jpeg",
                    hdr: false,
                },
                &source,
            )
            .await?;
        representations.push((
            variant,
            width,
            hash.to_string(),
            i64::try_from(bytes.len())?,
        ));
    }
    let page = albums::page(
        &mut conn,
        &yhotos_core::access::Principal::Anonymous,
        std::slice::from_ref(&slug),
        1,
        photos::PageSort::Taken,
    )
    .await?;
    if page.is_none_or(|page| page.total == 0) {
        for index in 0..60 {
            let photo = photos::create(
                &mut conn,
                &photos::NewPhoto {
                    album_id: album.id,
                    title: format!("Garden view {index:02}"),
                    original_hash: put.hash.to_string(),
                    original_name: "garden.jpeg".into(),
                    original_bytes: i64::try_from(originals.len())?,
                    mime: "image/jpeg".into(),
                    kind: photos::MediaKind::Image,
                },
            )
            .await?;
            let placeholder = vec![
                226_u8, 39, 142, 19, 150, 43, 144, 135, 129, 136, 120, 110, 144, 24, 249, 118, 128,
                135, 136, 136, 120, 136, 119,
            ];
            sqlx::query!("UPDATE photos SET width=2048,height=1365,dominant_colors=ARRAY[6191729],placeholder=$2,sort_order=$3 WHERE id=$1",photo.id.0,placeholder,index).execute(&mut *conn).await?;
            for (variant, width, hash, len) in &representations {
                let height = width * 2 / 3;
                sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,$3,'jpeg',$4,$5,$6,$7)",uuid::Uuid::now_v7(),photo.id.0,*variant,hash,*width,height,*len).execute(&mut *conn).await?;
            }
        }
    }
    drop(conn);
    let session = STANDARD.encode(format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7()));
    let config = Box::leak(Box::new(yhotos_config::Config::from_figment(
        figment::Figment::new()
            .merge(("database_url", url))
            .merge(("session_key", session))
            .merge(("data_dir", data.path().to_string_lossy().as_ref()))
            .merge(("public_base_url", "https://gallery.example"))
            .merge(("admin_listen", "127.0.0.1:8081"))
            .merge(("rate_limits.cheap_per_min", 100_000)),
    )?));
    let state = yhotos_http::AppState::new(config, db.pool().clone(), store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18011").await?;
    eprintln!("Fixture album: http://127.0.0.1:18011/a/gallery-fixture");
    axum::serve(
        listener,
        yhotos_http::public_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
