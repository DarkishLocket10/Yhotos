//! Visibility-filtered bounded map data and the server-rendered place fallback.
#[path = "map_bounds.rs"]
mod bounds;
use crate::{DbError, PgConnection};
pub use bounds::Bounds;
use yhotos_core::{ShortId, access::Principal};

/// A place is exposed only when at least one visible photo refers to it.
#[derive(Debug, serde::Deserialize)]
pub struct PlaceCount {
    /// Place identity used by the public place-results route.
    pub id: uuid::Uuid,
    /// Escaped city name.
    pub name: String,
    /// Escaped administrative region code.
    pub admin1: Option<String>,
    /// Country code.
    pub country: String,
    /// Visible photo count only.
    pub count: u64,
}
/// Lists all visible places with one query, including without JavaScript.
///
/// # Errors
/// Returns connection or invalid stored data errors.
pub async fn places(
    conn: &mut PgConnection,
    principal: &Principal<'_>,
) -> Result<Vec<PlaceCount>, DbError> {
    places_for(conn, &crate::access_sql::visible_photos(principal)).await
}
/// Lists places visible to the union of independently verified principals.
///
/// # Errors
/// Returns connection or invalid stored data errors.
pub async fn places_for(
    conn: &mut PgConnection,
    p: &crate::access_sql::Bindings,
) -> Result<Vec<PlaceCount>, DbError> {
    let row = crate::visible_page_query!(r#"SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY name,country,id),'[]'::jsonb) AS "payload!"
      FROM (SELECT l.id,l.name,l.admin1,l.country,count(*) AS count FROM places l
        JOIN visible_photos p ON p.place_id=l.id GROUP BY l.id) t"#,p).fetch_one(conn).await?;
    serde_json::from_value(row.payload).map_err(|_| DbError::InvalidInput)
}
/// A centroid and representative, computed exclusively from visible rows.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Cluster {
    /// Visible member count.
    pub count: u64,
    /// Mean longitude.
    pub lon: f64,
    /// Mean latitude.
    pub lat: f64,
    /// A visible representative's validated public id.
    pub short_id: ShortId,
    /// JPEG thumbnail digest, when one exists.
    pub thumb_hash: Option<String>,
    /// True at photo zoom, where exact-coordinate groups cannot split further.
    pub terminal: bool,
}
/// Grid-clusters the requested rectangle, returning at most 500 features.
/// At zoom 15 and above, photos at identical coordinates share one feature.
///
/// # Errors
/// Rejects zooms above 20 and malformed stored data; returns database failures.
pub async fn clusters(
    conn: &mut PgConnection,
    principal: &Principal<'_>,
    bounds: Bounds,
    zoom: u8,
) -> Result<Vec<Cluster>, DbError> {
    clusters_for(
        conn,
        &crate::access_sql::visible_photos(principal),
        bounds,
        zoom,
    )
    .await
}
/// Clusters only photos visible to the union of independently verified principals.
///
/// # Errors
/// Rejects invalid bounds or zooms; returns malformed stored data or database failures.
pub async fn clusters_for(
    conn: &mut PgConnection,
    p: &crate::access_sql::Bindings,
    bounds: Bounds,
    zoom: u8,
) -> Result<Vec<Cluster>, DbError> {
    if zoom > 20
        || Bounds::parse(&format!(
            "{},{},{},{}",
            bounds.west, bounds.south, bounds.east, bounds.north
        )) != Some(bounds)
    {
        return Err(DbError::InvalidInput);
    }
    let cells = (2_f64.powi(i32::from(zoom)) * 8.0 * (bounds.east - bounds.west) / 360.0)
        .ceil()
        .clamp(1.0, 22.0);
    // The representative's unique key comes from visible rows in this same
    // statement snapshot; thumbnail lookup need not repeat the access CTE.
    let rows = crate::visible_page_query!(
        r#",
      selected AS (
        SELECT p.short_id,p.gps_lon AS lon,p.gps_lat AS lat,
          CASE WHEN $10>=15 THEN p.gps_lon END AS point_lon,
          CASE WHEN $10>=15 THEN p.gps_lat END AS point_lat,
          CASE WHEN $10>=15 THEN 0::float8 ELSE
            least($11::float8-1,floor((p.gps_lon-$6)/($8-$6)*$11::float8)) + 22 *
            least($11::float8-1,floor((p.gps_lat-$7)/($9-$7)*$11::float8)) END AS cell
        FROM visible_photos p WHERE p.gps_point IS NOT NULL AND
          p.gps_point <@ box(point($6,$7),point($8,$9))
      ), clustered AS (
        SELECT count(*) AS count,
          coalesce(point_lon,avg(lon)) AS lon,
          coalesce(point_lat,avg(lat)) AS lat,
          min(short_id COLLATE "C") COLLATE "default" AS short_id
        FROM selected GROUP BY cell,point_lon,point_lat
      ) SELECT count AS "count!",lon AS "lon!",lat AS "lat!",short_id AS "short_id!",
        (SELECT d.hash FROM derivatives d
          WHERE d.photo_id=(SELECT id FROM photos WHERE short_id=c.short_id) AND d.variant='thumb' AND d.format='jpeg' AND NOT d.hdr) AS thumb_hash
      FROM clustered c ORDER BY count DESC,c.short_id COLLATE "C" LIMIT 500"#,
        p,
        bounds.west,
        bounds.south,
        bounds.east,
        bounds.north,
        i32::from(zoom),
        cells
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            if !r.lon.is_finite()
                || !r.lat.is_finite()
                || r.thumb_hash.as_ref().is_some_and(|h| {
                    h.len() != 64
                        || !h
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            {
                return Err(DbError::InvalidInput);
            }
            Ok(Cluster {
                count: u64::try_from(r.count).map_err(|_| DbError::InvalidInput)?,
                lon: r.lon,
                lat: r.lat,
                short_id: r.short_id.parse().map_err(|_| DbError::InvalidInput)?,
                thumb_hash: r.thumb_hash,
                terminal: zoom >= 15,
            })
        })
        .collect()
}

/// A bounded chooser entry; titles remain untrusted display text.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct PointPhoto {
    /// Canonical photo link identity.
    pub short_id: ShortId,
    /// Title for a text-only link.
    pub title: String,
}
/// Visible photos at an exact coordinate, paginated before serialization.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct PointPage {
    /// At most 60 entries, ordered by capture/posted date and identity.
    pub photos: Vec<PointPhoto>,
    /// Complete visible count.
    pub total: u64,
}
/// Lists visible photos sharing a coordinate in one statement.
///
/// # Errors
/// Rejects invalid coordinates and page zero; returns invalid stored data or SQL errors.
pub async fn point_photos(
    conn: &mut PgConnection,
    principal: &Principal<'_>,
    lon: f64,
    lat: f64,
    number: u32,
) -> Result<Option<PointPage>, DbError> {
    point_photos_for(
        conn,
        &crate::access_sql::visible_photos(principal),
        lon,
        lat,
        number,
    )
    .await
}
/// Lists coordinate matches visible to the union of independently verified principals.
///
/// # Errors
/// Rejects invalid coordinates or pagination; returns malformed stored data or SQL errors.
pub async fn point_photos_for(
    conn: &mut PgConnection,
    p: &crate::access_sql::Bindings,
    lon: f64,
    lat: f64,
    number: u32,
) -> Result<Option<PointPage>, DbError> {
    if !lon.is_finite()
        || !lat.is_finite()
        || !(-180.0..=180.0).contains(&lon)
        || !(-85.051_129..=85.051_129).contains(&lat)
        || number == 0
    {
        return Err(DbError::InvalidInput);
    }
    let offset = i64::from(number - 1) * 60;
    let row = crate::visible_page_query!(
        r#",
      selected AS MATERIALIZED (
        SELECT p.id,coalesce(p.taken_at,p.posted_at) AS date FROM visible_photos p
        WHERE p.gps_point <@ box(point($6,$7),point($6,$7))
          AND p.gps_lon=$6 AND p.gps_lat=$7
      ) SELECT jsonb_build_object(
        'total',(SELECT count(*) FROM selected),
        'photos',coalesce((SELECT jsonb_agg(jsonb_build_object('short_id',short_id,'title',title)
          ORDER BY date DESC,id) FROM (
            SELECT p.id,p.short_id,p.title,s.date FROM (
              SELECT id,date FROM selected ORDER BY date DESC,id LIMIT 60 OFFSET $8
            ) s JOIN visible_photos p ON p.id=s.id
          ) t),'[]'::jsonb)) AS "payload!"
    "#,
        p,
        lon,
        lat,
        offset
    )
    .fetch_one(conn)
    .await?;
    let page: PointPage = serde_json::from_value(row.payload).map_err(|_| DbError::InvalidInput)?;
    Ok((!page.photos.is_empty()).then_some(page))
}
