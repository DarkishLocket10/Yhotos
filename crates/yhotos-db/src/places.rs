//! Offline reverse geocoding. No network or filesystem access occurs here.
use crate::{DbError, PgConnection};
use sqlx::Connection;
use yhotos_core::PhotoId;

/// Resolves a live photo within 50 km, favoring populated cities, atomically.
/// Repeated calls reuse the `GeoNames` identity. Missing GPS is a no-op; a valid
/// coordinate with no nearby city clears its place. Locks fence metadata edits.
///
/// # Errors
/// Returns database failures without exposing coordinates or stored metadata.
pub async fn reverse(conn: &mut PgConnection, photo: PhotoId) -> Result<(), DbError> {
    let mut tx = conn.begin().await?;
    let row = sqlx::query!(
        "SELECT gps_lat,gps_lon FROM photos WHERE id=$1 AND deleted_at IS NULL AND NOT place_overridden FOR UPDATE",
        photo as _
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = row
        && let (Some(lat), Some(lon)) = (row.gps_lat, row.gps_lon)
    {
        // Conservative spherical cap envelope, including the poles and date line.
        let dy = 0.45_f64;
        let dx = if lat.abs() + dy >= 90.0 {
            180.0
        } else {
            (dy / (lat.abs() + dy).to_radians().cos()).min(180.0)
        };
        let city = sqlx::query!(
            r#"WITH candidates AS (
          SELECT g.*, 6371.0088 * acos(least(1.0,greatest(-1.0,
            sin(radians($1))*sin(radians(lat)) +
            cos(radians($1))*cos(radians(lat))*cos(radians(lon-$2))))) AS distance
          FROM gazetteer g WHERE
            point <@ box(point($2-$3,$1-0.45),point($2+$3,$1+0.45)) OR
            point <@ box(point($2-$3-360,$1-0.45),point($2+$3-360,$1+0.45)) OR
            point <@ box(point($2-$3+360,$1-0.45),point($2+$3+360,$1+0.45))
        ) SELECT geonameid,name,admin1,country,lat,lon FROM candidates
          WHERE distance <= 50.0 ORDER BY distance / ln(population::float8+10),geonameid LIMIT 1"#,
            lat,
            lon,
            dx
        )
        .fetch_optional(&mut *tx)
        .await?;
        let place = if let Some(city) = city {
            Some(sqlx::query!(r#"INSERT INTO places(id,geonameid,name,admin1,country,lat,lon)
              VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(geonameid) DO UPDATE SET
              name=excluded.name,admin1=excluded.admin1,country=excluded.country,lat=excluded.lat,lon=excluded.lon
              RETURNING id"#, uuid::Uuid::now_v7(),city.geonameid,city.name,city.admin1,city.country,city.lat,city.lon)
                .fetch_one(&mut *tx).await?.id)
        } else {
            None
        };
        sqlx::query!(
            "UPDATE photos SET place_id=$2 WHERE id=$1 AND place_id IS DISTINCT FROM $2",
            photo as _,
            place
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// One visibility-filtered place page using the public album grid's photo type.
#[derive(Debug, serde::Deserialize)]
pub struct PlacePage {
    /// Stored city name, escaped when rendered.
    pub name: String,
    /// Stored region, escaped when rendered.
    pub admin1: Option<String>,
    /// Stored country, escaped when rendered.
    pub country: String,
    /// Complete count, including photos outside the current page.
    pub total: u64,
    /// At most sixty visible photos with responsive derivatives.
    pub photos: Vec<crate::photos::PagePhoto>,
}
/// Returns a place only if it contains visible photos, in one SQL round trip.
///
/// # Errors
/// Returns invalid stored metadata or SQL failures. Unavailable pages return `None`.
pub async fn page(
    conn: &mut PgConnection,
    principal: &yhotos_core::access::Principal<'_>,
    place: uuid::Uuid,
    number: u32,
) -> Result<Option<PlacePage>, DbError> {
    page_for(
        conn,
        &crate::access_sql::visible_photos(principal),
        place,
        number,
    )
    .await
}
/// Reads a page visible to the union of independently verified principals in one query.
///
/// # Errors
/// Returns invalid stored metadata or database failures; unavailable pages return `None`.
pub async fn page_for(
    conn: &mut PgConnection,
    p: &crate::access_sql::Bindings,
    place: uuid::Uuid,
    number: u32,
) -> Result<Option<PlacePage>, DbError> {
    if number == 0 {
        return Ok(None);
    }
    let offset = i64::from(number - 1) * 60;
    let row = crate::visible_page_query!(r#",
      selected AS MATERIALIZED (
        SELECT p.id,coalesce(p.taken_at,p.posted_at) AS date FROM visible_photos p WHERE place_id=$6
      ), slice AS (
        SELECT p.* FROM (
          SELECT id FROM selected ORDER BY date DESC,id LIMIT 60 OFFSET $7
        ) s JOIN visible_photos p ON p.id=s.id
      )
      SELECT jsonb_build_object('name',l.name,'admin1',l.admin1,'country',l.country,
        'total',(SELECT count(*) FROM selected),
        'photos',coalesce((SELECT jsonb_agg(to_jsonb(p)||jsonb_build_object(
          'thumbhash',encode(p.placeholder,'base64'),
          'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object(
            'variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height)
            ORDER BY d.width,d.format,d.hdr) FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb))
          ORDER BY coalesce(p.taken_at,p.posted_at) DESC,p.id) FROM slice p),'[]'::jsonb)) AS "payload!"
      FROM places l WHERE l.id=$6 AND EXISTS(SELECT FROM slice)
    "#, p, place, offset).fetch_optional(conn).await?;
    row.map(|r| serde_json::from_value(r.payload).map_err(|_| DbError::InvalidInput))
        .transpose()
}
