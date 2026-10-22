//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use crate::share_links::TargetKind;
use sqlx::PgConnection;
use uuid::Uuid;
use yhotos_core::{AlbumId, PhotoId};

/// A stored internal target convertible to a typed core target.
#[derive(Clone, Debug)]
pub struct RedirectTarget {
    /// Stored target kind.
    pub target_kind: TargetKind,
    /// Stored target id.
    pub target_id: Uuid,
}

/// Creates or replaces an exact internal legacy path mapping to a typed target.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn insert(
    conn: &mut PgConnection,
    path: &str,
    target: yhotos_core::access::ShareTarget,
) -> Result<u64, DbError> {
    let (kind, id) = match target {
        yhotos_core::access::ShareTarget::Album(id) => (TargetKind::Album, id.0),
        yhotos_core::access::ShareTarget::Photo(id) => (TargetKind::Photo, id.0),
    };
    Ok(sqlx::query!(r#"INSERT INTO legacy_redirects(id,pattern,target_kind,target_id) VALUES ($1,$2,$3,$4) ON CONFLICT (pattern) DO UPDATE SET target_kind=excluded.target_kind,target_id=excluded.target_id"#, Uuid::now_v7(), path, kind as _, id).execute(conn).await?.rows_affected())
}

/// Looks up only an exact stored path; it never constructs a destination from request text.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn lookup(
    conn: &mut PgConnection,
    path: &str,
) -> Result<Option<RedirectTarget>, DbError> {
    Ok(sqlx::query_as!(RedirectTarget,r#"SELECT target_kind AS "target_kind: TargetKind",target_id FROM legacy_redirects WHERE pattern=$1"#, path).fetch_optional(conn).await?)
}

impl From<RedirectTarget> for yhotos_core::access::ShareTarget {
    fn from(row: RedirectTarget) -> Self {
        match row.target_kind {
            TargetKind::Album => Self::Album(AlbumId(row.target_id)),
            TargetKind::Photo => Self::Photo(PhotoId(row.target_id)),
        }
    }
}
