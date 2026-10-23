//! Administrative, transaction-composable Piwigo import operations.
use crate::{DbError, albums, photos, redirects};
use sqlx::PgConnection;
use time::{OffsetDateTime, PrimitiveDateTime};
use yhotos_core::{AlbumId, PhotoId, Slug, TagId, access::Visibility};

/// Temporarily frees imported slugs so renames and sibling collisions can be
/// applied in any order. Call inside a transaction and upsert every supplied
/// source identity before committing; readers retain their previous MVCC view.
///
/// # Errors
/// Returns a database failure; callers must roll back on any import error.
pub async fn prepare_slugs(
    conn: &mut PgConnection,
    album_sources: &[i64],
    tag_sources: &[i64],
) -> Result<(), DbError> {
    // Use a fresh prefix independent of planned source slugs.
    // Prefix + existing UUID stays below the eighty-character slug limit.
    let prefix = format!("stage-{}-", uuid::Uuid::now_v7().simple());
    sqlx::query!("UPDATE albums a SET slug=($2::text || replace(a.id::text,'-',''))::citext FROM migration_albums m WHERE a.id=m.album_id AND m.source_id=ANY($1)",album_sources,&prefix).execute(&mut *conn).await?;
    sqlx::query!("UPDATE tags t SET slug=($2::text || replace(t.id::text,'-',''))::citext FROM migration_tags m WHERE t.id=m.tag_id AND m.source_id=ANY($1)",tag_sources,&prefix).execute(conn).await?;
    Ok(())
}

// Resolve against retained destination identities as well as this run's plan.
// Staged source identities are free to rename; absent identities keep their URLs.
fn available_slug(requested: &Slug, occupied: &[String]) -> Slug {
    let mut candidate = requested.clone();
    let mut suffix = 2;
    while occupied
        .iter()
        .any(|s| s.eq_ignore_ascii_case(candidate.as_str()))
    {
        let tail = format!("-{suffix}");
        let stem = requested
            .as_str()
            .chars()
            .take(80 - tail.len())
            .collect::<String>();
        candidate = Slug::from_title(&format!("{}{tail}", stem.trim_end_matches('-')));
        suffix += 1;
    }
    candidate
}

/// Removes the previous import's redirects, preserving independently managed
/// mappings (including migration mappings that were manually retargeted).
/// Call before inserting this run's mappings in the same transaction.
///
/// # Errors
/// Returns database errors; callers must roll back on failure.
pub async fn reconcile_redirects(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM legacy_redirects r USING migration_redirects m WHERE r.pattern=m.pattern AND r.target_kind=m.target_kind AND r.target_id=m.target_id").execute(&mut *conn).await?;
    sqlx::query!("DELETE FROM migration_redirects")
        .execute(conn)
        .await?;
    Ok(())
}

/// Stores an exact internal redirect and records import ownership atomically
/// within the caller's transaction, for reconciliation on the next import.
///
/// # Errors
/// Returns database errors; callers must roll back on failure.
pub async fn insert_redirect(
    conn: &mut PgConnection,
    path: &str,
    target: yhotos_core::access::ShareTarget,
) -> Result<(), DbError> {
    redirects::insert(conn, path, target).await?;
    sqlx::query!("INSERT INTO migration_redirects(pattern,target_kind,target_id) SELECT pattern,target_kind,target_id FROM legacy_redirects WHERE pattern=$1 ON CONFLICT(pattern) DO UPDATE SET target_kind=excluded.target_kind,target_id=excluded.target_id",path).execute(conn).await?;
    Ok(())
}

/// Upserts one source album by its persistent Piwigo identity.
///
/// # Errors
/// Rejects invalid source ids and returns constraint or connection errors.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_album(
    conn: &mut PgConnection,
    source: i64,
    parent: Option<AlbumId>,
    slug: &Slug,
    title: &str,
    description: &str,
    order: i32,
    restricted: bool,
) -> Result<AlbumId, DbError> {
    if source <= 0 {
        return Err(DbError::InvalidInput);
    }
    let existing = sqlx::query!(
        "SELECT album_id FROM migration_albums WHERE source_id=$1",
        source
    )
    .fetch_optional(&mut *conn)
    .await?;
    let occupied = sqlx::query_scalar!("SELECT slug::text AS \"slug!\" FROM albums WHERE parent_id IS NOT DISTINCT FROM $1 AND id NOT IN (SELECT album_id FROM migration_albums WHERE source_id=$2)",parent.map(|p|p.0),source).fetch_all(&mut *conn).await?;
    let slug = available_slug(slug, &occupied);
    let visibility = if restricted {
        Visibility::Restricted
    } else {
        Visibility::Public
    };
    let id = if let Some(row) = existing {
        AlbumId(row.album_id)
    } else {
        let row = albums::create(conn, parent, &slug, title, visibility).await?;
        sqlx::query!(
            "INSERT INTO migration_albums(source_id,album_id) VALUES ($1,$2)",
            source,
            row.id.0
        )
        .execute(&mut *conn)
        .await?;
        row.id
    };
    // Move and rename together: the new slug need only be free in the new parent.
    sqlx::query!("UPDATE albums SET parent_id=$2,slug=$3,title=$4,description=$5,sort_order=$6,visibility=$7,manual_order=true WHERE id=$1", id.0,parent.map(|p|p.0),slug as _,title,description,order,visibility as _).execute(&mut *conn).await?;
    sqlx::query!("DELETE FROM album_grants WHERE album_id=$1", id.0)
        .execute(conn)
        .await?;
    Ok(id)
}

/// Validated source metadata for one image's category membership.
pub struct ImportPhoto {
    /// Positive Piwigo image identity.
    pub image_id: i64,
    /// Positive Piwigo category identity.
    pub category_id: i64,
    /// Original shared by every membership of this image.
    pub original: photos::NewPhoto,
    /// Plain text description.
    pub description: String,
    /// Plain text attribution.
    pub author: Option<String>,
    /// Source capture instant, with the operator's declared UTC offset.
    pub taken_at: Option<OffsetDateTime>,
    /// Uninterpreted source capture wall-clock time.
    pub taken_at_local: Option<PrimitiveDateTime>,
    /// Source publication instant.
    pub posted_at: OffsetDateTime,
    /// Category-specific order.
    pub sort_order: i32,
    /// Admin-only when true; no grants survive import.
    pub restricted: bool,
    /// Validated latitude.
    pub gps_lat: Option<f64>,
    /// Validated longitude.
    pub gps_lon: Option<f64>,
}

/// Batch-upserts memberships, reusing identities and unchanged originals.
/// New/replaced originals, removed source overrides and unfinished restorations
/// enqueue a new ingest generation atomically with the caller's transaction.
///
/// # Errors
/// Returns invalid source ids, constraints or database errors. Roll back on failure.
pub async fn upsert_photos(
    conn: &mut PgConnection,
    rows: &[ImportPhoto],
) -> Result<Vec<PhotoId>, DbError> {
    let mut ids = Vec::with_capacity(rows.len());
    let mut previous_gps = std::collections::BTreeMap::new();
    for row in rows {
        if row.image_id <= 0 || row.category_id <= 0 {
            return Err(DbError::InvalidInput);
        }
        let old = sqlx::query!("SELECT m.photo_id,m.original_hash,m.taken_at AS source_taken,m.taken_at_local AS source_local,m.gps_lat AS source_lat,m.gps_lon AS source_lon,p.gps_lat,p.gps_lon,p.deleted_at,p.status::text AS \"status!\",p.derivative_plan,EXISTS(SELECT 1 FROM derivatives d WHERE d.photo_id=p.id) AS \"has_derivatives!\" FROM migration_sources m JOIN photos p ON p.id=m.photo_id WHERE m.image_id=$1 AND m.category_id=$2 FOR UPDATE OF p", row.image_id, row.category_id).fetch_optional(&mut *conn).await?;
        let gps = old
            .as_ref()
            .map(|p| (p.gps_lat, p.gps_lon))
            .unwrap_or_default();
        let id = if let Some(old) = old {
            let id = PhotoId(old.photo_id);
            let removed_override = (old.source_taken.is_some() && row.taken_at.is_none())
                || (old.source_local.is_some() && row.taken_at_local.is_none())
                || (old.source_lat.is_some() && row.gps_lat.is_none())
                || (old.source_lon.is_some() && row.gps_lon.is_none());
            let unfinished_restoration = old.deleted_at.is_some()
                && (old.status != "ready" || old.derivative_plan.is_none() || !old.has_derivatives);
            if old.original_hash != row.original.original_hash
                || removed_override
                || unfinished_restoration
            {
                // The published value may be an old source override, not EXIF.
                // Reset extraction and fence old workers with a fresh generation.
                photos::replace_original(
                    conn,
                    id,
                    &row.original,
                    &photos::PhotoMetadata::default(),
                )
                .await?;
            }
            id
        } else {
            let photo = photos::create(conn, &row.original).await?;
            crate::jobs::enqueue_unique(conn, "ingest", &serde_json::json!({"photo_id":photo.id,"original_hash":row.original.original_hash,"generation":0}), 0, None, 3).await?;
            photo.id
        };
        sqlx::query!("INSERT INTO migration_sources(image_id,category_id,original_hash,photo_id,taken_at,taken_at_local,gps_lat,gps_lon) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(image_id,category_id) DO UPDATE SET original_hash=excluded.original_hash,taken_at=excluded.taken_at,taken_at_local=excluded.taken_at_local,gps_lat=excluded.gps_lat,gps_lon=excluded.gps_lon",row.image_id,row.category_id,row.original.original_hash,id.0,row.taken_at,row.taken_at_local,row.gps_lat,row.gps_lon).execute(&mut *conn).await?;
        previous_gps.insert(id.0, gps);
        ids.push(id);
    }
    // Removed overrides were reset above. Remaining absent values can use EXIF.
    let uuid: Vec<_> = ids.iter().map(|id| id.0).collect();
    let albums: Vec<_> = rows.iter().map(|r| r.original.album_id.0).collect();
    let titles: Vec<_> = rows.iter().map(|r| r.original.title.as_str()).collect();
    let descriptions: Vec<_> = rows.iter().map(|r| r.description.as_str()).collect();
    let authors: Vec<_> = rows.iter().map(|r| r.author.as_deref()).collect();
    let taken: Vec<_> = rows.iter().map(|r| r.taken_at).collect();
    let local: Vec<_> = rows.iter().map(|r| r.taken_at_local).collect();
    let posted: Vec<_> = rows.iter().map(|r| r.posted_at).collect();
    let orders: Vec<_> = rows.iter().map(|r| r.sort_order).collect();
    let restricted: Vec<_> = rows.iter().map(|r| r.restricted).collect();
    let lat: Vec<_> = rows.iter().map(|r| r.gps_lat).collect();
    let lon: Vec<_> = rows.iter().map(|r| r.gps_lon).collect();
    let names: Vec<_> = rows
        .iter()
        .map(|r| r.original.original_name.as_str())
        .collect();
    let published = sqlx::query!("UPDATE photos p SET deleted_at=NULL,place_id=CASE WHEN (p.gps_lat,p.gps_lon) IS DISTINCT FROM (coalesce(s.lat,p.gps_lat),coalesce(s.lon,p.gps_lon)) THEN NULL ELSE p.place_id END,album_id=s.album,title=s.title,description=s.description,author=s.author,taken_at=coalesce(s.taken,p.taken_at),taken_at_local=coalesce(s.local,p.taken_at_local),posted_at=s.posted,sort_order=s.ord,visibility=CASE WHEN s.restricted THEN 'restricted'::visibility ELSE 'inherit'::visibility END,gps_lat=coalesce(s.lat,p.gps_lat),gps_lon=coalesce(s.lon,p.gps_lon),original_name=s.name FROM UNNEST($1::uuid[],$2::uuid[],$3::text[],$4::text[],$5::text[],$6::timestamptz[],$7::timestamp[],$8::timestamptz[],$9::int[],$10::bool[],$11::float8[],$12::float8[],$13::text[]) AS s(id,album,title,description,author,taken,local,posted,ord,restricted,lat,lon,name) WHERE p.id=s.id RETURNING p.id,p.gps_lat,p.gps_lon", &uuid,&albums,&titles as &[&str],&descriptions as &[&str],&authors as &[Option<&str>],&taken as &[Option<OffsetDateTime>],&local as &[Option<PrimitiveDateTime>],&posted,&orders,&restricted,&lat as &[Option<f64>],&lon as &[Option<f64>],&names as &[&str]).fetch_all(&mut *conn).await?;
    for photo in published {
        if previous_gps.get(&photo.id) != Some(&(photo.gps_lat, photo.gps_lon)) {
            // Each coordinate change needs its own refresh: an older running
            // job may already have resolved the previous location. Deduplicating
            // against it could lose this change before that job marks completion.
            crate::jobs::enqueue(
                conn,
                "geo.reverse",
                &serde_json::json!({"photo_id":photo.id}),
                0,
                OffsetDateTime::now_utc(),
            )
            .await?;
        }
    }
    sqlx::query!("DELETE FROM photo_grants WHERE photo_id=ANY($1)", &uuid)
        .execute(conn)
        .await?;
    Ok(ids)
}

/// Retires obsolete migration-owned copies and revokes their grants atomically
/// with the caller's import transaction. Source identities remain for resumable
/// restoration, but obsolete photos cannot be viewed or downloaded.
///
/// # Errors
/// Returns database errors; callers must roll back the entire import on failure.
pub async fn reconcile_photos(
    conn: &mut PgConnection,
    retained: &[PhotoId],
) -> Result<(), DbError> {
    let ids: Vec<_> = retained.iter().map(|id| id.0).collect();
    sqlx::query!("UPDATE photos p SET visibility='restricted',deleted_at=coalesce(p.deleted_at,now()) FROM migration_sources m WHERE p.id=m.photo_id AND NOT (p.id=ANY($1))", &ids).execute(&mut *conn).await?;
    sqlx::query!("DELETE FROM photo_grants g USING migration_sources m WHERE g.photo_id=m.photo_id AND NOT (g.photo_id=ANY($1))", &ids).execute(conn).await?;
    Ok(())
}

/// Removes source-owned manual tag links before replacing them on a rerun.
///
/// # Errors
/// Returns database errors.
pub async fn clear_manual_tags(conn: &mut PgConnection, id: PhotoId) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM photo_tags WHERE photo_id=$1 AND source='manual'",
        id.0
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Upserts a manual tag using its Piwigo identity, preserving links on rename.
///
/// # Errors
/// Rejects nonpositive ids and returns constraints or database failures.
pub async fn upsert_tag(
    conn: &mut PgConnection,
    source: i64,
    slug: &Slug,
    name: &str,
) -> Result<TagId, DbError> {
    if source <= 0 {
        return Err(DbError::InvalidInput);
    }
    let occupied = sqlx::query_scalar!("SELECT slug::text AS \"slug!\" FROM tags WHERE id NOT IN (SELECT tag_id FROM migration_tags WHERE source_id=$1)",source).fetch_all(&mut *conn).await?;
    let slug = available_slug(slug, &occupied);
    if let Some(row) = sqlx::query!(
        "SELECT tag_id FROM migration_tags WHERE source_id=$1",
        source
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        sqlx::query!(
            "UPDATE tags SET slug=$2,name=$3,source='manual' WHERE id=$1",
            row.tag_id,
            slug as _,
            name
        )
        .execute(conn)
        .await?;
        return Ok(TagId(row.tag_id));
    }
    let row = sqlx::query!("INSERT INTO tags(id,slug,name,source) VALUES ($1,$2,$3,'manual') ON CONFLICT(slug) DO UPDATE SET name=excluded.name,source='manual' RETURNING id",TagId::new().0,slug as _,name).fetch_one(&mut *conn).await?;
    sqlx::query!(
        "INSERT INTO migration_tags(source_id,tag_id) VALUES ($1,$2)",
        source,
        row.id
    )
    .execute(conn)
    .await?;
    Ok(TagId(row.id))
}
