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
        "SELECT gps_lat,gps_lon FROM photos WHERE id=$1 AND deleted_at IS NULL FOR UPDATE",
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
