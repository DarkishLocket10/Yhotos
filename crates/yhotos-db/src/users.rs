//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use yhotos_core::UserId;

/// Account state including a password hash; Debug exposes only the typed user id.
#[derive(Clone)]
pub struct UserRow {
    /// Stored id.
    pub id: UserId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored username.
    pub username: String,
    /// Stored password hash.
    pub password_hash: String,
    /// Stored must change password.
    pub must_change_password: bool,
    /// Stored is admin.
    pub is_admin: bool,
    /// Stored disabled at.
    pub disabled_at: Option<OffsetDateTime>,
    /// Failed MFA attempts across every challenge and network.
    pub mfa_failures: i32,
    /// Account-wide MFA backoff deadline.
    pub mfa_locked_until: Option<OffsetDateTime>,
    /// Stored last login at.
    pub last_login_at: Option<OffsetDateTime>,
}

impl std::fmt::Debug for UserRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserRow")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Creates a non-admin account with a `UUIDv7` and a case-insensitive unique username.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn create(
    conn: &mut PgConnection,
    username: &str,
    password_hash: &secrecy::SecretString,
    must_change: bool,
) -> Result<UserRow, DbError> {
    Ok(sqlx::query_as!(UserRow,r#"INSERT INTO users(id,username,password_hash,must_change_password) VALUES ($1,$2,$3,$4) RETURNING id as "id: _", created_at, updated_at, username::text AS "username!", password_hash, must_change_password, is_admin, disabled_at, last_login_at, mfa_failures, mfa_locked_until"#, UserId::new() as _, username, secrecy::ExposeSecret::expose_secret(password_hash), must_change).fetch_one(conn).await?)
}

/// Looks up account state by case-insensitive username, including disabled accounts.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_username(
    conn: &mut PgConnection,
    username: &str,
) -> Result<Option<UserRow>, DbError> {
    Ok(sqlx::query_as!(UserRow,r#"SELECT id as "id: _", created_at, updated_at, username::text AS "username!", password_hash, must_change_password, is_admin, disabled_at, last_login_at, mfa_failures, mfa_locked_until FROM users WHERE username=$1::citext"#, username).fetch_optional(conn).await?)
}

/// Fetches account state by its verified identifier, including disabled accounts.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_id(conn: &mut PgConnection, id: UserId) -> Result<Option<UserRow>, DbError> {
    Ok(sqlx::query_as!(UserRow,r#"SELECT id as "id: _", created_at, updated_at, username::text AS "username!", password_hash, must_change_password, is_admin, disabled_at, last_login_at, mfa_failures, mfa_locked_until FROM users WHERE id=$1"#, id as _).fetch_optional(conn).await?)
}

/// Changes the authenticated user password through the narrow SECURITY DEFINER seam; caller supplies a verified user id.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn set_password(
    conn: &mut PgConnection,
    id: UserId,
    password_hash: &secrecy::SecretString,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"SELECT user_set_own_password($1,$2)"#,
        id as _,
        secrecy::ExposeSecret::expose_secret(password_hash)
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Disables an account by recording the current UTC timestamp.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn disable(conn: &mut PgConnection, id: UserId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE users SET disabled_at=now() WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Clears the account disabled timestamp without changing credentials or privileges.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn enable(conn: &mut PgConnection, id: UserId) -> Result<u64, DbError> {
    Ok(
        sqlx::query!(r#"UPDATE users SET disabled_at=NULL WHERE id=$1"#, id as _)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

/// Records successful authentication through the SECURITY DEFINER seam; caller must verify identity first.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn touch_login(conn: &mut PgConnection, id: UserId) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"SELECT user_touch_login($1)"#, id as _)
        .execute(conn)
        .await?
        .rows_affected())
}

/// Creates the initial administrative account; only an administrative connection may call this.
///
/// # Errors
/// Rejects an invalid username or a database connection, permission or uniqueness failure.
pub async fn create_admin(
    conn: &mut PgConnection,
    username: &str,
    password_hash: &secrecy::SecretString,
) -> Result<(), DbError> {
    if username.is_empty() || username.len() > 80 || username.chars().any(char::is_control) {
        return Err(DbError::InvalidInput);
    }
    sqlx::query!("INSERT INTO users(id,username,password_hash,must_change_password,is_admin) VALUES($1,$2,$3,true,true)",UserId::new() as _,username,secrecy::ExposeSecret::expose_secret(password_hash)).execute(conn).await?;
    Ok(())
}

/// Serializes credential changes and login finalization for the same account.
/// Call before re-reading credentials, and retain the transaction until commit.
///
/// # Errors
/// Returns a database connection or lock timeout failure.
pub async fn lock_auth(conn: &mut PgConnection, id: UserId) -> Result<(), DbError> {
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text,0))",
        id as _
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Persists account-wide MFA backoff while the caller holds `lock_auth`.
/// Only completed MFA may clear this state.
///
/// # Errors
/// Returns a database connection, permission or constraint failure.
pub async fn set_mfa_lockout(
    conn: &mut PgConnection,
    id: UserId,
    failures: i32,
    deadline: Option<OffsetDateTime>,
) -> Result<(), DbError> {
    sqlx::query!(
        "SELECT user_set_mfa_lockout($1,$2,$3)",
        id as _,
        failures,
        deadline
    )
    .execute(conn)
    .await?;
    Ok(())
}
