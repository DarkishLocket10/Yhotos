#![forbid(unsafe_code)]
//! Deterministic synthetic image publication into a disposable benchmark database.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};

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
        albums::create(conn, parent, slug, title, Visibility::Restricted)
            .await?
            .id,
    )
}

#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let data = std::env::var("YHOTOS_FIXTURE_DATA").context("YHOTOS_FIXTURE_DATA is required")?;
    let store = Store::open(&data)?;
    let db = Db::connect(&url.clone().into(), Role::Admin, 4, DatabaseTls::Disable).await?;
    db.migrate().await?;
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
    let restricted = fixture_album(
        &mut conn,
        None,
        &["restricted-fixture"],
        "Restricted fixture",
    )
    .await?;
    let images: Vec<FixtureImage> = serde_json::from_slice(
        &std::fs::read("deploy/dev/fixtures/generated.json")
            .context("Generate fixtures first; see deploy/dev/PAGES.md")?,
    )?;
    ensure!(
        images.len() == 300,
        "expected sixty photos with four full-frame sizes and a square crop"
    );
    let mut hashes = std::collections::HashSet::new();
    let mut public_photo = None;
    let mut restricted_photo = None;
    let mut derivative = None;
    let count: i32 = std::env::var("YHOTOS_FIXTURE_COUNT")
        .unwrap_or_else(|_| "60".into())
        .parse()?;
    ensure!(
        (60..=10_000).contains(&count),
        "fixture count must be 60..=10000"
    );
    for index in 0..count {
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
                .find(|image| image.index == index % 60 && image.variant == variant)
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
                index >= 60 || hashes.insert(hash.to_string()),
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
            vec![album.id, restricted]
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
            if index == 0 {
                let path = format!("/p/{}", yhotos_core::ShortId::from_photo_id(photo_id));
                if target == restricted {
                    restricted_photo = Some(path);
                } else {
                    public_photo = Some(path);
                }
            }
            let placeholder = vec![
                226_u8, 39, 142, 19, 150, 43, 144, 135, 129, 136, 120, 110, 144, 24, 249, 118, 128,
                135, 136, 136, 120, 136, 119,
            ];
            let hash = original.hash.to_string();
            let len = i64::try_from(original.bytes)?;
            sqlx::query!("UPDATE photos SET original_hash=$4,original_bytes=$5,width=2048,height=1365,dominant_colors=ARRAY[6191729],placeholder=$2,sort_order=$3 WHERE id=$1",photo_id.0,placeholder,index,hash,len).execute(&mut *conn).await?;
            for (variant, width, height, hash, len) in &representations {
                if index == 0 && *variant == "small" {
                    derivative = Some(format!("/i/{hash}/small.jpeg"));
                }
                sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,$3,'jpeg',$4,$5,$6,$7) ON CONFLICT (photo_id,variant,format,hdr) DO UPDATE SET hash=excluded.hash,width=excluded.width,height=excluded.height,bytes=excluded.bytes",uuid::Uuid::now_v7(),photo_id.0,*variant,hash,*width,*height,*len).execute(&mut *conn).await?;
            }
        }
    }
    let routes = serde_json::json!({
        "home": "/", "album": "/a/gallery-fixture",
        "photo": public_photo.context("missing public photo")?,
        "restricted": restricted_photo.context("missing restricted photo")?,
        "missing": format!("/p/{}", yhotos_core::ShortId::from_photo_id(yhotos_core::PhotoId::new())),
        "derivative": derivative.context("missing derivative")?,
        "search": "/search?q=garden", "photos": count
    });
    let output =
        std::env::var("YHOTOS_FIXTURE_ROUTES").context("YHOTOS_FIXTURE_ROUTES is required")?;
    std::fs::write(output, serde_json::to_vec_pretty(&routes)?)?;
    Ok(())
}
