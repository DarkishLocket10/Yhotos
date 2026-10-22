//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{PhotoId, ShareLinkId, UserId};

/// One photo like attributed to exactly one account or share link.
#[derive(Clone, Debug)]
pub struct LikeRow {
    /// Stored id.
    pub id: Uuid,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored user id.
    pub user_id: Option<UserId>,
    /// Stored share link id.
    pub share_link_id: Option<ShareLinkId>,
    /// Stored photo id.
    pub photo_id: PhotoId,
}

/// Exactly one authenticated actor; the caller verifies identity and capability.
#[derive(Clone, Copy, Debug)]
pub enum Actor {
    /// A verified account.
    User(UserId),
    /// A verified link with the requested capability.
    ShareLink(ShareLinkId),
}
impl Actor {
    pub(crate) fn columns(self) -> (Option<UserId>, Option<ShareLinkId>) {
        match self {
            Self::User(id) => (Some(id), None),
            Self::ShareLink(id) => (None, Some(id)),
        }
    }
}

/// Adds one actor like idempotently; the caller must verify the actor and capability.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn like(conn: &mut PgConnection, photo: PhotoId, actor: Actor) -> Result<u64, DbError> {
    let (user, link) = actor.columns();
    Ok(sqlx::query!(r#"INSERT INTO likes(id,photo_id,user_id,share_link_id) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING"#, Uuid::now_v7(), photo as _, user as _, link as _).execute(conn).await?.rows_affected())
}

/// Deletes only the supplied actor like on this photo.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn unlike(conn: &mut PgConnection, photo: PhotoId, actor: Actor) -> Result<u64, DbError> {
    let (user, link) = actor.columns();
    Ok(sqlx::query!(
        r#"DELETE FROM likes WHERE photo_id=$1 AND (user_id=$2 OR share_link_id=$3)"#,
        photo as _,
        user as _,
        link as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Returns a zero-based page of up to fifty liked non-trashed photos for caller-side access
/// checking.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn liked_by_user(
    conn: &mut PgConnection,
    user: UserId,
    page: u32,
) -> Result<Vec<crate::photos::PhotoRow>, DbError> {
    Ok(sqlx::query_as!(crate::photos::PhotoRow,r#"SELECT p.id as "id: _", p.created_at, p.updated_at, p.short_id as "short_id: _", p.album_id as "album_id: _", p.title, p.description, p.taken_at, p.posted_at, p.sort_order, p.visibility as "visibility: _", p.original_hash, p.original_name, p.original_bytes, p.mime, p.kind as "kind: _", p.width, p.height, p.orientation, p.is_hdr, p.has_gain_map, p.icc_name, p.camera_make, p.camera_model, p.lens, p.focal_mm, p.aperture, p.shutter, p.iso, p.gps_lat, p.gps_lon, p.place_id, p.dominant_colors, p.placeholder, p.deleted_at FROM photos p JOIN likes l ON l.photo_id=p.id WHERE l.user_id=$1 AND p.deleted_at IS NULL ORDER BY l.created_at DESC,l.id LIMIT 50 OFFSET $2"#, user as _, i64::from(page)*50).fetch_all(conn).await?)
}

/// Fetches every like with its exactly-one actor attribution in stable creation order.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn likes_of_photo(
    conn: &mut PgConnection,
    photo: PhotoId,
) -> Result<Vec<LikeRow>, DbError> {
    Ok(sqlx::query_as!(LikeRow,r#"SELECT id, created_at, user_id as "user_id: _", share_link_id as "share_link_id: _", photo_id as "photo_id: _" FROM likes WHERE photo_id=$1 ORDER BY created_at,id"#, photo as _).fetch_all(conn).await?)
}
