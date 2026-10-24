//! Administrative biometric repository; absent from public builds.
use crate::DbError;
use sqlx::PgConnection;
use uuid::Uuid;

/// A private person summary, excluding hidden identities.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Person {
    /// Internal administrative identity.
    pub id: Uuid,
    /// Untrusted, optional display name.
    pub name: Option<String>,
    /// Face count in non-deleted photos.
    pub count: i64,
    /// Up to three representative face ids, highest detector quality first.
    pub samples: Vec<Uuid>,
}
/// A stable sixty-person page, excluding hidden or empty clusters.
pub struct PeoplePage {
    /// Private summaries in name/identifier order.
    pub people: Vec<Person>,
    /// Matching clusters before pagination.
    pub total: usize,
}
/// Lists a one-based page of private clusters, optionally excluding a merge source.
///
/// # Errors
/// Rejects page zero and returns database failures or malformed stored summaries.
pub async fn people(
    conn: &mut PgConnection,
    page: u32,
    exclude: Option<Uuid>,
) -> Result<PeoplePage, DbError> {
    let offset = i64::from(page.checked_sub(1).ok_or(DbError::InvalidInput)?) * 60;
    // Three quality-ranked samples are a deliberate preview cap, not a face
    // browser. The linked person photo grid paginates the complete collection.
    let row=sqlx::query!(r#"WITH clusters AS MATERIALIZED (
        SELECT p.id,p.name,count(f.id) AS count,(array_agg(f.id ORDER BY f.quality DESC,f.id))[1:3] AS samples FROM persons p
        JOIN faces f ON f.person_id=p.id JOIN photos photo ON photo.id=f.photo_id
        WHERE NOT p.hidden AND ($2::uuid IS NULL OR p.id<>$2) AND photo.deleted_at IS NULL AND photo.status='ready' GROUP BY p.id
      ), person_page AS (SELECT * FROM clusters ORDER BY name NULLS LAST,id LIMIT 60 OFFSET $1)
      SELECT (SELECT count(*) FROM clusters) AS "total!",
        coalesce(jsonb_agg(to_jsonb(p) ORDER BY p.name NULLS LAST,p.id),'[]'::jsonb) AS "people!"
      FROM person_page p"#,offset,exclude).fetch_one(conn).await?;
    Ok(PeoplePage {
        people: serde_json::from_value(row.people).map_err(|_| DbError::InvalidInput)?,
        total: usize::try_from(row.total).map_err(|_| DbError::InvalidInput)?,
    })
}
/// Updates a name or hides an identity, refusing to disclose hidden identities again.
///
/// # Errors
/// Returns invalid text or database failures.
pub async fn update(
    conn: &mut PgConnection,
    id: Uuid,
    name: &str,
    hide: bool,
) -> Result<bool, DbError> {
    if name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(DbError::InvalidInput);
    }
    Ok(sqlx::query!(
        "UPDATE persons SET name=nullif($2,''),hidden=$3 WHERE id=$1 AND NOT hidden",
        id,
        name,
        hide
    )
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}
/// Explicitly merges two non-hidden identities only in an administrator's transaction.
///
/// # Errors
/// Returns invalid identities or database failures; callers roll back on error.
pub async fn merge(conn: &mut PgConnection, source: Uuid, target: Uuid) -> Result<(), DbError> {
    if source == target {
        return Err(DbError::InvalidInput);
    }
    let rows = sqlx::query!(
        "SELECT id FROM persons WHERE id=ANY($1) AND NOT hidden ORDER BY id FOR UPDATE",
        &[source, target]
    )
    .fetch_all(&mut *conn)
    .await?;
    if rows.len() != 2 {
        return Err(DbError::InvalidInput);
    }
    sqlx::query!(
        "UPDATE faces SET person_id=$2 WHERE person_id=$1",
        source,
        target
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!("DELETE FROM persons WHERE id=$1", source)
        .execute(conn)
        .await?;
    Ok(())
}
/// A stable page of up to sixty distinct ready photos, with the full match count.
pub struct PhotoPage {
    /// Matching photos in identifier order, after pagination.
    pub photos: Vec<crate::photos::PagePhoto>,
    /// Count before pagination, excluding hidden identities and deleted photos.
    pub total: usize,
}

/// Returns a sixty-photo page for a non-hidden cluster.
///
/// # Errors
/// Rejects page zero and returns malformed metadata or database failures.
pub async fn photos(
    conn: &mut PgConnection,
    person: Uuid,
    page: u32,
) -> Result<PhotoPage, DbError> {
    find(conn, Some(person), None, page).await
}

/// Finds a named person only on the admin listener; hidden people never match.
///
/// # Errors
/// Rejects invalid names or page zero and returns malformed metadata or database failures.
pub async fn named_photos(
    conn: &mut PgConnection,
    name: &str,
    page: u32,
) -> Result<PhotoPage, DbError> {
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(DbError::InvalidInput);
    }
    find(conn, None, Some(name), page).await
}
async fn find(
    conn: &mut PgConnection,
    id: Option<Uuid>,
    name: Option<&str>,
    page: u32,
) -> Result<PhotoPage, DbError> {
    let offset = i64::from(page.checked_sub(1).ok_or(DbError::InvalidInput)?) * 60;
    let row=sqlx::query!(r#"WITH matches AS MATERIALIZED (
      SELECT p.id FROM photos p WHERE p.deleted_at IS NULL AND p.status='ready' AND EXISTS (
        SELECT FROM faces f JOIN persons person ON person.id=f.person_id
        WHERE f.photo_id=p.id AND NOT person.hidden AND (person.id=$1 OR lower(person.name)=lower($2)))
      ), photo_page AS (
        SELECT p.* FROM photos p JOIN matches m ON m.id=p.id ORDER BY p.id LIMIT 60 OFFSET $3
      ) SELECT (SELECT count(*) FROM matches) AS "total!",
      coalesce(jsonb_agg(to_jsonb(p) || jsonb_build_object(
      'taken_at',to_char(p.taken_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
      'thumbhash',encode(p.placeholder,'base64'),
      'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object('variant',d.variant,'format',d.format,
      'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height) ORDER BY d.width,d.format,d.hdr)
      FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb)) ORDER BY p.id),'[]'::jsonb) AS "photos!"
      FROM photo_page p"#,
        id,name,offset).fetch_one(conn).await?;
    Ok(PhotoPage {
        photos: serde_json::from_value(row.photos).map_err(|_| DbError::InvalidInput)?,
        total: usize::try_from(row.total).map_err(|_| DbError::InvalidInput)?,
    })
}

/// Current non-hidden face, its medium input, and an optional matching private cache.
pub struct Preview {
    /// Normalized untrusted box; callers validate before requesting a crop.
    pub bbox: Vec<f32>,
    /// Medium SDR JPEG digest, validated through storage by the caller.
    pub source: String,
    /// Cache digest only when both input digest and box still match.
    pub cached: Option<String>,
}
/// Resolves a preview only for a non-hidden identity in a ready, non-deleted photo.
///
/// # Errors
/// Returns database failures.
pub async fn preview(conn: &mut PgConnection, id: Uuid) -> Result<Option<Preview>, DbError> {
    Ok(sqlx::query!("SELECT f.bbox,d.hash AS source,c.hash AS cached FROM faces f JOIN persons p ON p.id=f.person_id JOIN photos photo ON photo.id=f.photo_id JOIN derivatives d ON d.photo_id=photo.id AND d.variant='medium' AND d.format='jpeg' AND NOT d.hdr LEFT JOIN face_crops c ON c.face_id=f.id AND c.source_hash=d.hash AND c.bbox=f.bbox WHERE f.id=$1 AND NOT p.hidden AND photo.deleted_at IS NULL AND photo.status='ready'",id).fetch_optional(conn).await?.map(|r|Preview {bbox:r.bbox,source:r.source,cached:r.cached}))
}
/// Publishes a private cache reference only if source, box and privacy still match.
/// No face derivative is inserted into the publicly readable derivatives table.
///
/// # Errors
/// Returns invalid sizes or database failures.
pub async fn cache_crop(
    conn: &mut PgConnection,
    id: Uuid,
    bbox: &[f32],
    source: &str,
    hash: &str,
    bytes: u64,
) -> Result<bool, DbError> {
    let bytes = i64::try_from(bytes).map_err(|_| DbError::InvalidInput)?;
    Ok(sqlx::query!("INSERT INTO face_crops(face_id,bbox,source_hash,hash,bytes) SELECT f.id,f.bbox,d.hash,$4,$5 FROM faces f JOIN persons p ON p.id=f.person_id JOIN photos photo ON photo.id=f.photo_id JOIN derivatives d ON d.photo_id=photo.id AND d.variant='medium' AND d.format='jpeg' AND NOT d.hdr WHERE f.id=$1 AND f.bbox=$2 AND d.hash=$3 AND NOT p.hidden AND photo.deleted_at IS NULL AND photo.status='ready' ON CONFLICT(face_id) DO UPDATE SET bbox=excluded.bbox,source_hash=excluded.source_hash,hash=excluded.hash,bytes=excluded.bytes",id,bbox,source,hash,bytes).execute(conn).await?.rows_affected()==1)
}
