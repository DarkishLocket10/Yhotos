#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]
//! Import identity and ingest metadata precedence over real PostgreSQL.
use sqlx::PgPool;
use time::{OffsetDateTime, PrimitiveDateTime};
use yhotos_core::{ShortId, Slug};
use yhotos_db::{
    jobs::{self, JobStatus},
    migration::{self, ImportPhoto},
    photos::{self, MediaKind, NewPhoto, PhotoMetadata},
};

#[sqlx::test(migrations = "./migrations")]
async fn bulk_identities_replacements_and_curated_metadata_survive_ingest(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    let album = migration::upsert_album(
        &mut tx,
        1,
        None,
        &Slug::from_title("Fixture"),
        "Fixture",
        "",
        0,
        false,
    )
    .await
    .unwrap();
    let date = OffsetDateTime::now_utc();
    let hash = format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    );
    let mut row = ImportPhoto {
        image_id: 1,
        category_id: 1,
        original: NewPhoto {
            album_id: album,
            title: "Title".into(),
            original_hash: hash,
            original_name: "one.jpg".into(),
            original_bytes: 12,
            mime: "image/jpeg".into(),
            kind: MediaKind::Image,
        },
        description: "Description".into(),
        author: Some("Author".into()),
        taken_at: Some(date),
        taken_at_local: Some(PrimitiveDateTime::new(date.date(), date.time())),
        posted_at: date,
        sort_order: 8,
        restricted: true,
        gps_lat: Some(45.0),
        gps_lon: Some(-73.0),
    };
    let first = migration::upsert_photos(&mut tx, std::slice::from_ref(&row))
        .await
        .unwrap();
    let second = migration::upsert_photos(&mut tx, std::slice::from_ref(&row))
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        jobs::list(&mut tx, JobStatus::Pending, 0)
            .await
            .unwrap()
            .len(),
        2
    );
    let id = *first.first().unwrap();
    // Ingest's worker role cannot read import identities directly. The fixed-
    // search-path trigger may preserve curated metadata without granting that access.
    sqlx::query!("SET LOCAL ROLE yhotos_worker")
        .execute(&mut *tx)
        .await
        .unwrap();
    // The publication statement includes derivative_plan and status, unlike
    // an administrative update_metadata call. Missing EXIF cannot erase Piwigo.
    sqlx::query!("UPDATE photos SET status='ready',derivative_plan='test-plan',taken_at=NULL,taken_at_local=NULL,gps_lat=NULL,gps_lon=NULL WHERE id=$1", id.0).execute(&mut *tx).await.unwrap();
    let photo = photos::get_by_short_id(&mut tx, &ShortId::from_photo_id(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        photo.taken_at.unwrap().unix_timestamp(),
        date.unix_timestamp()
    );
    assert_eq!(photo.gps_lat, Some(45.0));
    let author = sqlx::query!("SELECT author FROM photos WHERE id=$1", id.0)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(author.author.as_deref(), Some("Author"));
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    // After publication, admins can correct and then remove imported fields.
    let corrected = date + time::Duration::days(1);
    photos::update_metadata(
        &mut tx,
        id,
        &PhotoMetadata {
            title: "Corrected".into(),
            taken_at: Some(corrected),
            gps_lat: Some(12.0),
            gps_lon: Some(13.0),
            ..PhotoMetadata::default()
        },
    )
    .await
    .unwrap();
    let photo = photos::get_by_short_id(&mut tx, &ShortId::from_photo_id(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        photo.taken_at.unwrap().unix_timestamp(),
        corrected.unix_timestamp()
    );
    assert_eq!((photo.gps_lat, photo.gps_lon), (Some(12.0), Some(13.0)));
    // A rebuild with the same original/generation must retain administrative
    // corrections even if the codec reports the old EXIF/source values.
    sqlx::query!("SET LOCAL ROLE yhotos_worker")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("UPDATE photos SET status='ready',derivative_plan='rebuilt-corrected',taken_at=$2,taken_at_local=$3,gps_lat=45,gps_lon=-73 WHERE id=$1",id.0,date,PrimitiveDateTime::new(date.date(), date.time()))
        .execute(&mut *tx).await.unwrap();
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    let rebuilt = photos::get_by_short_id(&mut tx, &ShortId::from_photo_id(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rebuilt.taken_at.unwrap().unix_timestamp(),
        corrected.unix_timestamp()
    );
    assert_eq!((rebuilt.gps_lat, rebuilt.gps_lon), (Some(12.0), Some(13.0)));
    photos::update_metadata(&mut tx, id, &PhotoMetadata::default())
        .await
        .unwrap();
    sqlx::query!("UPDATE photos SET taken_at_local=NULL WHERE id=$1", id.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("SET LOCAL ROLE yhotos_worker")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("UPDATE photos SET status='ready',derivative_plan='rebuilt-removed',taken_at=$2,taken_at_local=$3,gps_lat=45,gps_lon=-73 WHERE id=$1",id.0,date,PrimitiveDateTime::new(date.date(), date.time()))
        .execute(&mut *tx).await.unwrap();
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    let invalidated = sqlx::query!("SELECT taken_at,taken_at_local,gps_lat,gps_lon,metadata_edited FROM migration_sources WHERE photo_id=$1",id.0)
        .fetch_one(&mut *tx).await.unwrap();
    assert!(invalidated.metadata_edited);
    assert!(
        invalidated.taken_at.is_none()
            && invalidated.taken_at_local.is_none()
            && invalidated.gps_lat.is_none()
            && invalidated.gps_lon.is_none()
    );
    let cleared = sqlx::query!(
        "SELECT taken_at,taken_at_local,gps_lat,gps_lon FROM photos WHERE id=$1",
        id.0
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        (
            cleared.taken_at,
            cleared.taken_at_local,
            cleared.gps_lat,
            cleared.gps_lon
        ),
        (None, None, None, None)
    );
    row.original.original_hash = format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    );
    row.original.title = "Replaced".into();
    assert_eq!(
        migration::upsert_photos(&mut tx, &[row]).await.unwrap(),
        first
    );
    let queued = jobs::list(&mut tx, JobStatus::Pending, 0).await.unwrap();
    assert_eq!(queued.len(), 4);
    assert!(queued.iter().any(|j| {
        j.payload
            .get("generation")
            .is_some_and(|generation| *generation == 1)
    }));
    // Identical bytes in a later ingest generation cannot revive source-only
    // metadata from the migration's earlier intended generation.
    sqlx::query!(
        "UPDATE photos SET generation=generation+1 WHERE id=$1",
        id.0
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query!("SET LOCAL ROLE yhotos_worker")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query!("UPDATE photos SET status='ready',derivative_plan='later-generation',taken_at=NULL,taken_at_local=NULL,gps_lat=NULL,gps_lon=NULL WHERE id=$1",id.0)
        .execute(&mut *tx).await.unwrap();
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
    let later = photos::get_by_short_id(&mut tx, &ShortId::from_photo_id(id))
        .await
        .unwrap()
        .unwrap();
    assert!(later.taken_at.is_none() && later.gps_lat.is_none() && later.gps_lon.is_none());
    tx.rollback().await.unwrap();
}
