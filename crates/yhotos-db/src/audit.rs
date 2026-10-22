//! Append-only security outcomes. Never pass passwords, bearers, or MFA state.
use crate::DbError;
use sqlx::PgConnection;
use uuid::Uuid;
use yhotos_core::UserId;
/// Persists a credential-free security outcome in the caller's transaction.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn write(
    c: &mut PgConnection,
    user: Option<UserId>,
    action: &str,
    target: Option<Uuid>,
    detail: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query!("INSERT INTO audit_log(id,user_id,action,target_kind,target_id,detail) VALUES($1,$2,$3,'auth',$4,$5)",Uuid::now_v7(),user as _,action,target,detail).execute(c).await?;
    Ok(())
}
