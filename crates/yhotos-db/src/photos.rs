//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{AlbumId, PhotoId, ShortId, access::Visibility};

/// Closed values accepted by the database enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "media_kind", rename_all = "lowercase")]
pub enum MediaKind {
    /// Image value.
    Image,
    /// Raw value.
    Raw,
    /// Video value.
    Video,
}

/// Original and extracted metadata with a canonical short id; callers must check access.
#[derive(Clone, Debug)]
pub struct PhotoRow {
    /// Stored id.
    pub id: PhotoId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored short id.
    pub short_id: ShortId,
    /// Stored album id.
    pub album_id: AlbumId,
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored taken at.
    pub taken_at: Option<OffsetDateTime>,
    /// Stored posted at.
    pub posted_at: OffsetDateTime,
    /// Stored sort order.
    pub sort_order: i32,
    /// Stored visibility.
    pub visibility: Visibility,
    /// Stored original hash.
    pub original_hash: String,
    /// Stored original name.
    pub original_name: String,
    /// Stored original bytes.
    pub original_bytes: i64,
    /// Stored mime.
    pub mime: String,
    /// Stored kind.
    pub kind: MediaKind,
    /// Stored width.
    pub width: Option<i32>,
    /// Stored height.
    pub height: Option<i32>,
    /// Stored orientation.
    pub orientation: Option<i32>,
    /// Stored is hdr.
    pub is_hdr: bool,
    /// Stored has gain map.
    pub has_gain_map: bool,
    /// Stored icc name.
    pub icc_name: Option<String>,
    /// Stored camera make.
    pub camera_make: Option<String>,
    /// Stored camera model.
    pub camera_model: Option<String>,
    /// Stored lens.
    pub lens: Option<String>,
    /// Stored focal mm.
    pub focal_mm: Option<f64>,
    /// Stored aperture.
    pub aperture: Option<f64>,
    /// Stored shutter.
    pub shutter: Option<String>,
    /// Stored iso.
    pub iso: Option<i32>,
    /// Stored gps lat.
    pub gps_lat: Option<f64>,
    /// Stored gps lon.
    pub gps_lon: Option<f64>,
    /// Stored place id.
    pub place_id: Option<Uuid>,
    /// Stored dominant colors.
    pub dominant_colors: Vec<i32>,
    /// Stored placeholder.
    pub placeholder: Option<Vec<u8>>,
    /// Stored deleted at.
    pub deleted_at: Option<OffsetDateTime>,
}

/// A photo plus its derivative descriptors from one SELECT, before access checking.
#[derive(Clone, Debug)]
pub struct AlbumPhotoRow {
    /// Stored id.
    pub id: PhotoId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored short id.
    pub short_id: ShortId,
    /// Stored album id.
    pub album_id: AlbumId,
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored taken at.
    pub taken_at: Option<OffsetDateTime>,
    /// Stored posted at.
    pub posted_at: OffsetDateTime,
    /// Stored sort order.
    pub sort_order: i32,
    /// Stored visibility.
    pub visibility: Visibility,
    /// Stored original hash.
    pub original_hash: String,
    /// Stored original name.
    pub original_name: String,
    /// Stored original bytes.
    pub original_bytes: i64,
    /// Stored mime.
    pub mime: String,
    /// Stored kind.
    pub kind: MediaKind,
    /// Stored width.
    pub width: Option<i32>,
    /// Stored height.
    pub height: Option<i32>,
    /// Stored orientation.
    pub orientation: Option<i32>,
    /// Stored is hdr.
    pub is_hdr: bool,
    /// Stored has gain map.
    pub has_gain_map: bool,
    /// Stored icc name.
    pub icc_name: Option<String>,
    /// Stored camera make.
    pub camera_make: Option<String>,
    /// Stored camera model.
    pub camera_model: Option<String>,
    /// Stored lens.
    pub lens: Option<String>,
    /// Stored focal mm.
    pub focal_mm: Option<f64>,
    /// Stored aperture.
    pub aperture: Option<f64>,
    /// Stored shutter.
    pub shutter: Option<String>,
    /// Stored iso.
    pub iso: Option<i32>,
    /// Stored gps lat.
    pub gps_lat: Option<f64>,
    /// Stored gps lon.
    pub gps_lon: Option<f64>,
    /// Stored place id.
    pub place_id: Option<Uuid>,
    /// Stored dominant colors.
    pub dominant_colors: Vec<i32>,
    /// Stored placeholder.
    pub placeholder: Option<Vec<u8>>,
    /// Stored deleted at.
    pub deleted_at: Option<OffsetDateTime>,
    /// Stored derivatives.
    pub derivatives: serde_json::Value,
}

/// Supported album orderings; identifiers break ties for stable pagination.
#[derive(Clone, Copy, Debug)]
pub enum PhotoOrder {
    /// Album manual order, ascending.
    Manual,
    /// Capture date, newest first, unknown dates last.
    Taken,
    /// Posted date, newest first.
    Posted,
    /// Title, ascending.
    Name,
}

/// Required original fields; create generates the `UUIDv7` and canonical short lookup key.
#[derive(Clone, Debug)]
pub struct NewPhoto {
    /// Stored album id.
    pub album_id: AlbumId,
    /// Stored title.
    pub title: String,
    /// Stored original hash.
    pub original_hash: String,
    /// Stored original name.
    pub original_name: String,
    /// Stored original bytes.
    pub original_bytes: i64,
    /// Stored mime.
    pub mime: String,
    /// Stored kind.
    pub kind: MediaKind,
}

/// Complete replacement metadata; absent optional values clear stored fields.
#[derive(Clone, Debug, Default)]
pub struct PhotoMetadata {
    /// Stored title.
    pub title: String,
    /// Stored description.
    pub description: String,
    /// Stored taken at.
    pub taken_at: Option<OffsetDateTime>,
    /// Stored width.
    pub width: Option<i32>,
    /// Stored height.
    pub height: Option<i32>,
    /// Stored orientation.
    pub orientation: Option<i32>,
    /// Stored is hdr.
    pub is_hdr: bool,
    /// Stored has gain map.
    pub has_gain_map: bool,
    /// Stored icc name.
    pub icc_name: Option<String>,
    /// Stored camera make.
    pub camera_make: Option<String>,
    /// Stored camera model.
    pub camera_model: Option<String>,
    /// Stored lens.
    pub lens: Option<String>,
    /// Stored focal mm.
    pub focal_mm: Option<f64>,
    /// Stored aperture.
    pub aperture: Option<f64>,
    /// Stored shutter.
    pub shutter: Option<String>,
    /// Stored iso.
    pub iso: Option<i32>,
    /// Stored gps lat.
    pub gps_lat: Option<f64>,
    /// Stored gps lon.
    pub gps_lon: Option<f64>,
    /// Stored place id.
    pub place_id: Option<Uuid>,
    /// Stored dominant colors.
    pub dominant_colors: Vec<i32>,
    /// Stored placeholder.
    pub placeholder: Option<Vec<u8>>,
}

/// Inserts an original with an app-generated `UUIDv7` and stable short lookup key.
///
/// # Errors
/// Returns invalid hash/size constraints or another database failure.
pub async fn create(conn: &mut PgConnection, photo: &NewPhoto) -> Result<PhotoRow, DbError> {
    let id = PhotoId::new();
    let short = ShortId::from_photo_id(id);
    Ok(sqlx::query_as!(PhotoRow, r#"INSERT INTO photos (id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind)
      VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id as "id: _", created_at, updated_at, short_id as "short_id: _", album_id as "album_id: _", title, description, taken_at, posted_at, sort_order, visibility as "visibility: _", original_hash, original_name, original_bytes, mime, kind as "kind: _", width, height, orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens, focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id, dominant_colors, placeholder, deleted_at"#, id as _, short as _, photo.album_id as _, photo.title, photo.original_hash, photo.original_name, photo.original_bytes, photo.mime, photo.kind as _).fetch_one(conn).await?)
}

/// Get by short id with bound parameters and no visibility policy.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_short_id(
    conn: &mut PgConnection,
    short: &ShortId,
) -> Result<Option<PhotoRow>, DbError> {
    Ok(sqlx::query_as!(PhotoRow,r#"SELECT id as "id: _", created_at, updated_at, short_id as "short_id: _", album_id as "album_id: _", title, description, taken_at, posted_at, sort_order, visibility as "visibility: _", original_hash, original_name, original_bytes, mime, kind as "kind: _", width, height, orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens, focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id, dominant_colors, placeholder, deleted_at FROM photos WHERE short_id=$1"#, short as _).fetch_optional(conn).await?)
}

/// Returns every owner of a content hash; the caller must authorize each photo.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_hash(conn: &mut PgConnection, hash: &str) -> Result<Vec<PhotoRow>, DbError> {
    Ok(sqlx::query_as!(PhotoRow,r#"SELECT id as "id: _", created_at, updated_at, short_id as "short_id: _", album_id as "album_id: _", title, description, taken_at, posted_at, sort_order, visibility as "visibility: _", original_hash, original_name, original_bytes, mime, kind as "kind: _", width, height, orientation, is_hdr, has_gain_map, icc_name, camera_make, camera_model, lens, focal_mm, aperture, shutter, iso, gps_lat, gps_lon, place_id, dominant_colors, placeholder, deleted_at FROM photos WHERE original_hash=$1 ORDER BY id"#, hash).fetch_all(conn).await?)
}

/// Returns a bounded photo page and derivative descriptors with one round trip.
/// Visibility is passed through for the caller's access decisions.
///
/// # Errors
/// Rejects page sizes outside 1..=200 and returns database failures.
pub async fn list_in_album(
    conn: &mut PgConnection,
    album: AlbumId,
    order: PhotoOrder,
    page: u32,
    per_page: u32,
) -> Result<Vec<AlbumPhotoRow>, DbError> {
    let (limit, offset) = crate::page(page, per_page)?;
    let order = match order {
        PhotoOrder::Manual => "manual",
        PhotoOrder::Taken => "taken",
        PhotoOrder::Posted => "posted",
        PhotoOrder::Name => "name",
    };
    Ok(sqlx::query_file_as!(
        AlbumPhotoRow,
        "queries/list_in_album.sql",
        album as _,
        order,
        limit,
        offset
    )
    .fetch_all(conn)
    .await?)
}

/// Replaces the extracted metadata set; absent optional values clear stored values.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn update_metadata(
    conn: &mut PgConnection,
    id: PhotoId,
    metadata: &PhotoMetadata,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"UPDATE photos SET title=$2,description=$3,taken_at=$4,width=$5,height=$6,orientation=$7,is_hdr=$8,has_gain_map=$9,icc_name=$10,camera_make=$11,camera_model=$12,lens=$13,focal_mm=$14,aperture=$15,shutter=$16,iso=$17,gps_lat=$18,gps_lon=$19,place_id=$20,dominant_colors=$21,placeholder=$22 WHERE id=$1"#, id as _, metadata.title, metadata.description, metadata.taken_at, metadata.width, metadata.height, metadata.orientation, metadata.is_hdr, metadata.has_gain_map, metadata.icc_name, metadata.camera_make, metadata.camera_model, metadata.lens, metadata.focal_mm, metadata.aperture, metadata.shutter, metadata.iso, metadata.gps_lat, metadata.gps_lon, metadata.place_id, &metadata.dominant_colors, metadata.placeholder).execute(conn).await?.rows_affected())
}

/// Moves the supplied photos together in one statement, preserving their URLs.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn move_to_album(
    conn: &mut PgConnection,
    ids: &[PhotoId],
    album: AlbumId,
) -> Result<u64, DbError> {
    let ids: Vec<_> = ids.iter().map(|id| id.0).collect();
    Ok(sqlx::query!(
        r#"UPDATE photos SET album_id=$2 WHERE id=ANY($1)"#,
        &ids,
        album as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Sets the explicit visibility of all supplied photos without resolving access.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn set_visibility(
    conn: &mut PgConnection,
    ids: &[PhotoId],
    visibility: Visibility,
) -> Result<u64, DbError> {
    let ids: Vec<_> = ids.iter().map(|id| id.0).collect();
    Ok(sqlx::query!(
        r#"UPDATE photos SET visibility=$2 WHERE id=ANY($1)"#,
        &ids,
        visibility as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Moves a photo to trash while preserving its original and interactions.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn soft_delete(conn: &mut PgConnection, id: PhotoId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE photos SET deleted_at=now() WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Restores a trashed photo without changing its URL.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn restore(conn: &mut PgConnection, id: PhotoId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE photos SET deleted_at=NULL WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Permanently deletes older trash rows and their dependent records; storage cleanup is separate.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn purge_deleted_before(
    conn: &mut PgConnection,
    before: OffsetDateTime,
) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"DELETE FROM photos WHERE deleted_at < $1"#, before)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Replaces original metadata without changing the photo URL or interactions.
/// Removes stale derivatives, video, tags and ML data in the same transaction.
/// Resets publication and enqueues ingest for the new hash even if an older ingest is running.
///
/// # Errors
/// Returns constraint or connection failures; no partial replacement is committed.
pub async fn replace_original(
    conn: &mut PgConnection,
    id: PhotoId,
    original: &NewPhoto,
    metadata: &PhotoMetadata,
) -> Result<(), DbError> {
    use sqlx::Connection;
    let mut tx = conn.begin().await?;
    sqlx::query!("UPDATE photos SET original_hash=$2, original_name=$3, original_bytes=$4, mime=$5, kind=$6,status='pending',failure_reason=NULL,derivative_plan=NULL WHERE id=$1", id as _, original.original_hash, original.original_name, original.original_bytes, original.mime, original.kind as _).execute(&mut *tx).await?;
    update_metadata(&mut tx, id, metadata).await?;
    sqlx::query!("DELETE FROM derivatives WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM video_renditions WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM embeddings WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM faces WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM ocr_text WHERE photo_id=$1", id as _)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM photo_tags WHERE photo_id=$1 AND source='auto'",
        id as _
    )
    .execute(&mut *tx)
    .await?;
    crate::jobs::enqueue_unique(
        &mut tx,
        "ingest",
        &serde_json::json!({"photo_id": id, "original_hash": original.original_hash}),
        0,
        None,
        3,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// An album's validated visibility and grants, ordered leaf to root in `OwnerRow`.
#[derive(Clone, Debug)]
pub struct AlbumOwner {
    /// Album at this level.
    pub id: AlbumId,
    /// Visibility at this level only.
    pub visibility: Visibility,
    /// Explicit user and group grants at this level only.
    pub grants: Vec<yhotos_core::access::Grant>,
}

/// Complete authorization context and response metadata for one live file owner.
#[derive(Clone, Debug)]
pub struct OwnerRow {
    /// Owning photo; a digest can have more than one owner.
    pub photo_id: PhotoId,
    /// Explicit photo visibility.
    pub photo_visibility: Visibility,
    /// Photo grants, separate from album grants.
    pub photo_grants: Vec<yhotos_core::access::Grant>,
    /// Complete album chain, containing album first, nearest parent next.
    pub albums: Vec<AlbumOwner>,
    /// Stored original display name, never a filesystem path.
    pub original_name: String,
    /// Source media kind.
    pub kind: MediaKind,
    /// Database-derived MIME for the selected representation.
    pub mime: String,
    /// Nonnegative byte length of the selected representation.
    pub bytes: u64,
    /// Positive width, when known.
    pub width: Option<i32>,
    /// Positive height, when known.
    pub height: Option<i32>,
}

struct FileOwnerDb {
    photo_id: PhotoId,
    photo_visibility: Visibility,
    original_name: String,
    kind: MediaKind,
    bytes: i64,
    mime: String,
    width: Option<i32>,
    height: Option<i32>,
    photo_users: Vec<Uuid>,
    photo_groups: Vec<Uuid>,
    albums: serde_json::Value,
}
#[derive(serde::Deserialize)]
struct AlbumOwnerDb {
    id: AlbumId,
    visibility: String,
    users: Vec<Uuid>,
    groups: Vec<Uuid>,
}
fn file_grants(users: Vec<Uuid>, groups: Vec<Uuid>) -> Vec<yhotos_core::access::Grant> {
    use yhotos_core::{GroupId, UserId, access::Grant};
    users
        .into_iter()
        .map(|id| Grant::User(UserId(id)))
        .chain(groups.into_iter().map(|id| Grant::Group(GroupId(id))))
        .collect()
}

/// All live owners of an original, including complete ancestor grants, in one query.
/// Multiple owners are preserved so a restricted duplicate cannot conceal a public copy.
///
/// # Errors
/// Returns database failures or invalid stored metadata. Never authorizes the caller.
pub async fn owner_of_hash(conn: &mut PgConnection, hash: &str) -> Result<Vec<OwnerRow>, DbError> {
    file_owners(conn, hash, "original", "", "", false).await
}

pub(crate) async fn file_owners(
    conn: &mut PgConnection,
    hash: &str,
    area: &str,
    variant: &str,
    format: &str,
    hdr: bool,
) -> Result<Vec<OwnerRow>, DbError> {
    let rows = sqlx::query_file_as!(
        FileOwnerDb,
        "queries/file_owners.sql",
        hash,
        area,
        variant,
        format,
        hdr
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            let chain: Vec<AlbumOwnerDb> =
                serde_json::from_value(row.albums).map_err(|_| DbError::InvalidInput)?;
            let albums = chain
                .into_iter()
                .map(|album| {
                    let visibility = match album.visibility.as_str() {
                        "public" => Visibility::Public,
                        "restricted" => Visibility::Restricted,
                        _ => return Err(DbError::InvalidInput),
                    };
                    Ok(AlbumOwner {
                        id: album.id,
                        visibility,
                        grants: file_grants(album.users, album.groups),
                    })
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            if albums.is_empty()
                || row.width.is_some_and(|n| n <= 0)
                || row.height.is_some_and(|n| n <= 0)
            {
                return Err(DbError::InvalidInput);
            }
            Ok(OwnerRow {
                photo_id: row.photo_id,
                photo_visibility: row.photo_visibility,
                photo_grants: file_grants(row.photo_users, row.photo_groups),
                albums,
                original_name: row.original_name,
                kind: row.kind,
                mime: row.mime,
                bytes: u64::try_from(row.bytes).map_err(|_| DbError::InvalidInput)?,
                width: row.width,
                height: row.height,
            })
        })
        .collect()
}

/// Fixed public sort choices; unknown external values fall back to date taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageSort {
    /// Most recent capture first, with posted date for missing capture dates.
    #[default]
    Taken,
    /// Most recently posted first.
    Posted,
    /// Alphabetical title, with a stable id tie breaker.
    Name,
    /// Administrator-specified ascending order.
    Manual,
}
impl PageSort {
    /// Accepts only the documented sort names.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "posted" => Self::Posted,
            "name" => Self::Name,
            "manual" => Self::Manual,
            _ => Self::Taken,
        }
    }
    /// Canonical query-string value; safe for links and SQL CASE selection.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Taken => "taken",
            Self::Posted => "posted",
            Self::Name => "name",
            Self::Manual => "manual",
        }
    }
}

/// A derivative descriptor; rendering validates URL components and dimensions.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct PageDerivative {
    /// Stored variant, from the fixed file-serving vocabulary.
    pub variant: String,
    /// Stored image encoding.
    pub format: String,
    /// Whether this representation contains HDR pixels.
    pub hdr: bool,
    /// Content digest, checked before constructing an image URL.
    pub hash: String,
    /// Positive pixel width.
    pub w: u32,
    /// Positive pixel height.
    pub h: u32,
}

/// Only page-relevant metadata; no grants, secrets or internal paths are rendered.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PagePhoto {
    /// Photo cache tag identity.
    pub id: PhotoId,
    /// Canonical public lookup key.
    pub short_id: ShortId,
    /// Album cache tag identity.
    pub album_id: AlbumId,
    /// Untrusted title, escaped by templates.
    pub title: String,
    /// Untrusted plain text description, escaped by templates.
    pub description: String,
    /// Original content digest, checked before constructing a download URL.
    pub original_hash: String,
    /// Display name only, percent-encoded for downloads.
    pub original_name: String,
    /// Closed media vocabulary, checked by rendering.
    pub kind: String,
    /// Optional source width.
    pub width: Option<u32>,
    /// Optional source height.
    pub height: Option<u32>,
    /// Source EXIF orientation; derivatives already have orientation applied.
    pub orientation: Option<u8>,
    /// RGB integers from extraction; out-of-range values are ignored.
    pub dominant_colors: Vec<i32>,
    /// Base64-encoded thumbhash from PostgreSQL, bounded at rendering.
    pub thumbhash: Option<String>,
    /// All available responsive representations.
    pub derivatives: Vec<PageDerivative>,
    /// Capture date as stored text, escaped when rendered.
    #[serde(default)]
    pub taken_at: Option<String>,
    /// Camera manufacturer.
    pub camera_make: Option<String>,
    /// Camera model.
    pub camera_model: Option<String>,
    /// Lens name.
    pub lens: Option<String>,
    /// Focal length in millimetres.
    pub focal_mm: Option<f64>,
    /// Lens aperture.
    pub aperture: Option<f64>,
    /// Exposure duration.
    pub shutter: Option<String>,
    /// ISO sensitivity.
    pub iso: Option<i32>,
    /// Geographic latitude, range checked by rendering.
    pub gps_lat: Option<f64>,
    /// Geographic longitude, range checked by rendering.
    pub gps_lon: Option<f64>,
}

/// A visible photo and its same-sort neighbours, fetched in one round trip.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PhotoPage {
    /// Visible photo and its derivatives.
    pub photo: PagePhoto,
    /// Root-to-leaf album breadcrumbs.
    pub chain: Vec<crate::albums::Breadcrumb>,
    /// Previous visible photo under the selected sort.
    pub previous: Option<ShortId>,
    /// Next visible photo under the selected sort.
    pub next: Option<ShortId>,
}

/// Resolves a short id only after SQL visibility and computes neighbours before filtering it.
///
/// # Errors
/// Returns connection failures or malformed stored metadata.
pub async fn page(
    conn: &mut PgConnection,
    principal: &yhotos_core::access::Principal<'_>,
    short_id: &ShortId,
    sort: PageSort,
) -> Result<Option<PhotoPage>, DbError> {
    let bindings = crate::access_sql::visible_photos(principal);
    let key = short_id.to_string();
    let row = crate::visible_page_query!(r#",
      ordered AS (
        SELECT p.*, lag(short_id) OVER ordering AS previous, lead(short_id) OVER ordering AS next
        FROM visible_photos p
        WHERE p.album_id=(SELECT album_id FROM visible_photos WHERE short_id=$6)
        WINDOW ordering AS (ORDER BY
          CASE WHEN $7='taken' THEN coalesce(taken_at,posted_at) END DESC,
          CASE WHEN $7='posted' THEN posted_at END DESC,
          CASE WHEN $7='name' THEN title END COLLATE "C" ASC,
          CASE WHEN $7='manual' THEN sort_order END ASC, id ASC)
      ) SELECT jsonb_build_object(
        'photo', to_jsonb(p) || jsonb_build_object('thumbhash',encode(p.placeholder,'base64'),
          'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
            'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
            ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb)),
        'chain', a.chain, 'previous',p.previous,'next',p.next) AS "payload!"
      FROM ordered p JOIN album_levels a ON a.id=p.album_id WHERE p.short_id=$6
    "#, bindings, key, sort.as_str()).fetch_optional(conn).await?;
    row.map(|r| serde_json::from_value(r.payload).map_err(|_| DbError::InvalidInput))
        .transpose()
}
