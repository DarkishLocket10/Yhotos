//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use uuid::Uuid;
use yhotos_core::{AlbumId, GroupId, PhotoId, UserId};

/// Closed values accepted by the database enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "principal_kind", rename_all = "lowercase")]
pub enum PrincipalKind {
    /// User value.
    User,
    /// Group value.
    Group,
}

/// An explicit user or group grant convertible to the pure core access type.
#[derive(Clone, Debug)]
pub struct GrantRow {
    /// Stored principal kind.
    pub principal_kind: PrincipalKind,
    /// Stored principal id.
    pub principal_id: Uuid,
}

impl From<GrantRow> for yhotos_core::access::Grant {
    fn from(row: GrantRow) -> Self {
        match row.principal_kind {
            PrincipalKind::User => Self::User(UserId(row.principal_id)),
            PrincipalKind::Group => Self::Group(GroupId(row.principal_id)),
        }
    }
}
fn principal(grant: yhotos_core::access::Grant) -> (PrincipalKind, Uuid) {
    match grant {
        yhotos_core::access::Grant::User(id) => (PrincipalKind::User, id.0),
        yhotos_core::access::Grant::Group(id) => (PrincipalKind::Group, id.0),
    }
}

/// Adds an explicit user or group album grant idempotently.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn grant_album(
    conn: &mut PgConnection,
    id: AlbumId,
    grant: yhotos_core::access::Grant,
) -> Result<u64, DbError> {
    let (kind, principal_id) = principal(grant);
    Ok(sqlx::query!(r#"INSERT INTO album_grants(id,album_id,principal_kind,principal_id) VALUES ($1,$2,$3,$4) ON CONFLICT (album_id,principal_kind,principal_id) DO NOTHING"#, Uuid::now_v7(), id as _, kind as _, principal_id).execute(conn).await?.rows_affected())
}

/// Removes exactly the specified principal grant from an album.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn revoke_album(
    conn: &mut PgConnection,
    id: AlbumId,
    grant: yhotos_core::access::Grant,
) -> Result<u64, DbError> {
    let (kind, principal_id) = principal(grant);
    Ok(sqlx::query!(
        r#"DELETE FROM album_grants WHERE album_id=$1 AND principal_kind=$2 AND principal_id=$3"#,
        id as _,
        kind as _,
        principal_id
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Fetches explicit grants on this album; ancestor resolution belongs to core access logic.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn grants_for_album(
    conn: &mut PgConnection,
    id: AlbumId,
) -> Result<Vec<GrantRow>, DbError> {
    Ok(sqlx::query_as!(GrantRow,r#"SELECT principal_kind as "principal_kind: _", principal_id FROM album_grants WHERE album_id=$1 ORDER BY principal_kind,principal_id"#, id as _).fetch_all(conn).await?)
}

/// Adds an explicit user or group photo grant idempotently.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn grant_photo(
    conn: &mut PgConnection,
    id: PhotoId,
    grant: yhotos_core::access::Grant,
) -> Result<u64, DbError> {
    let (kind, principal_id) = principal(grant);
    Ok(sqlx::query!(r#"INSERT INTO photo_grants(id,photo_id,principal_kind,principal_id) VALUES ($1,$2,$3,$4) ON CONFLICT (photo_id,principal_kind,principal_id) DO NOTHING"#, Uuid::now_v7(), id as _, kind as _, principal_id).execute(conn).await?.rows_affected())
}

/// Removes exactly the specified principal grant from a photo.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn revoke_photo(
    conn: &mut PgConnection,
    id: PhotoId,
    grant: yhotos_core::access::Grant,
) -> Result<u64, DbError> {
    let (kind, principal_id) = principal(grant);
    Ok(sqlx::query!(
        r#"DELETE FROM photo_grants WHERE photo_id=$1 AND principal_kind=$2 AND principal_id=$3"#,
        id as _,
        kind as _,
        principal_id
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Fetches explicit photo grants without making an authorization decision.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn grants_for_photo(
    conn: &mut PgConnection,
    id: PhotoId,
) -> Result<Vec<GrantRow>, DbError> {
    Ok(sqlx::query_as!(GrantRow,r#"SELECT principal_kind as "principal_kind: _", principal_id FROM photo_grants WHERE photo_id=$1 ORDER BY principal_kind,principal_id"#, id as _).fetch_all(conn).await?)
}
