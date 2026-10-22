//! Compile-time SQL counterpart of `yhotos_core::access`.
//!
//! The macro concatenates SQL literals during macro expansion, never at runtime.
//! All principal values remain bound parameters; the same CTE serves every page.

use uuid::Uuid;
use yhotos_core::access::{Principal, ShareTarget};

/// Owned, verified principal bindings for the five fixed CTE parameters.
pub struct Bindings {
    /// Authenticated user, or no user for anonymous and share principals.
    pub user: Option<Uuid>,
    /// Verified memberships, never values supplied in a query string.
    pub groups: Vec<Uuid>,
    /// Administrative bypass obtained from trusted identity loading.
    pub admin: bool,
    /// Validated album capability, when present.
    pub album: Option<Uuid>,
    /// Validated single-photo capability, when present.
    pub photo: Option<Uuid>,
}

/// Converts the pure access principal to SQL parameters without changing its scope.
#[must_use]
pub fn visible_photos(principal: &Principal<'_>) -> Bindings {
    let mut bindings = Bindings {
        user: None,
        groups: Vec::new(),
        admin: false,
        album: None,
        photo: None,
    };
    match principal {
        Principal::Anonymous => {}
        Principal::User {
            id,
            groups,
            is_admin,
        } => {
            bindings.user = Some(id.0);
            bindings.groups = groups.iter().map(|id| id.0).collect();
            bindings.admin = *is_admin;
        }
        Principal::ShareLink {
            target: ShareTarget::Album(id),
        } => bindings.album = Some(id.0),
        Principal::ShareLink {
            target: ShareTarget::Photo(id),
        } => bindings.photo = Some(id.0),
    }
    bindings
}

/// Expands a checked static query with visibility applied before LIMIT or windows.
/// Parameters six onwards belong to the supplied literal SELECT.
#[macro_export]
macro_rules! visible_page_query {
    ($tail:literal, $p:expr $(, $arg:expr)* $(,)?) => {
        sqlx::query!(
            r#"WITH RECURSIVE identity AS (
              SELECT $1::uuid AS user_id, $2::uuid[] AS groups, $3::bool AS admin,
                     $4::uuid AS shared_album, $5::uuid AS shared_photo
            ), album_levels AS (
              SELECT a.*, ARRAY[a.id] AS ids, a.slug::text AS path,
                jsonb_build_array(jsonb_build_object('id',a.id,'title',a.title,'path',a.slug::text)) AS chain,
                (a.visibility <> 'restricted' OR a.id=i.shared_album OR EXISTS (
                  SELECT 1 FROM album_grants g WHERE g.album_id=a.id AND
                   ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                    (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) IS TRUE AS levels_pass
              FROM albums a CROSS JOIN identity i WHERE a.parent_id IS NULL AND a.deleted_at IS NULL
              UNION ALL
              SELECT a.*, parent.ids || a.id, parent.path || '/' || a.slug::text,
                parent.chain || jsonb_build_array(jsonb_build_object('id',a.id,'title',a.title,'path',parent.path || '/' || a.slug::text)),
                parent.levels_pass AND (a.visibility <> 'restricted' OR a.id=i.shared_album OR EXISTS (
                  SELECT 1 FROM album_grants g WHERE g.album_id=a.id AND
                   ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                    (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) IS TRUE
              FROM albums a JOIN album_levels parent ON parent.id=a.parent_id CROSS JOIN identity i
              WHERE a.deleted_at IS NULL AND NOT a.id=ANY(parent.ids)
            ), visible_albums AS (
              SELECT a.* FROM album_levels a CROSS JOIN identity i
              WHERE i.admin OR (a.levels_pass AND i.shared_photo IS NULL AND
                (i.shared_album IS NULL OR i.shared_album=ANY(a.ids)))
            ), visible_photos AS NOT MATERIALIZED (
              SELECT p.* FROM photos p JOIN album_levels a ON a.id=p.album_id CROSS JOIN identity i
              WHERE p.deleted_at IS NULL AND (i.admin OR (a.levels_pass AND (
                (i.shared_photo IS NOT NULL AND p.id=i.shared_photo) OR
                (i.shared_photo IS NULL AND (i.shared_album IS NULL OR i.shared_album=ANY(a.ids)) AND
                  (p.visibility <> 'restricted' OR EXISTS (
                    SELECT 1 FROM photo_grants g WHERE g.photo_id=p.id AND
                     ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                      (g.principal_kind='group' AND g.principal_id=ANY(i.groups)))))))))
            ) "# + $tail,
            $p.user, &$p.groups, $p.admin, $p.album, $p.photo $(, $arg)*
        )
    };
}
