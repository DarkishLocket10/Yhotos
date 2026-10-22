//! Visibility-filtered bounded map data and the server-rendered place fallback.
#[path = "map_bounds.rs"]
mod bounds;
use crate::{DbError, PgConnection};
pub use bounds::Bounds;
use yhotos_core::{ShortId, access::Principal};

/// A place is exposed only when at least one visible photo refers to it.
#[derive(Debug, serde::Deserialize)]
pub struct PlaceCount {
    /// Internal place identity; public filters use the encoded name.
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
    let p = crate::access_sql::visible_photos(principal);
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
}
/// Grid-clusters the requested rectangle, returning at most 500 features.
/// At zoom 15 and above, each returned feature represents exactly one photo.
///
/// # Errors
/// Rejects zooms above 20 and malformed stored data; returns database failures.
pub async fn clusters(
    conn: &mut PgConnection,
    principal: &Principal<'_>,
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
    let p = crate::access_sql::visible_photos(principal);
    let cells = (2_f64.powi(i32::from(zoom)) * 8.0 * (bounds.east - bounds.west) / 360.0)
        .ceil()
        .clamp(1.0, 22.0);
    // The representative's unique key comes from visible rows in this same
    // statement snapshot; thumbnail lookup need not repeat the access CTE.
    let rows = crate::visible_page_query!(
        r#",
      selected AS (
        SELECT p.short_id,p.gps_lon AS lon,p.gps_lat AS lat,
          CASE WHEN $10>=15 THEN p.short_id COLLATE "C" END AS individual,
          CASE WHEN $10>=15 THEN 0::float8 ELSE
            least($11::float8-1,floor((p.gps_lon-$6)/($8-$6)*$11::float8)) + 22 *
            least($11::float8-1,floor((p.gps_lat-$7)/($9-$7)*$11::float8)) END AS cell
        FROM visible_photos p WHERE p.gps_point IS NOT NULL AND
          p.gps_point <@ box(point($6,$7),point($8,$9))
      ), clustered AS (
        SELECT count(*) AS count,avg(lon) AS lon,avg(lat) AS lat,min(short_id COLLATE "C") COLLATE "default" AS short_id
        FROM selected GROUP BY cell,individual
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
            })
        })
        .collect()
}
