//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{AlbumId, PhotoId, ShortId, access::Visibility};

/// Closed values accepted by the database enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "media_kind", rename_all = "lowercase")]
pub enum MediaKind {
    /// Image value.
    Image,
    /// Raw value.
    Raw,
    /// Video value.
    Video,
}

/// Original and extracted metadata with a canonical short id; callers must check access.
#[derive(Clone, Debug)]
pub struct PhotoRow {
    /// Stored id.
    pub id: PhotoId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored short id.
    pub short_id: ShortId,
    /// Stored album id.
    pub album_id: AlbumId,
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored taken at.
    pub taken_at: Option<OffsetDateTime>,
    /// Stored posted at.
    pub posted_at: OffsetDateTime,
    /// Stored sort order.
    pub sort_order: i32,
    /// Stored visibility.
    pub visibility: Visibility,
    /// Stored original hash.
    pub original_hash: String,
    /// Stored original name.
    pub original_name: String,
    /// Stored original bytes.
    pub original_bytes: i64,
    /// Stored mime.
    pub mime: String,
    /// Stored kind.
    pub kind: MediaKind,
    /// Stored width.
    pub width: Option<i32>,
    /// Stored height.
    pub height: Option<i32>,
    /// Stored orientation.
    pub orientation: Option<i32>,
    /// Stored is hdr.
    pub is_hdr: bool,
    /// Stored has gain map.
    pub has_gain_map: bool,
    /// Stored icc name.
    pub icc_name: Option<String>,
    /// Stored camera make.
    pub camera_make: Option<String>,
    /// Stored camera model.
    pub camera_model: Option<String>,
    /// Stored lens.
    pub lens: Option<String>,
    /// Stored focal mm.
    pub focal_mm: Option<f64>,
    /// Stored aperture.
    pub aperture: Option<f64>,
    /// Stored shutter.
    pub shutter: Option<String>,
    /// Stored iso.
    pub iso: Option<i32>,
    /// Stored gps lat.
    pub gps_lat: Option<f64>,
    /// Stored gps lon.
    pub gps_lon: Option<f64>,
    /// Stored place id.
    pub place_id: Option<Uuid>,
    /// Stored dominant colors.
    pub dominant_colors: Vec<i32>,
    /// Stored placeholder.
    pub placeholder: Option<Vec<u8>>,
    /// Stored deleted at.
    pub deleted_at: Option<OffsetDateTime>,
}

/// A photo plus its derivative descriptors from one SELECT, before access checking.
#[derive(Clone, Debug)]
pub struct AlbumPhotoRow {
    /// Stored id.
    pub id: PhotoId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored short id.
    pub short_id: ShortId,
    /// Stored album id.
    pub album_id: AlbumId,
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored taken at.
    pub taken_at: Option<OffsetDateTime>,
    /// Stored posted at.
    pub posted_at: OffsetDateTime,
    /// Stored sort order.
    pub sort_order: i32,
    /// Stored visibility.
    pub visibility: Visibility,
    /// Stored original hash.
    pub original_hash: String,
    /// Stored original name.
    pub original_name: String,
    /// Stored original bytes.
    pub original_bytes: i64,
    /// Stored mime.
    pub mime: String,
    /// Stored kind.
    pub kind: MediaKind,
    /// Stored width.
    pub width: Option<i32>,
    /// Stored height.
    pub height: Option<i32>,
    /// Stored orientation.
    pub orientation: Option<i32>,
    /// Stored is hdr.
    pub is_hdr: bool,
    /// Stored has gain map.
    pub has_gain_map: bool,
    /// Stored icc name.
    pub icc_name: Option<String>,
    /// Stored camera make.
    pub camera_make: Option<String>,
    /// Stored camera model.
    pub camera_model: Option<String>,
    /// Stored lens.
    pub lens: Option<String>,
    /// Stored focal mm.
    pub focal_mm: Option<f64>,
    /// Stored aperture.
    pub aperture: Option<f64>,
    /// Stored shutter.
    pub shutter: Option<String>,
    /// Stored iso.
    pub iso: Option<i32>,
    /// Stored gps lat.
    pub gps_lat: Option<f64>,
    /// Stored gps lon.
    pub gps_lon: Option<f64>,
    /// Stored place id.
    pub place_id: Option<Uuid>,
    /// Stored dominant colors.
    pub dominant_colors: Vec<i32>,
    /// Stored placeholder.
    pub placeholder: Option<Vec<u8>>,
    /// Stored deleted at.
    pub deleted_at: Option<OffsetDateTime>,
    /// Stored derivatives.
    pub derivatives: serde_json::Value,
}

/// Supported album orderings; identifiers break ties for stable pagination.
#[derive(Clone, Copy, Debug)]
pub enum PhotoOrder {
    /// Album manual order, ascending.
    Manual,
    /// Capture date, newest first, unknown dates last.
    Taken,
    /// Posted date, newest first.
    Posted,
    /// Title, ascending.
    Name,
}

/// Required original fields; create generates the `UUIDv7` and canonical short lookup key.
#[derive(Clone, Debug)]
pub struct NewPhoto {
    /// Stored album id.
    pub album_id: AlbumId,
    /// Stored title.
    pub title: String,
    /// Stored original hash.
    pub original_hash: String,
    /// Stored original name.
    pub original_name: String,
    /// Stored original bytes.
    pub original_bytes: i64,
    /// Stored mime.
    pub mime: String,
    /// Stored kind.
    pub kind: MediaKind,
}

/// Complete replacement metadata; absent optional values clear stored fields.
#[derive(Clone, Debug, Default)]
pub struct PhotoMetadata {
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored taken at.
    pub taken_at: Option<OffsetDateTime>,
    /// Stored width.
    pub width: Option<i32>,
    /// Stored height.
    pub height: Option<i32>,
    /// Stored orientation.
    pub orientation: Option<i32>,
    /// Stored is hdr.
    pub is_hdr: bool,
    /// Stored has gain map.
    pub has_gain_map: bool,
    /// Stored icc name.
    pub icc_name: Option<String>,
    /// Stored camera make.
    pub camera_make: Option<String>,
    /// Stored camera model.
    pub camera_model: Option<String>,
    /// Stored lens.
    pub lens: Option<String>,
    /// Stored focal mm.
    pub focal_mm: Option<f64>,
    /// Stored aperture.
    pub aperture: Option<f64>,
    /// Stored shutter.
    pub shutter: Option<String>,
    /// Stored iso.
    pub iso: Option<i32>,
    /// Stored gps lat.
    pub gps_lat: Option<f64>,
    /// Stored gps lon.
    pub gps_lon: Option<f64>,
    /// Stored place id.
    pub place_id: Option<Uuid>,
    /// Stored dominant colors.
    pub dominant_colors: Vec<i32>,
    /// Stored placeholder.
    pub placeholder: Option<Vec<u8>>,
}

/// Inserts an original with an app-generated `UUIDv7` and stable short lookup key.
///
/// # Errors
/// Returns invalid hash/size constraints or another database failure.
pub async fn create(conn: &mut PgConnection, photo: &NewPhoto) -> Result<PhotoRow, DbError> {
    let id = PhotoId::new();
    let short = ShortId::from_photo_id(id);
    Ok(sqlx::query_as!(PhotoRow, r#"INSERT INTO photos (id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind)
      VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id as "id: _", created_at, updated_at, short_id as "short_id: _", album_id as "album_id: _", title, description, taken_at, posted_at, sort_order, visibility as "visibility: _", original_hash, original_name, original_bytes, mime, kind as "kind: _", width, height, orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens, focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id, dominant_colors, placeholder, deleted_at"#, id as _, short as _, photo.album_id as _, photo.title, photo.original_hash, photo.original_name, photo.original_bytes, photo.mime, photo.kind as _).fetch_one(conn).await?)
}

/// Get by short id with bound parameters and no visibility policy.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_short_id(
    conn: &mut PgConnection,
    short: &ShortId,
) -> Result<Option<PhotoRow>, DbError> {
    Ok(sqlx::query_as!(PhotoRow,r#"SELECT id as "id: _", created_at, updated_at, short_id as "short_id: _", album_id as "album_id: _", title, description, taken_at, posted_at, sort_order, visibility as "visibility: _", original_hash, original_name, original_bytes, mime, kind as "kind: _", width, height, orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens, focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id, dominant_colors, placeholder, deleted_at FROM photos WHERE short_id=$1"#, short as _).fetch_optional(conn).await?)
}

/// Returns every owner of a content hash; the caller must authorize each photo.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_hash(conn: &mut PgConnection, hash: &str) -> Result<Vec<PhotoRow>, DbError> {
    Ok(sqlx::query_as!(PhotoRow,r#"SELECT id as "id: _", created_at, updated_at, short_id as "short_id: _", album_id as "album_id: _", title, description, taken_at, posted_at, sort_order, visibility as "visibility: _", original_hash, original_name, original_bytes, mime, kind as "kind: _", width, height, orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens, focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id, dominant_colors, placeholder, deleted_at FROM photos WHERE original_hash=$1 ORDER BY id"#, hash).fetch_all(conn).await?)
}

/// Returns a bounded photo page and derivative descriptors with one round trip.
/// Visibility is passed through for the caller's access decisions.
///
/// # Errors
/// Rejects page sizes outside 1..=200 and returns database failures.
pub async fn list_in_album(
    conn: &mut PgConnection,
    album: AlbumId,
    order: PhotoOrder,
    page: u32,
    per_page: u32,
) -> Result<Vec<AlbumPhotoRow>, DbError> {
    let (limit, offset) = crate::page(page, per_page)?;
    let order = match order {
        PhotoOrder::Manual => "manual",
        PhotoOrder::Taken => "taken",
        PhotoOrder::Posted => "posted",
        PhotoOrder::Name => "name",
    };
    Ok(sqlx::query_file_as!(
        AlbumPhotoRow,
        "queries/list_in_album.sql",
        album as _,
        order,
        limit,
        offset
    )
    .fetch_all(conn)
    .await?)
}

/// Replaces the extracted metadata set; absent optional values clear stored values.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn update_metadata(
    conn: &mut PgConnection,
    id: PhotoId,
    metadata: &PhotoMetadata,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"UPDATE photos SET title=$2,description=$3,taken_at=$4,width=$5,height=$6,orientation=$7,is_hdr=$8,has_gain_map=$9,icc_name=$10,camera_make=$11,camera_model=$12,lens=$13,focal_mm=$14,aperture=$15,shutter=$16,iso=$17,gps_lat=$18,gps_lon=$19,place_id=$20,dominant_colors=$21,placeholder=$22 WHERE id=$1"#, id as _, metadata.title, metadata.description, metadata.taken_at, metadata.width, metadata.height, metadata.orientation, metadata.is_hdr, metadata.has_gain_map, metadata.icc_name, metadata.camera_make, metadata.camera_model, metadata.lens, metadata.focal_mm, metadata.aperture, metadata.shutter, metadata.iso, metadata.gps_lat, metadata.gps_lon, metadata.place_id, &metadata.dominant_colors, metadata.placeholder).execute(conn).await?.rows_affected())
}

/// Moves the supplied photos together in one statement, preserving their URLs.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn move_to_album(
    conn: &mut PgConnection,
    ids: &[PhotoId],
    album: AlbumId,
) -> Result<u64, DbError> {
    let ids: Vec<_> = ids.iter().map(|id| id.0).collect();
    Ok(sqlx::query!(
        r#"UPDATE photos SET album_id=$2 WHERE id=ANY($1)"#,
        &ids,
        album as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Sets the explicit visibility of all supplied photos without resolving access.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn set_visibility(
    conn: &mut PgConnection,
    ids: &[PhotoId],
    visibility: Visibility,
) -> Result<u64, DbError> {
    let ids: Vec<_> = ids.iter().map(|id| id.0).collect();
    Ok(sqlx::query!(
        r#"UPDATE photos SET visibility=$2 WHERE id=ANY($1)"#,
        &ids,
        visibility as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Moves a photo to trash while preserving its original and interactions.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn soft_delete(conn: &mut PgConnection, id: PhotoId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE photos SET deleted_at=now() WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Restores a trashed photo without changing its URL.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn restore(conn: &mut PgConnection, id: PhotoId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE photos SET deleted_at=NULL WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Permanently deletes older trash rows and their dependent records; storage cleanup is separate.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn purge_deleted_before(
    conn: &mut PgConnection,
    before: OffsetDateTime,
) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"DELETE FROM photos WHERE deleted_at < $1"#, before)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Replaces original metadata without changing the photo URL or interactions.
/// Removes stale derivatives, video, tags and ML data in the same transaction.
///
/// # Errors
/// Returns constraint or connection failures; no partial replacement is committed.
pub async fn replace_original(
    conn: &mut PgConnection,
    id: PhotoId,
    original: &NewPhoto,
    metadata: &PhotoMetadata,
) -> Result<(), DbError> {
    use sqlx::Connection;
    let mut tx = conn.begin().await?;
    sqlx::query!("UPDATE photos SET original_hash=$2, original_name=$3, original_bytes=$4, mime=$5, kind=$6 WHERE id=$1", id as _, original.original_hash, original.original_name, original.original_bytes, original.mime, original.kind as _).execute(&mut *tx).await?;
    update_metadata(&mut tx, id, metadata).await?;
    sqlx::query!("DELETE FROM derivatives WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM video_renditions WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM embeddings WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM faces WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM ocr_text WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM photo_tags WHERE photo_id=$1 AND source='auto'",
        id as _
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
