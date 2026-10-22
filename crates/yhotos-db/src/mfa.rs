//! Narrow MFA functions preserve the public role's lack of direct table access.
use crate::DbError;
use sqlx::PgConnection;
use uuid::Uuid;
use yhotos_core::UserId;

/// An enrolled factor; secret ciphertext remains bound to its row identifier.
pub struct Method {
    /// Method identifier.
    pub id: Uuid,
    /// Factor kind, including the reserved unsupported email kind.
    pub kind: String,
    /// Sealed TOTP secret.
    pub secret_enc: Option<Vec<u8>>,
    /// Globally unique `WebAuthn` identifier.
    pub credential_id: Option<Vec<u8>>,
    /// Serialized verified passkey.
    pub public_key: Option<Vec<u8>>,
    /// Latest persisted signature count.
    pub sign_count: i64,
    /// Display label.
    pub label: String,
    /// Verified relying-party hostname; unbound legacy passkeys are unusable.
    pub rp_id: Option<String>,
    /// Latest consumed TOTP time step.
    pub last_used_step: Option<i64>,
}
/// Fetches only enabled factors for the authenticated account.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn list(c: &mut PgConnection, user: UserId) -> Result<Vec<Method>, DbError> {
    Ok(sqlx::query_as!(Method,r#"SELECT id as "id!",kind as "kind!",secret_enc,credential_id,public_key,sign_count as "sign_count!",label as "label!",last_used_step,rp_id FROM user_mfa_list($1)"#,user as _).fetch_all(c).await?)
}
/// Stores a verified factor through the least-privilege function seam.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn add(c: &mut PgConnection, user: UserId, m: &Method) -> Result<(), DbError> {
    sqlx::query!(
        "SELECT user_mfa_add($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        m.id,
        user as _,
        m.kind,
        m.secret_enc,
        m.credential_id,
        m.public_key,
        m.sign_count,
        m.last_used_step,
        m.rp_id
    )
    .execute(c)
    .await?;
    Ok(())
}
/// Claims a TOTP counter using a monotonic compare-and-set, rejecting concurrent replay.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn use_totp(
    c: &mut PgConnection,
    user: UserId,
    id: Uuid,
    step: i64,
) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar!("SELECT user_mfa_use($1,$2,$3)", id, user as _, step)
            .fetch_one(c)
            .await?
            .unwrap_or(false),
    )
}
/// Claims a signature counter and updated credential in one compare-and-set.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn use_passkey(
    c: &mut PgConnection,
    user: UserId,
    m: &Method,
    new_count: i64,
    key: &[u8],
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT user_passkey_use($1,$2,$3,$4,$5)",
        m.id,
        user as _,
        m.sign_count,
        new_count,
        key
    )
    .fetch_one(c)
    .await?
    .unwrap_or(false))
}
/// Removes only the account's own factor after a fresh password verification.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn remove(
    c: &mut PgConnection,
    user: UserId,
    id: Uuid,
    session: Uuid,
) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar!("SELECT user_mfa_remove($1,$2,$3)", id, user as _, session)
            .fetch_one(c)
            .await?
            .unwrap_or(false),
    )
}
