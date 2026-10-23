//! Single-statement file lookups with complete owning-photo access context.

use crate::{
    DbError,
    photos::{OwnerRow, file_owners},
};
use sqlx::PgConnection;

/// Returns every live owner of this exact derivative, including ancestor grants.
/// MIME and dimensions describe the matched row; callers must authorize before opening.
///
/// # Errors
/// Returns invalid stored metadata or database failures.
pub async fn lookup(
    conn: &mut PgConnection,
    hash: &str,
    variant: &str,
    format: &str,
    hdr: bool,
) -> Result<Vec<OwnerRow>, DbError> {
    file_owners(conn, hash, "derivative", variant, format, hdr).await
}

/// Returns live owners of non-HLS MP4 bytes by hash, independently of codec.
///
/// # Errors
/// Returns invalid stored metadata or database failures.
pub async fn lookup_video(conn: &mut PgConnection, hash: &str) -> Result<Vec<OwnerRow>, DbError> {
    file_owners(conn, hash, "video", "", "mp4", false).await
}

/// Scrub metadata for an already authorized photo; callers validate hash and duration.
/// # Errors
/// Returns database failures or no published sprite.
pub async fn scrub(
    conn: &mut PgConnection,
    photo: yhotos_core::PhotoId,
) -> Result<Option<(String, i64)>, DbError> {
    let row=sqlx::query!("SELECT d.hash,v.duration_ms FROM derivatives d JOIN video_renditions v ON v.photo_id=d.photo_id WHERE d.photo_id=$1 AND d.variant='scrub' AND d.format='jpeg' AND NOT d.hdr AND v.codec='h264' AND NOT v.hls",photo.0).fetch_optional(conn).await?;
    Ok(row.map(|r| (r.hash, r.duration_ms)))
}
