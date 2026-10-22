//! Durable tus state. Callers hold the row lock through filesystem writes and fsync.
use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{AlbumId, UserId};

/// Owned upload state; temporary identifiers must be validated by storage before use.
#[derive(Debug)]
pub struct Upload {
    /// Opaque upload row identifier.
    pub id: Uuid,
    /// Authenticated creator.
    pub user_id: UserId,
    /// Destination album.
    pub album_id: AlbumId,
    /// Sanitized display name, never a filesystem path.
    pub filename: String,
    /// Declared complete length.
    pub declared_bytes: i64,
    /// Last committed, fsynced offset.
    pub received_bytes: i64,
    /// Advisory MIME, never authoritative.
    pub mime_hint: String,
    /// UUID v4 only, never a path.
    pub tmp_path: String,
    /// Validated creation header, echoed exactly for tus resumption.
    pub metadata: String,
    /// Fixed expiry, not extended by reads.
    pub expires_at: OffsetDateTime,
    /// Creation time.
    pub created_at: OffsetDateTime,
}
/// Creates a bounded upload; locks its owner to enforce a 100-upload quota across processes.
/// # Errors
/// Returns invalid input for unavailable albums or exceeded quota, or database errors.
#[allow(clippy::too_many_arguments)] // Mirrors the bounded creation fields and their owner.
pub async fn create(
    conn: &mut PgConnection,
    user: UserId,
    album: AlbumId,
    filename: &str,
    bytes: i64,
    mime: &str,
    temp: &str,
    metadata: &str,
) -> Result<Upload, DbError> {
    sqlx::query!("SELECT id FROM users WHERE id=$1 FOR UPDATE", user.0)
        .fetch_one(&mut *conn)
        .await?;
    let count = sqlx::query_scalar!("SELECT count(*) FROM uploads WHERE user_id=$1", user.0)
        .fetch_one(&mut *conn)
        .await?
        .unwrap_or(100);
    if count >= 100
        || sqlx::query!(
            "SELECT id FROM albums WHERE id=$1 AND deleted_at IS NULL",
            album.0
        )
        .fetch_optional(&mut *conn)
        .await?
        .is_none()
    {
        return Err(DbError::InvalidInput);
    }
    Ok(sqlx::query_as!(Upload,r#"INSERT INTO uploads(id,user_id,album_id,filename,declared_bytes,mime_hint,tmp_path,metadata) VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id,user_id AS "user_id: _",album_id AS "album_id: _",filename,declared_bytes,received_bytes,mime_hint,tmp_path,metadata,expires_at,created_at"#,Uuid::now_v7(),user.0,album.0,filename,bytes,mime,temp,metadata).fetch_one(conn).await?)
}
/// Locks only an unexpired upload owned by the authenticated user.
/// # Errors
/// Returns connection errors; missing, expired and other-user uploads all return None.
pub async fn lock(
    conn: &mut PgConnection,
    id: Uuid,
    user: UserId,
) -> Result<Option<Upload>, DbError> {
    Ok(sqlx::query_as!(Upload,r#"SELECT id,user_id AS "user_id: _",album_id AS "album_id: _",filename,declared_bytes,received_bytes,mime_hint,tmp_path,metadata,expires_at,created_at FROM uploads WHERE id=$1 AND user_id=$2 AND expires_at>now() FOR UPDATE"#,id,user.0).fetch_optional(conn).await?)
}
/// Locks an owned, unexpired upload and its live destination until transaction end.
/// The album share lock prevents both soft and hard deletion during PATCH or completion.
/// # Errors
/// Returns database errors; unavailable albums and uploads all return None.
pub async fn lock_for_write(
    conn: &mut PgConnection,
    id: Uuid,
    user: UserId,
) -> Result<Option<Upload>, DbError> {
    let Some(row) = lock(conn, id, user).await? else {
        return Ok(None);
    };
    lock_live_album(conn, row).await
}
/// Locks an owned, unexpired upload without waiting, then locks its live destination.
/// A missing row may have completed; callers must check `was_completed` before returning 404.
/// # Errors
/// Returns SQLSTATE 55P03 on upload lock contention, or other database errors.
pub async fn lock_for_patch(
    conn: &mut PgConnection,
    id: Uuid,
    user: UserId,
) -> Result<Option<Upload>, DbError> {
    let Some(row) = sqlx::query_as!(Upload,r#"SELECT id,user_id AS "user_id: _",album_id AS "album_id: _",filename,declared_bytes,received_bytes,mime_hint,tmp_path,metadata,expires_at,created_at FROM uploads WHERE id=$1 AND user_id=$2 AND expires_at>now() FOR UPDATE NOWAIT"#,id,user.0).fetch_optional(&mut *conn).await? else {
        return Ok(None);
    };
    lock_live_album(conn, row).await
}
async fn lock_live_album(conn: &mut PgConnection, row: Upload) -> Result<Option<Upload>, DbError> {
    if sqlx::query!(
        "SELECT id FROM albums WHERE id=$1 AND deleted_at IS NULL FOR SHARE",
        row.album_id.0
    )
    .fetch_optional(conn)
    .await?
    .is_none()
    {
        return Ok(None);
    }
    Ok(Some(row))
}
/// Advances the durable offset after the caller has synced the corresponding bytes.
/// # Errors
/// Returns constraint or database errors.
pub async fn advance(conn: &mut PgConnection, id: Uuid, offset: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE uploads SET received_bytes=$2 WHERE id=$1",
        id,
        offset
    )
    .execute(conn)
    .await?;
    Ok(())
}
/// Deletes a row while its caller holds the upload lock.
/// # Errors
/// Returns database errors.
pub async fn remove(conn: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM uploads WHERE id=$1", id)
        .execute(conn)
        .await?;
    Ok(())
}
/// Records successful completion in the same transaction that removes the upload.
/// The owner-only marker also covers duplicate uploads and expires after 24 hours.
/// # Errors
/// Returns database errors.
pub async fn record_completion(
    conn: &mut PgConnection,
    id: Uuid,
    user: UserId,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO completed_uploads(id,user_id) VALUES($1,$2)",
        id,
        user.0
    )
    .execute(conn)
    .await?;
    Ok(())
}
/// Recognizes only recent completions belonging to the authenticated upload owner.
/// # Errors
/// Returns database errors.
pub async fn was_completed(
    conn: &mut PgConnection,
    id: Uuid,
    user: UserId,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM completed_uploads WHERE id=$1 AND user_id=$2 AND expires_at>now()) AS "completed!""#,
        id,
        user.0
    )
    .fetch_one(conn)
    .await?)
}
/// Removes completion markers after their fixed retention period.
/// # Errors
/// Returns database errors.
pub async fn remove_expired_completions(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM completed_uploads WHERE expires_at<=now()")
        .execute(conn)
        .await?;
    Ok(())
}
/// Claims expired rows without racing active PATCH or termination transactions.
/// # Errors
/// Returns database errors.
pub async fn expired(conn: &mut PgConnection) -> Result<Vec<Upload>, DbError> {
    Ok(sqlx::query_as!(Upload,r#"SELECT id,user_id AS "user_id: _",album_id AS "album_id: _",filename,declared_bytes,received_bytes,mime_hint,tmp_path,metadata,expires_at,created_at FROM uploads WHERE expires_at<=now() ORDER BY expires_at LIMIT 100 FOR UPDATE SKIP LOCKED"#).fetch_all(conn).await?)
}
/// Reports whether an orphan candidate is still referenced by durable upload state.
/// # Errors
/// Returns database errors.
pub async fn references(conn: &mut PgConnection, temp: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM uploads WHERE tmp_path=$1)",
        temp
    )
    .fetch_one(conn)
    .await?
    .unwrap_or(true))
}
/// Serializes completion by content hash without requiring uniqueness on existing photo rows.
/// # Errors
/// Returns database errors.
pub async fn lock_hash(conn: &mut PgConnection, hash: &str) -> Result<(), DbError> {
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1,12))",
        hash
    )
    .execute(conn)
    .await?;
    Ok(())
}
/// Returns only the ingest status to the authenticated administrative caller.
/// # Errors
/// Returns database errors.
pub async fn status(conn: &mut PgConnection, short: &str) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT status::text AS "status!" FROM photos WHERE short_id=$1"#,
        short
    )
    .fetch_optional(conn)
    .await?)
}

/// Keeps a bounded PATCH transaction alive during streaming and host validation.
/// The HTTP absolute deadline still cancels and rolls back stalled operations.
/// # Errors
/// Rejects unbounded deadlines and returns database errors.
pub async fn patch_deadline(conn: &mut PgConnection, seconds: u32) -> Result<(), DbError> {
    if !(1..=720).contains(&seconds) {
        return Err(DbError::InvalidInput);
    }
    let value = format!("{seconds}s");
    sqlx::query!("SELECT set_config('idle_in_transaction_session_timeout',$1,true) AS idle,set_config('statement_timeout',$1,true) AS statement",&value).fetch_one(conn).await?;
    Ok(())
}
