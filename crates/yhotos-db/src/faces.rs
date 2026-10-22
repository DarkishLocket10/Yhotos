//! Administrative biometric repository; absent from public builds.
use crate::DbError;
use sqlx::PgConnection;
use uuid::Uuid;

/// A private person summary, excluding hidden identities.
#[derive(serde::Serialize)]
pub struct Person {
    /// Internal administrative identity.
    pub id: Uuid,
    /// Untrusted, optional display name.
    pub name: Option<String>,
    /// Face count in non-deleted photos.
    pub count: i64,
}
/// Lists up to two hundred visible private clusters.
///
/// # Errors
/// Returns database failures.
pub async fn people(conn: &mut PgConnection) -> Result<Vec<Person>, DbError> {
    let rows=sqlx::query!(r#"SELECT p.id,p.name,count(f.id) AS "count!" FROM persons p
        JOIN faces f ON f.person_id=p.id JOIN photos photo ON photo.id=f.photo_id
        WHERE NOT p.hidden AND photo.deleted_at IS NULL GROUP BY p.id ORDER BY p.name NULLS LAST,p.id LIMIT 200"#).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|r| Person {
            id: r.id,
            name: r.name,
            count: r.count,
        })
        .collect())
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
/// Returns up to sixty ready private photos for a non-hidden cluster.
///
/// # Errors
/// Returns database failures.
pub async fn photos(
    conn: &mut PgConnection,
    person: Uuid,
) -> Result<Vec<crate::photos::PagePhoto>, DbError> {
    find(conn, Some(person), None).await
}

/// Finds a named person only on the admin listener; hidden people never match.
///
/// # Errors
/// Returns invalid names or database failures.
pub async fn named_photos(
    conn: &mut PgConnection,
    name: &str,
) -> Result<Vec<crate::photos::PagePhoto>, DbError> {
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(DbError::InvalidInput);
    }
    find(conn, None, Some(name)).await
}
async fn find(
    conn: &mut PgConnection,
    id: Option<Uuid>,
    name: Option<&str>,
) -> Result<Vec<crate::photos::PagePhoto>, DbError> {
    let rows=sqlx::query!(r#"SELECT to_jsonb(p) || jsonb_build_object('thumbhash',encode(p.placeholder,'base64'),
      'derivatives',coalesce((SELECT jsonb_agg(jsonb_build_object('variant',d.variant,'format',d.format,
      'hdr',d.hdr,'hash',d.hash,'w',d.width,'h',d.height) ORDER BY d.width,d.format,d.hdr)
      FROM derivatives d WHERE d.photo_id=p.id),'[]'::jsonb)) AS "payload!"
      FROM photos p WHERE p.deleted_at IS NULL AND p.status='ready' AND EXISTS (
        SELECT FROM faces f JOIN persons person ON person.id=f.person_id
        WHERE f.photo_id=p.id AND NOT person.hidden AND (person.id=$1 OR lower(person.name)=lower($2)))
      ORDER BY p.id LIMIT 60"#,id,name).fetch_all(conn).await?;
    rows.into_iter()
        .map(|r| serde_json::from_value(r.payload).map_err(|_| DbError::InvalidInput))
        .collect()
}
