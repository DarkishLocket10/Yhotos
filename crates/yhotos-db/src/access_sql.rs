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
    pub album: Vec<Uuid>,
    /// Validated single-photo capability, when present.
    pub photo: Vec<Uuid>,
}

impl Bindings {
    /// Adds independently scoped link principals to the account's visibility union.
    /// Anonymous access is suppressed while link scopes are present, matching files.
    #[must_use]
    pub fn with_links(mut self, targets: impl IntoIterator<Item = ShareTarget>) -> Self {
        for target in targets {
            match target {
                ShareTarget::Album(id) => self.album.push(id.0),
                ShareTarget::Photo(id) => self.photo.push(id.0),
            }
        }
        self
    }
}

/// Converts the pure access principal to SQL parameters without changing its scope.
#[must_use]
pub fn visible_photos(principal: &Principal<'_>) -> Bindings {
    let mut bindings = Bindings {
        user: None,
        groups: Vec::new(),
        admin: false,
        album: Vec::new(),
        photo: Vec::new(),
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
        } => bindings.album.push(id.0),
        Principal::ShareLink {
            target: ShareTarget::Photo(id),
        } => bindings.photo.push(id.0),
    }
    bindings
}

/// Expands a checked static query with visibility applied before LIMIT or windows.
/// Parameters six onwards belong to the supplied literal SELECT.
#[macro_export]
macro_rules! visible_page_query {
    ($tail:literal, $p:expr $(, $arg:expr)* $(,)?) => {
        $crate::visible_page_query!(@with r#"WITH RECURSIVE identity AS (
              SELECT $1::uuid AS user_id, $2::uuid[] AS groups, $3::bool AS admin,
                     $4::uuid[] AS shared_albums, $5::uuid[] AS shared_photos
            )"#; [$tail];
            [$p.user, &$p.groups, $p.admin, &$p.album, &$p.photo $(, $arg)*])
    };
    (@with $identity:literal; [$($tail:literal),+]; [$($arg:expr),*]) => {
        sqlx::query!($identity + r#", album_levels AS (
              SELECT a.*, ARRAY[a.id] AS ids, a.slug::text AS path,
                jsonb_build_array(jsonb_build_object('id',a.id,'title',a.title,'path',a.slug::text)) AS chain,
                CASE WHEN a.visibility='restricted' THEN ARRAY[a.id] ELSE ARRAY[]::uuid[] END AS restricted_ids,
                (a.visibility <> 'restricted' OR EXISTS (
                  SELECT 1 FROM album_grants g WHERE g.album_id=a.id AND
                   ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                    (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) IS TRUE AS levels_pass
              FROM albums a CROSS JOIN identity i WHERE a.parent_id IS NULL AND a.deleted_at IS NULL
              UNION ALL
              SELECT a.*, parent.ids || a.id, parent.path || '/' || a.slug::text,
                parent.chain || jsonb_build_array(jsonb_build_object('id',a.id,'title',a.title,'path',parent.path || '/' || a.slug::text)),
                parent.restricted_ids || CASE WHEN a.visibility='restricted' THEN ARRAY[a.id] ELSE ARRAY[]::uuid[] END,
                parent.levels_pass AND (a.visibility <> 'restricted' OR EXISTS (
                  SELECT 1 FROM album_grants g WHERE g.album_id=a.id AND
                   ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                    (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))) IS TRUE
              FROM albums a JOIN album_levels parent ON parent.id=a.parent_id CROSS JOIN identity i
              WHERE a.deleted_at IS NULL AND NOT a.id=ANY(parent.ids)
            ), visible_albums AS (
              SELECT a.* FROM album_levels a CROSS JOIN identity i
              WHERE i.admin OR (
                ((i.user_id IS NOT NULL OR (cardinality(i.shared_albums)=0 AND cardinality(i.shared_photos)=0)) AND a.levels_pass)
                OR EXISTS (SELECT FROM unnest(i.shared_albums) target
                  WHERE target=ANY(a.ids) AND a.restricted_ids <@ ARRAY[target]))
            ), visible_photos AS NOT MATERIALIZED (
              SELECT p.*, coalesce((SELECT jsonb_agg(jsonb_build_object('codec',v.codec,'hash',v.hash,'duration_ms',v.duration_ms) ORDER BY v.codec) FROM video_renditions v WHERE v.photo_id=p.id AND NOT v.hls),'[]'::jsonb) AS video, ARRAY(SELECT target FROM unnest(i.shared_albums) target
                WHERE p.visibility <> 'restricted' AND target=ANY(a.ids) AND a.restricted_ids <@ ARRAY[target]) AS shared_albums,
                (i.admin OR (i.user_id IS NOT NULL AND a.levels_pass AND
                (p.visibility <> 'restricted' OR EXISTS (SELECT FROM photo_grants g WHERE g.photo_id=p.id AND
                  ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                   (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))))) AS account_view,
                (p.id=ANY(i.shared_photos) AND cardinality(a.restricted_ids)=0) AS photo_link_view
              FROM photos p JOIN album_levels a ON a.id=p.album_id CROSS JOIN identity i
              WHERE p.deleted_at IS NULL AND (i.admin OR (
                (a.levels_pass AND (i.user_id IS NOT NULL OR (cardinality(i.shared_albums)=0 AND cardinality(i.shared_photos)=0)) AND
                  (p.visibility <> 'restricted' OR EXISTS (
                    SELECT 1 FROM photo_grants g WHERE g.photo_id=p.id AND
                     ((g.principal_kind='user' AND g.principal_id=i.user_id) OR
                      (g.principal_kind='group' AND g.principal_id=ANY(i.groups))))))
                OR (p.id=ANY(i.shared_photos) AND cardinality(a.restricted_ids)=0)
                OR (p.visibility <> 'restricted' AND EXISTS (SELECT FROM unnest(i.shared_albums) target
                  WHERE target=ANY(a.ids) AND a.restricted_ids <@ ARRAY[target]))))
            ) "# $(+ $tail)*, $($arg),*)
    };
}

/// Shares the album/photo page SQL between ordinary routes and token landings.
/// Parameters 6..9 are offset, sort, album path and photo short id. Null lookup
/// keys select the verified share target; callers append a checked result SELECT.
#[macro_export]
macro_rules! gallery_page_query {
    ($identity:literal; $result:literal; [$($arg:expr),*]) => {
        $crate::visible_page_query!(@with $identity; [r#",
      selected AS (SELECT a.* FROM visible_albums a CROSS JOIN identity i
        WHERE a.path=$8::text OR ($8::text IS NULL AND a.id=ANY(i.shared_albums))),
      summaries AS (
        SELECT a.id,a.title,a.description,a.path,a.sort_order,
          (SELECT count(*) FROM visible_photos p WHERE p.album_id=a.id) AS count,
          (SELECT to_jsonb(p) || jsonb_build_object('thumbhash',encode(p.placeholder,'base64'),
            'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
              'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
              ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb))
           FROM visible_photos p WHERE p.album_id=a.id OR p.id=a.cover_photo_id
           ORDER BY (p.id=a.cover_photo_id) DESC NULLS LAST,p.sort_order,p.id LIMIT 1) AS cover
        FROM visible_albums a WHERE a.id=(SELECT id FROM selected) OR a.parent_id=(SELECT id FROM selected)
      ), ordered AS (
        SELECT p.*,row_number() OVER (ORDER BY
          CASE WHEN $7='taken' THEN coalesce(taken_at,posted_at) END DESC,
          CASE WHEN $7='posted' THEN posted_at END DESC,
          CASE WHEN $7='name' THEN title END COLLATE "C" ASC,
          CASE WHEN $7='manual' THEN sort_order END ASC, id ASC) AS position
        FROM visible_photos p WHERE p.album_id=(SELECT id FROM selected)
      ), slice AS (
        SELECT * FROM ordered ORDER BY position LIMIT 60 OFFSET $6
      ), album_payload AS (SELECT jsonb_build_object(
        'album',(SELECT to_jsonb(s) FROM summaries s WHERE s.id=a.id), 'chain',a.chain,
        'children',coalesce((SELECT jsonb_agg(to_jsonb(s) ORDER BY s.sort_order,s.id) FROM summaries s WHERE s.id<>a.id),'[]'::jsonb),
        'total',(SELECT count(*) FROM ordered),
        'photos',coalesce((SELECT jsonb_agg(to_jsonb(p) || jsonb_build_object('thumbhash',encode(p.placeholder,'base64'),
          'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
            'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
            ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb)) ORDER BY p.position)
          FROM slice p),'[]'::jsonb)) AS payload FROM selected a
      WHERE $6=0 OR EXISTS (SELECT 1 FROM slice)
     ),
      photo_ordered AS (
        SELECT p.*, lag(short_id) OVER ordering AS previous, lead(short_id) OVER ordering AS next
        FROM visible_photos p
        WHERE p.album_id=(SELECT album_id FROM visible_photos WHERE short_id=$9::text OR ($9::text IS NULL AND id=ANY((SELECT shared_photos FROM identity)::uuid[])))
        WINDOW ordering AS (ORDER BY
          CASE WHEN $7='taken' THEN coalesce(taken_at,posted_at) END DESC,
          CASE WHEN $7='posted' THEN posted_at END DESC,
          CASE WHEN $7='name' THEN title END COLLATE "C" ASC,
          CASE WHEN $7='manual' THEN sort_order END ASC, id ASC)
      ), photo_payload AS (SELECT jsonb_build_object(
        'photo', to_jsonb(p) || jsonb_build_object('thumbhash',encode(p.placeholder,'base64'),
          'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
            'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
            ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb)),
        'chain', a.chain, 'previous',p.previous,'next',p.next) AS payload
      FROM photo_ordered p JOIN album_levels a ON a.id=p.album_id WHERE p.short_id=$9::text OR ($9::text IS NULL AND p.id=ANY((SELECT shared_photos FROM identity)::uuid[]))
     )"#, $result]; [$($arg),*])
    };
    ($result:literal, $p:expr $(, $arg:expr)* $(,)?) => {
        $crate::gallery_page_query!(r#"WITH RECURSIVE identity AS (
              SELECT $1::uuid AS user_id, $2::uuid[] AS groups, $3::bool AS admin,
                     $4::uuid[] AS shared_albums, $5::uuid[] AS shared_photos
            )"#; $result;
            [$p.user, &$p.groups, $p.admin, &$p.album, &$p.photo $(, $arg)*])
    };
}
