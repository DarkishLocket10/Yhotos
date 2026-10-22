//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use crate::likes::Actor;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{PhotoId, ShareLinkId, UserId};

/// Closed values accepted by the database enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "flag_resolution", rename_all = "lowercase")]
pub enum FlagResolution {
    /// Replaced value.
    Replaced,
    /// Reedited value.
    Reedited,
    /// Removed value.
    Removed,
    /// Visibility value.
    Visibility,
    /// Dismissed value.
    Dismissed,
}

/// One exactly-attributed flag with a paired optional resolution and UTC timestamp.
#[derive(Clone, Debug)]
pub struct FlagRow {
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
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored resolved at.
    pub resolved_at: Option<OffsetDateTime>,
    /// Stored resolution.
    pub resolution: Option<FlagResolution>,
}

/// Records a flag attributed to exactly one verified account or share link.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn flag(
    conn: &mut PgConnection,
    photo: PhotoId,
    actor: Actor,
) -> Result<FlagRow, DbError> {
    let (user, link) = actor.columns();
    Ok(sqlx::query_as!(FlagRow,r#"INSERT INTO flags(id,photo_id,user_id,share_link_id) VALUES ($1,$2,$3,$4) RETURNING id, created_at, user_id as "user_id: _", share_link_id as "share_link_id: _", photo_id as "photo_id: _", updated_at, resolved_at, resolution as "resolution: _""#, Uuid::now_v7(), photo as _, user as _, link as _).fetch_one(conn).await?)
}

/// Returns a stable zero-based page of up to fifty unresolved flags.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn open_flags(conn: &mut PgConnection, page: u32) -> Result<Vec<FlagRow>, DbError> {
    Ok(sqlx::query_as!(FlagRow,r#"SELECT id, created_at, user_id as "user_id: _", share_link_id as "share_link_id: _", photo_id as "photo_id: _", updated_at, resolved_at, resolution as "resolution: _" FROM flags WHERE resolved_at IS NULL ORDER BY created_at,id LIMIT 50 OFFSET $1"#, i64::from(page)*50).fetch_all(conn).await?)
}

/// Records a resolution and UTC timestamp together, preserving flag attribution.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn resolve_flag(
    conn: &mut PgConnection,
    id: Uuid,
    resolution: FlagResolution,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"UPDATE flags SET resolution=$2,resolved_at=now() WHERE id=$1"#,
        id,
        resolution as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}
