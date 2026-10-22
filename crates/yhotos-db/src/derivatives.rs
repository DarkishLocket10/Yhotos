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
