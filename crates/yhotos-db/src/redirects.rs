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

/// Constructs a relative destination only when the principal can view the stored target.
/// Missing, deleted and restricted targets (including ancestors) resolve to no destination.
///
/// # Errors
/// Returns database failures; invalid stored slugs or short ids fail closed.
pub async fn destination(
    conn: &mut PgConnection,
    principal: &yhotos_core::access::Principal<'_>,
    patterns: &[String],
) -> Result<Option<String>, DbError> {
    let bindings = crate::access_sql::visible_photos(principal);
    let row = crate::visible_page_query!(
        r#", target AS (
      SELECT target_kind,target_id FROM legacy_redirects WHERE pattern=ANY($6)
      ORDER BY array_position($6,pattern) LIMIT 1
    ) SELECT CASE WHEN t.target_kind='photo' THEN
      (SELECT '/p/' || p.short_id FROM visible_photos p WHERE p.id=t.target_id)
      ELSE (SELECT '/a/' || a.path FROM visible_albums a WHERE a.id=t.target_id)
      END AS destination FROM target t"#,
        bindings,
        patterns
    )
    .fetch_optional(conn)
    .await?;
    let target = row.and_then(|row| row.destination);
    Ok(target.filter(|path| valid_destination(path)))
}

/// Accepts only canonical album and photo paths, excluding schemes, slashes and escapes.
#[must_use]
pub fn valid_destination(path: &str) -> bool {
    if let Some(id) = path.strip_prefix("/p/") {
        return id.parse::<yhotos_core::ShortId>().is_ok();
    }
    path.strip_prefix("/a/").is_some_and(|path| {
        !path.is_empty()
            && path
                .split('/')
                .all(|part| yhotos_core::Slug::new(part).is_ok())
    })
}
