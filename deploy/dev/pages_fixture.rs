#![forbid(unsafe_code)]
//! Local-only fixture server for the public-page browser and Lighthouse checks.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::net::SocketAddr;
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{DatabaseTls, Db, Role, albums, photos};
use yhotos_storage::{ContentHash, DerivativeKey, Store};

#[derive(serde::Deserialize)]
struct FixtureImage {
    index: i32,
    variant: String,
    width: i32,
    height: i32,
    base64: String,
}

async fn fixture_album(
    conn: &mut sqlx::PgConnection,
    parent: Option<yhotos_core::AlbumId>,
    path: &[&str],
    title: &str,
) -> Result<yhotos_core::AlbumId> {
    let slugs = path
        .iter()
        .map(|value| Slug::new(value))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(album) = albums::get_by_path(conn, &slugs).await? {
        return Ok(album.id);
    }
    let slug = slugs.last().context("fixture path is empty")?;
    Ok(
        albums::create(conn, parent, slug, title, Visibility::Public)
            .await?
            .id,
    )
}

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
    let mobile_parent =
        fixture_album(&mut conn, None, &["mobile-fixture"], "Album cover fixture").await?;
    let mobile_child = fixture_album(
        &mut conn,
        Some(mobile_parent),
        &["mobile-fixture", "child"],
        "Child album fixture",
    )
    .await?;
    let single =
        fixture_album(&mut conn, None, &["single-fixture"], "Single photo fixture").await?;
    let images: Vec<FixtureImage> = serde_json::from_slice(
        &std::fs::read("deploy/dev/fixtures/generated.json")
            .context("Generate fixtures first; see deploy/dev/PAGES.md")?,
    )?;
    ensure!(
        images.len() == 300,
        "expected sixty photos with four full-frame sizes and a square crop"
    );
    let mut hashes = std::collections::HashSet::new();
    for index in 0..60 {
        let mut representations = Vec::new();
        let mut original = None;
        for (variant, width) in [
            ("thumb", 320),
            ("thumbsquare", 320),
            ("small", 640),
            ("medium", 1280),
            ("large", 2048),
        ] {
            let image = images
                .iter()
                .find(|image| image.index == index && image.variant == variant)
                .context("missing fixture representation")?;
            let height = if variant == "thumbsquare" {
                width
            } else {
                (width * 2 + 1) / 3
            };
            ensure!(
                image.width == width && image.height == height,
                "incorrect fixture dimensions"
            );
            let bytes = STANDARD.decode(&image.base64)?;
            let hash: ContentHash = blake3::hash(&bytes).to_hex().as_str().parse()?;
            ensure!(
                hashes.insert(hash.to_string()),
                "fixture images must have distinct content hashes"
            );
            let source_dir = store.tmp_dir()?;
            let source = source_dir.path().join("fixture.jpeg");
            std::fs::write(&source, &bytes)?;
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
            if variant == "large" {
                original = Some(
                    store
                        .put_original(bytes.as_slice(), bytes.len() as u64)
                        .await?,
                );
            }
            representations.push((
                variant,
                width,
                height,
                hash.to_string(),
                i64::try_from(bytes.len())?,
            ));
        }
        let original = original.context("missing fixture original")?;
        let targets = if index == 0 {
            vec![album.id, mobile_child, single]
        } else {
            vec![album.id]
        };
        for target in targets {
            let title = format!("Garden view {index:02}");
            let existing = sqlx::query!(
                "SELECT id FROM photos WHERE album_id=$1 AND title=$2 AND deleted_at IS NULL",
                target.0,
                title
            )
            .fetch_optional(&mut *conn)
            .await?;
            let photo_id = if let Some(row) = existing {
                yhotos_core::PhotoId(row.id)
            } else {
                photos::create(
                    &mut conn,
                    &photos::NewPhoto {
                        album_id: target,
                        title,
                        original_hash: original.hash.to_string(),
                        original_name: "garden.jpeg".into(),
                        original_bytes: i64::try_from(original.bytes)?,
                        mime: "image/jpeg".into(),
                        kind: photos::MediaKind::Image,
                    },
                )
                .await?
                .id
            };
            let placeholder = vec![
                226_u8, 39, 142, 19, 150, 43, 144, 135, 129, 136, 120, 110, 144, 24, 249, 118, 128,
                135, 136, 136, 120, 136, 119,
            ];
            let hash = original.hash.to_string();
            let len = i64::try_from(original.bytes)?;
            sqlx::query!("UPDATE photos SET original_hash=$4,original_bytes=$5,width=2048,height=1365,dominant_colors=ARRAY[6191729],placeholder=$2,sort_order=$3 WHERE id=$1",photo_id.0,placeholder,index,hash,len).execute(&mut *conn).await?;
            for (variant, width, height, hash, len) in &representations {
                sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,$3,'jpeg',$4,$5,$6,$7) ON CONFLICT (photo_id,variant,format,hdr) DO UPDATE SET hash=excluded.hash,width=excluded.width,height=excluded.height,bytes=excluded.bytes",uuid::Uuid::now_v7(),photo_id.0,*variant,hash,*width,*height,*len).execute(&mut *conn).await?;
            }
        }
    }
    let place = sqlx::query!(
        "SELECT id FROM places WHERE name='Fixture city' AND country='CA' ORDER BY id LIMIT 1"
    )
    .fetch_optional(&mut *conn)
    .await?;
    let place = if let Some(row) = place {
        row.id
    } else {
        let id = uuid::Uuid::now_v7();
        sqlx::query!("INSERT INTO places(id,name,admin1,country,lat,lon) VALUES($1,'Fixture city','Fixture region','CA',43.65,-79.38)",id)
            .execute(&mut *conn).await?;
        id
    };
    // All sixty photos deliberately share one coordinate to exercise the chooser.
    sqlx::query!(
        "UPDATE photos SET place_id=$1,gps_lat=43.65,gps_lon=-79.38 WHERE album_id=$2",
        place,
        album.id.0
    )
    .execute(&mut *conn)
    .await?;
    drop(conn);
    let session = STANDARD.encode(format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7()));
    let config = Box::leak(Box::new(yhotos_config::Config::from_figment(
        figment::Figment::new()
            .merge(("database_url_public", url))
            .merge(("session_key", session))
            .merge(("data_dir", data.path().to_string_lossy().as_ref()))
            .merge(("public_base_url", "https://gallery.example"))
            .merge(("admin_listen", "127.0.0.1:8081"))
            .merge(("admin_base_url", "https://admin.gallery.example"))
            .merge(("database_url_admin", "postgres://127.0.0.1/test"))
            .merge(("rate_limits.cheap_per_min", 100_000)),
    )?));
    let state = yhotos_http::AppState::new(config, db.pool().clone(), store, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18011").await?;
    eprintln!("Fixture album: http://127.0.0.1:18011/a/gallery-fixture");
    axum::serve(
        listener,
        yhotos_http::public_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
