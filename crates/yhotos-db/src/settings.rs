//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;

/// Fetches a setting without substituting a default.
///
/// # Errors
/// Returns database failures.
pub async fn get(conn: &mut PgConnection, key: &str) -> Result<Option<serde_json::Value>, DbError> {
    Ok(
        sqlx::query_scalar!("SELECT value FROM settings WHERE key=$1", key)
            .fetch_optional(conn)
            .await?,
    )
}

/// Atomically creates or replaces a JSON setting by its exact key.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn set(
    conn: &mut PgConnection,
    key: &str,
    value: &serde_json::Value,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"INSERT INTO settings(key,value) VALUES ($1,$2) ON CONFLICT (key) DO UPDATE SET value=excluded.value"#, key, value).execute(conn).await?.rows_affected())
}
