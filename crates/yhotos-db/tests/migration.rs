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
        1
    );
    let id = *first.first().unwrap();
    // Ingest's worker role cannot read import identities directly. The fixed-
    // search-path trigger may preserve curated metadata without granting that access.
    sqlx::query!("SET LOCAL ROLE yhotos_worker")
        .execute(&mut *tx)
        .await
        .unwrap();
    // Ingest may supply missing EXIF fields but cannot erase curated DB values.
    photos::update_metadata(
        &mut tx,
        id,
        &PhotoMetadata {
            title: "Title".into(),
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
        date.unix_timestamp()
    );
    assert_eq!(photo.gps_lat, Some(45.0));
    let author = sqlx::query!("SELECT author FROM photos WHERE id=$1", id.0)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(author.author.as_deref(), Some("Author"));
    sqlx::query!("RESET ROLE").execute(&mut *tx).await.unwrap();
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
    assert_eq!(queued.len(), 2);
    assert!(
        queued
            .iter()
            .any(|j| *j.payload.get("generation").unwrap() == 1)
    );
    tx.rollback().await.unwrap();
}
