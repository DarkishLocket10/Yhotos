//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use yhotos_core::{AlbumId, PhotoId, Slug, access::Visibility};

/// Album state with a canonical slug and typed references; visibility is unresolved.
#[derive(Clone, Debug)]
pub struct AlbumRow {
    /// Stored id.
    pub id: AlbumId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored parent id.
    pub parent_id: Option<AlbumId>,
    /// Stored slug.
    pub slug: Slug,
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored sort order.
    pub sort_order: i32,
    /// Stored cover photo id.
    pub cover_photo_id: Option<PhotoId>,
    /// Stored visibility.
    pub visibility: Visibility,
    /// Stored manual order.
    pub manual_order: bool,
    /// Stored deleted at.
    pub deleted_at: Option<OffsetDateTime>,
}

/// Creates an album with a `UUIDv7` and a slug unique within its parent.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn create(
    conn: &mut PgConnection,
    parent: Option<AlbumId>,
    slug: &Slug,
    title: &str,
    visibility: Visibility,
) -> Result<AlbumRow, DbError> {
    Ok(sqlx::query_as!(AlbumRow,r#"INSERT INTO albums (id,parent_id,slug,title,visibility) VALUES ($1,$2,$3,$4,$5) RETURNING id as "id: _", created_at, updated_at, parent_id as "parent_id: _", slug as "slug: _", title, description, sort_order, cover_photo_id as "cover_photo_id: _", visibility as "visibility: _", manual_order, deleted_at"#, AlbumId::new() as _, parent as _, slug as _, title, visibility as _).fetch_one(conn).await?)
}

/// Fetches an album by its typed id, including trash for caller-side state checks.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_id(conn: &mut PgConnection, id: AlbumId) -> Result<Option<AlbumRow>, DbError> {
    Ok(sqlx::query_as!(AlbumRow,r#"SELECT id as "id: _", created_at, updated_at, parent_id as "parent_id: _", slug as "slug: _", title, description, sort_order, cover_photo_id as "cover_photo_id: _", visibility as "visibility: _", manual_order, deleted_at FROM albums WHERE id=$1"#, id as _).fetch_optional(conn).await?)
}

/// Resolves a complete root-to-leaf slug path in one recursive query.
///
/// # Errors
/// Returns a database failure; an empty path resolves to no album.
pub async fn get_by_path(
    conn: &mut PgConnection,
    path: &[Slug],
) -> Result<Option<AlbumRow>, DbError> {
    let path: Vec<_> = path.iter().map(Slug::as_str).collect();
    Ok(sqlx::query_as!(AlbumRow, r#"WITH RECURSIVE chain AS (
      SELECT a.*, 1 AS depth FROM albums a WHERE parent_id IS NULL AND slug=($1::text[])[1]::citext
      UNION ALL SELECT a.*, c.depth+1 FROM albums a JOIN chain c ON a.parent_id=c.id
      WHERE a.slug=($1::text[])[c.depth+1]::citext AND c.depth < cardinality($1)
    ) SELECT id as "id!: _", created_at AS "created_at!", updated_at AS "updated_at!", parent_id as "parent_id: _", slug as "slug!: _", title AS "title!", description AS "description!", sort_order AS "sort_order!", cover_photo_id as "cover_photo_id: _", visibility as "visibility!: _", manual_order AS "manual_order!", deleted_at FROM chain WHERE depth=cardinality($1)"#, &path as &[&str]).fetch_optional(conn).await?)
}

/// Returns direct children in stable manual order, including trashed albums.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn children(
    conn: &mut PgConnection,
    parent: Option<AlbumId>,
) -> Result<Vec<AlbumRow>, DbError> {
    Ok(sqlx::query_as!(AlbumRow,r#"SELECT id as "id: _", created_at, updated_at, parent_id as "parent_id: _", slug as "slug: _", title, description, sort_order, cover_photo_id as "cover_photo_id: _", visibility as "visibility: _", manual_order, deleted_at FROM albums WHERE parent_id IS NOT DISTINCT FROM $1 ORDER BY sort_order,id"#, parent as _).fetch_all(conn).await?)
}

/// Returns the complete ancestor chain, nearest parent first, terminating even on corrupt cycles.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn ancestors(conn: &mut PgConnection, id: AlbumId) -> Result<Vec<AlbumRow>, DbError> {
    Ok(sqlx::query_as!(AlbumRow,r#"WITH RECURSIVE chain AS (SELECT a.*, 0 AS depth, ARRAY[a.id] AS visited FROM albums a WHERE id=$1 UNION ALL SELECT a.*, c.depth+1, c.visited || a.id FROM albums a JOIN chain c ON c.parent_id=a.id WHERE NOT a.id=ANY(c.visited)) SELECT id as "id!: _", created_at AS "created_at!", updated_at AS "updated_at!", parent_id as "parent_id: _", slug as "slug!: _", title AS "title!", description AS "description!", sort_order AS "sort_order!", cover_photo_id as "cover_photo_id: _", visibility as "visibility!: _", manual_order AS "manual_order!", deleted_at FROM chain WHERE depth>0 ORDER BY depth"#, id as _).fetch_all(conn).await?)
}

/// Replaces editable album metadata while preserving its parent and identity.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn update(
    conn: &mut PgConnection,
    id: AlbumId,
    slug: &Slug,
    title: &str,
    description: &str,
    visibility: Visibility,
    manual_order: bool,
) -> Result<AlbumRow, DbError> {
    Ok(sqlx::query_as!(AlbumRow,r#"UPDATE albums SET slug=$2,title=$3,description=$4,visibility=$5,manual_order=$6 WHERE id=$1 RETURNING id as "id: _", created_at, updated_at, parent_id as "parent_id: _", slug as "slug: _", title, description, sort_order, cover_photo_id as "cover_photo_id: _", visibility as "visibility: _", manual_order, deleted_at"#, id as _, slug as _, title, description, visibility as _, manual_order).fetch_one(conn).await?)
}

/// Assigns zero-based order to the supplied children in one statement.
///
/// # Errors
/// Rejects duplicate ids and orders too large for the stored integer.
pub async fn reorder(
    conn: &mut PgConnection,
    parent: Option<AlbumId>,
    ids: &[AlbumId],
) -> Result<u64, DbError> {
    if ids.len() > i32::MAX as usize
        || ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len()
    {
        return Err(DbError::InvalidInput);
    }
    let ids: Vec<_> = ids.iter().map(|id| id.0).collect();
    Ok(sqlx::query!("UPDATE albums a SET sort_order=(o.position-1)::integer FROM unnest($2::uuid[]) WITH ORDINALITY AS o(id,position) WHERE a.id=o.id AND a.parent_id IS NOT DISTINCT FROM $1", parent as _, &ids).execute(conn).await?.rows_affected())
}

/// Sets or clears the cover reference; the database rejects nonexistent photos.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn set_cover(
    conn: &mut PgConnection,
    id: AlbumId,
    photo: Option<PhotoId>,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"UPDATE albums SET cover_photo_id=$2 WHERE id=$1"#,
        id as _,
        photo as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Moves an album to trash without removing descendants or references.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn soft_delete(conn: &mut PgConnection, id: AlbumId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE albums SET deleted_at=now() WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Clears an album trash timestamp while preserving its identity.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn restore(conn: &mut PgConnection, id: AlbumId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE albums SET deleted_at=NULL WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Returns all albums in one query, including trash, for caller-side tree and access resolution.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn list_tree(conn: &mut PgConnection) -> Result<Vec<AlbumRow>, DbError> {
    Ok(sqlx::query_as!(AlbumRow,r#"SELECT id as "id: _", created_at, updated_at, parent_id as "parent_id: _", slug as "slug: _", title, description, sort_order, cover_photo_id as "cover_photo_id: _", visibility as "visibility: _", manual_order, deleted_at FROM albums ORDER BY parent_id NULLS FIRST,sort_order,id"#).fetch_all(conn).await?)
}

/// A canonical root-to-leaf breadcrumb, validated again at the HTTP boundary.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Breadcrumb {
    /// Album cache identity.
    pub id: AlbumId,
    /// Escaped display title.
    pub title: String,
    /// Slash-separated canonical slugs without the `/a/` prefix.
    pub path: String,
}

/// Visible album summary, including a visible cover and a visibility-filtered count.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AlbumCard {
    /// Album cache identity.
    pub id: AlbumId,
    /// Display title.
    pub title: String,
    /// Plain text description.
    pub description: String,
    /// Full root-to-leaf slug path.
    pub path: String,
    /// Count of visible direct photos.
    pub count: u64,
    /// A visible cover, preferring the explicitly selected photo when available.
    pub cover: Option<crate::photos::PagePhoto>,
}

/// A complete visibility-filtered page, with pagination fixed at sixty photos.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AlbumPage {
    /// Album identity and metadata.
    pub album: AlbumCard,
    /// Root-to-leaf breadcrumb chain.
    pub chain: Vec<Breadcrumb>,
    /// Direct child albums which the principal can view.
    pub children: Vec<AlbumCard>,
    /// At most sixty visible photos, in stable selected order.
    pub photos: Vec<crate::photos::PagePhoto>,
    /// Total visible photos before LIMIT.
    pub total: u64,
}

/// Home summaries and covers in one checked SQL round trip.
///
/// # Errors
/// Returns database errors or malformed stored metadata.
pub async fn home(
    conn: &mut PgConnection,
    principal: &yhotos_core::access::Principal<'_>,
) -> Result<Vec<AlbumCard>, DbError> {
    home_for(conn, &crate::access_sql::visible_photos(principal)).await
}

/// Fetches a visibility-filtered page for the union of independently verified principals.
///
/// # Errors
/// Returns invalid pagination, malformed metadata or a database failure.
pub async fn home_for(
    conn: &mut PgConnection,
    bindings: &crate::access_sql::Bindings,
) -> Result<Vec<AlbumCard>, DbError> {
    let row = crate::visible_page_query!(r#",
      summaries AS (
        SELECT a.id,a.title,a.description,a.path,a.sort_order,
          (SELECT count(*) FROM visible_photos p WHERE p.album_id=a.id) AS count,
          (SELECT to_jsonb(p) || jsonb_build_object('thumbhash',encode(p.placeholder,'base64'),
          'video',CASE WHEN p.kind='video' THEN coalesce((SELECT jsonb_agg(jsonb_build_object('codec',v.codec,'hash',v.hash,'duration_ms',v.duration_ms,'av1_level',v.av1_level) ORDER BY v.codec) FROM video_renditions v WHERE v.photo_id=p.id AND NOT v.hls),'[]'::jsonb) ELSE '[]'::jsonb END,
            'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
             'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
             ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb))
           FROM (SELECT * FROM visible_photos p WHERE p.album_id=a.id OR p.id=a.cover_photo_id
           ORDER BY (p.id=a.cover_photo_id) DESC NULLS LAST,p.sort_order,p.id LIMIT 1) p) AS cover
        FROM visible_albums a WHERE a.parent_id IS NULL
      ) SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY sort_order,id),'[]'::jsonb) AS "payload!" FROM summaries s
    "#, bindings).fetch_one(conn).await?;
    serde_json::from_value(row.payload).map_err(|_| DbError::InvalidInput)
}

/// Resolves album, breadcrumbs, children, count and sixty-photo page in one round trip.
/// SQL filters before LIMIT, so hidden photos never affect page boundaries.
///
/// # Errors
/// Rejects page zero/overflow and returns database errors or malformed metadata.
pub async fn page(
    conn: &mut PgConnection,
    principal: &yhotos_core::access::Principal<'_>,
    path: &[Slug],
    page: u32,
    sort: crate::photos::PageSort,
) -> Result<Option<AlbumPage>, DbError> {
    page_for(
        conn,
        &crate::access_sql::visible_photos(principal),
        path,
        page,
        sort,
    )
    .await
}

/// Fetches a visibility-filtered page for the union of independently verified principals.
///
/// # Errors
/// Returns invalid pagination, malformed metadata or a database failure.
pub async fn page_for(
    conn: &mut PgConnection,
    bindings: &crate::access_sql::Bindings,
    path: &[Slug],
    page: u32,
    sort: crate::photos::PageSort,
) -> Result<Option<AlbumPage>, DbError> {
    let offset = page.checked_sub(1).ok_or(DbError::InvalidInput)?;
    let offset = i64::from(offset) * 60;
    let path = path.iter().map(Slug::as_str).collect::<Vec<_>>().join("/");
    let row = crate::gallery_page_query!(
        r#"SELECT payload AS "payload!" FROM album_payload"#,
        bindings,
        offset,
        sort.as_str(),
        Some(path),
        None::<String>
    )
    .fetch_optional(conn)
    .await?;
    row.map(|r| serde_json::from_value(r.payload).map_err(|_| DbError::InvalidInput))
        .transpose()
}
