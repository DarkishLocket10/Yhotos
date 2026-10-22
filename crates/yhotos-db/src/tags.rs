//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{PhotoId, Slug, TagId};

/// Closed values accepted by the database enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "tag_source", rename_all = "lowercase")]
pub enum TagSource {
    /// Manual value.
    Manual,
    /// Auto value.
    Auto,
}

/// A canonical tag identity, provenance and hidden flag; access remains caller-owned.
#[derive(Clone, Debug)]
pub struct TagRow {
    /// Stored id.
    pub id: TagId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored slug.
    pub slug: Slug,
    /// Stored name.
    pub name: String,
    /// Stored source.
    pub source: TagSource,
    /// Stored hidden.
    pub hidden: bool,
}

/// An attached tag with its association provenance and bounded optional confidence.
#[derive(Clone, Debug)]
pub struct PhotoTagRow {
    /// Stored tag id.
    pub tag_id: TagId,
    /// Stored slug.
    pub slug: Slug,
    /// Stored name.
    pub name: String,
    /// Stored hidden.
    pub hidden: bool,
    /// Stored source.
    pub source: TagSource,
    /// Stored confidence.
    pub confidence: Option<f32>,
}

/// Creates a unique canonical tag or renames the existing tag, preserving its identity and source.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn upsert_by_slug(
    conn: &mut PgConnection,
    slug: &Slug,
    name: &str,
    source: TagSource,
) -> Result<TagRow, DbError> {
    Ok(sqlx::query_as!(TagRow,r#"INSERT INTO tags(id,slug,name,source) VALUES ($1,$2,$3,$4) ON CONFLICT (slug) DO UPDATE SET name=excluded.name RETURNING id as "id: _", created_at, updated_at, slug as "slug: _", name, source as "source: _", hidden"#, TagId::new() as _, slug as _, name, source as _).fetch_one(conn).await?)
}

/// Attaches a tag or replaces the existing attachment source and confidence.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn attach(
    conn: &mut PgConnection,
    photo: PhotoId,
    tag: TagId,
    source: TagSource,
    confidence: Option<f32>,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"INSERT INTO photo_tags(id,photo_id,tag_id,source,confidence) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (photo_id,tag_id) DO UPDATE SET source=excluded.source,confidence=excluded.confidence"#, Uuid::now_v7(), photo as _, tag as _, source as _, confidence).execute(conn).await?.rows_affected())
}

/// Removes one photo-tag association without deleting either entity.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn detach(conn: &mut PgConnection, photo: PhotoId, tag: TagId) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"DELETE FROM photo_tags WHERE photo_id=$1 AND tag_id=$2"#,
        photo as _,
        tag as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Fetches all attached tags with attachment source and confidence, including hidden tags.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn tags_of(conn: &mut PgConnection, photo: PhotoId) -> Result<Vec<PhotoTagRow>, DbError> {
    Ok(sqlx::query_as!(PhotoTagRow,r#"SELECT t.id AS "tag_id: TagId", t.slug AS "slug: Slug", t.name, t.hidden, pt.source AS "source: TagSource", pt.confidence FROM photo_tags pt JOIN tags t ON t.id=pt.tag_id WHERE pt.photo_id=$1 ORDER BY t.slug"#, photo as _).fetch_all(conn).await?)
}

/// Returns a stable zero-based page of up to fifty non-trashed photos without access filtering.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn photos_with_tag(
    conn: &mut PgConnection,
    tag: TagId,
    page: u32,
) -> Result<Vec<crate::photos::PhotoRow>, DbError> {
    Ok(sqlx::query_as!(crate::photos::PhotoRow,r#"SELECT p.id as "id: _", p.created_at, p.updated_at, p.short_id as "short_id: _", p.album_id as "album_id: _", p.title, p.description, p.taken_at, p.posted_at, p.sort_order, p.visibility as "visibility: _", p.original_hash, p.original_name, p.original_bytes, p.mime, p.kind as "kind: _", p.width, p.height, p.orientation, p.is_hdr, p.has_gain_map, p.icc_name, p.camera_make, p.camera_model, p.lens, p.focal_mm, p.aperture, p.shutter, p.iso, p.gps_lat, p.gps_lon, p.place_id, p.dominant_colors, p.placeholder, p.deleted_at FROM photos p JOIN photo_tags pt ON pt.photo_id=p.id WHERE pt.tag_id=$1 AND p.deleted_at IS NULL ORDER BY p.id LIMIT 50 OFFSET $2"#, tag as _, i64::from(page)*50).fetch_all(conn).await?)
}
