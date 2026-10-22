//! Session and single-use ceremony storage. Identity and audience are verified by HTTP.
use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{GroupId, UserId};

/// A session snapshot, containing only hashed bearer material.
#[derive(Clone)]
pub struct Session {
    /// Session identifier.
    pub id: Uuid,
    /// Authenticated account.
    pub user_id: UserId,
    /// Listener audience.
    pub admin: bool,
    /// Absolute start.
    pub created_at: OffsetDateTime,
    /// Coalesced activity timestamp.
    pub last_seen_at: OffsetDateTime,
    /// Hard expiry.
    pub expires_at: OffsetDateTime,
    /// Last password verification.
    pub reauth_at: Option<OffsetDateTime>,
    /// Set only after this session completes MFA or enrollment.
    pub mfa_at: Option<OffsetDateTime>,
    /// Keyed network fingerprint.
    pub ip_hash: Vec<u8>,
    /// Coarse browser category.
    pub ua_class: String,
    /// Storage digest, never bearer text.
    pub token_hash: Vec<u8>,
}
/// Loads one session by a fixed-size token digest.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn get(c: &mut PgConnection, hash: &[u8]) -> Result<Option<Session>, DbError> {
    Ok(sqlx::query_as!(Session,r#"SELECT id,user_id as "user_id: _",admin,created_at,last_seen_at,expires_at,reauth_at,mfa_at,ip_hash,ua_class,token_hash FROM sessions WHERE token_hash=$1"#, hash).fetch_optional(c).await?)
}
/// Inserts a freshly generated bearer digest with explicit audience and lifetime.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn create(c: &mut PgConnection, s: &Session) -> Result<(), DbError> {
    sqlx::query!("INSERT INTO sessions(id,user_id,token_hash,admin,created_at,last_seen_at,expires_at,reauth_at,mfa_at,ip_hash,ua_class) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",s.id,s.user_id as _,s.token_hash,s.admin,s.created_at,s.last_seen_at,s.expires_at,s.reauth_at,s.mfa_at,s.ip_hash,s.ua_class).execute(c).await?;
    Ok(())
}
/// Lists the account's sessions without exposing bearer tokens.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn list(c: &mut PgConnection, user: UserId) -> Result<Vec<Session>, DbError> {
    Ok(sqlx::query_as!(Session,r#"SELECT id,user_id as "user_id: _",admin,created_at,last_seen_at,expires_at,reauth_at,mfa_at,ip_hash,ua_class,token_hash FROM sessions WHERE user_id=$1 ORDER BY created_at DESC"#,user as _).fetch_all(c).await?)
}
/// Deletes only a session owned by the authenticated account, returning its cache key.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn revoke(
    c: &mut PgConnection,
    user: UserId,
    id: Uuid,
) -> Result<Option<Vec<u8>>, DbError> {
    Ok(sqlx::query_scalar!(
        "DELETE FROM sessions WHERE user_id=$1 AND id=$2 RETURNING token_hash",
        user as _,
        id
    )
    .fetch_optional(c)
    .await?)
}
/// Revokes every session during password rotation, returning cache keys.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn revoke_all(c: &mut PgConnection, user: UserId) -> Result<Vec<Vec<u8>>, DbError> {
    Ok(sqlx::query_scalar!(
        "DELETE FROM sessions WHERE user_id=$1 RETURNING token_hash",
        user as _
    )
    .fetch_all(c)
    .await?)
}
/// Performs at most one activity write per five minutes, including concurrent loads.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn touch(c: &mut PgConnection, id: Uuid) -> Result<bool, DbError> {
    Ok(sqlx::query!("UPDATE sessions SET last_seen_at=clock_timestamp() WHERE id=$1 AND last_seen_at<=clock_timestamp()-interval '5 minutes'",id).execute(c).await?.rows_affected()>0)
}
/// Records a successful password check on the still-existing session.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn reauthenticate(c: &mut PgConnection, id: Uuid, user: UserId) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE sessions SET reauth_at=clock_timestamp() WHERE id=$1 AND user_id=$2",
        id,
        user as _
    )
    .execute(c)
    .await?;
    Ok(())
}
/// Loads group memberships for an authenticated account.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn groups(c: &mut PgConnection, user: UserId) -> Result<Vec<GroupId>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT group_id as "group_id: _" FROM user_groups WHERE user_id=$1"#,
        user as _
    )
    .fetch_all(c)
    .await?)
}
/// Server-side password-authenticated login, bounded by five attempts and five minutes.
pub struct PendingLogin {
    /// Binding for assertion ceremonies.
    pub id: Uuid,
    /// Authenticated user.
    pub user_id: UserId,
    /// Listener audience.
    pub admin: bool,
    /// Validated internal destination.
    pub next_path: Option<String>,
    /// Attempts already reserved.
    pub attempts: i32,
    /// Digest of the password hash version verified before the challenge.
    pub password_version: Option<Vec<u8>>,
    /// Time the password was actually verified, before MFA.
    pub password_at: OffsetDateTime,
}
/// Creates a password-authenticated challenge; cookies hold only the digest's bearer.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn pending_create(
    c: &mut PgConnection,
    id: Uuid,
    user: UserId,
    hash: &[u8],
    admin: bool,
    next: Option<&str>,
    password_version: &[u8],
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO pending_login(id,user_id,token_hash,admin,next_path,password_version) VALUES($1,$2,$3,$4,$5,$6)",
        id,
        user as _,
        hash,
        admin,
        next,
        password_version
    )
    .execute(c)
    .await?;
    Ok(())
}
/// Locks and fetches an unexpired pending login. Caller must hold a transaction.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn pending_get(
    c: &mut PgConnection,
    hash: &[u8],
    admin: bool,
) -> Result<Option<PendingLogin>, DbError> {
    Ok(sqlx::query_as!(PendingLogin,r#"SELECT id,user_id as "user_id: _",admin,next_path,attempts,password_version,password_at FROM pending_login WHERE token_hash=$1 AND admin=$2 AND expires_at>clock_timestamp() AND attempts<5 FOR UPDATE"#,hash,admin).fetch_optional(c).await?)
}
/// Reserves one attempt atomically; deletes the fifth failed challenge.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn pending_failure(c: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE pending_login SET attempts=attempts+1 WHERE id=$1 AND attempts<5",
        id
    )
    .execute(&mut *c)
    .await?;
    sqlx::query!("DELETE FROM pending_login WHERE id=$1 AND attempts>=5", id)
        .execute(c)
        .await?;
    Ok(())
}
/// Consumes a successful challenge once while its row lock is held.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn pending_delete(c: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM pending_login WHERE id=$1", id)
        .execute(c)
        .await?;
    Ok(())
}
/// Replaces ceremony state only for its authenticated session or pending-login binding.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn ceremony_put(
    c: &mut PgConnection,
    binding: Uuid,
    user: UserId,
    kind: &str,
    state: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query!("INSERT INTO pending_webauthn(binding_id,user_id,kind,state) VALUES($1,$2,$3,$4) ON CONFLICT(binding_id,kind) DO UPDATE SET state=EXCLUDED.state,user_id=EXCLUDED.user_id,expires_at=clock_timestamp()+interval '5 minutes'",binding,user as _,kind,state).execute(c).await?;
    Ok(())
}
/// Atomically consumes unexpired state; failure cannot replay the same ceremony.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn ceremony_take(
    c: &mut PgConnection,
    binding: Uuid,
    user: UserId,
    kind: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    Ok(sqlx::query_scalar!("DELETE FROM pending_webauthn WHERE binding_id=$1 AND user_id=$2 AND kind=$3 AND expires_at>clock_timestamp() RETURNING state",binding,user as _,kind).fetch_optional(c).await?)
}
/// Reclaims expired challenge and lockout rows without running on every request.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn cleanup(c: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM pending_login WHERE expires_at<clock_timestamp()")
        .execute(&mut *c)
        .await?;
    sqlx::query!("DELETE FROM pending_webauthn WHERE expires_at<clock_timestamp()")
        .execute(&mut *c)
        .await?;
    sqlx::query!("DELETE FROM auth_lockout WHERE updated_at<clock_timestamp()-interval '1 day'")
        .execute(c)
        .await?;
    Ok(())
}
/// Locks persistent rate-limit state; callers acquire keys in sorted order.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn lockout_get(c: &mut PgConnection, key: &[u8]) -> Result<serde_json::Value, DbError> {
    sqlx::query!(
        "INSERT INTO auth_lockout(key,state) VALUES($1,'{}'::jsonb) ON CONFLICT DO NOTHING",
        key
    )
    .execute(&mut *c)
    .await?;
    Ok(sqlx::query_scalar!(
        "SELECT state FROM auth_lockout WHERE key=$1 FOR UPDATE",
        key
    )
    .fetch_one(c)
    .await?)
}
/// Saves lockout state in the same transaction that locked it.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn lockout_set(
    c: &mut PgConnection,
    key: &[u8],
    state: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE auth_lockout SET state=$2,updated_at=clock_timestamp() WHERE key=$1",
        key,
        state
    )
    .execute(c)
    .await?;
    Ok(())
}

/// One-query session context. Authorization flags come only from the joined account.
#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent account flags mirror the database schema"
)]
pub struct Loaded {
    /// Session timestamps and audience.
    pub session: Session,
    /// Temporary password gate.
    pub must_change_password: bool,
    /// Account privilege, never a cookie claim.
    pub is_admin: bool,
    /// Disabled accounts cannot authenticate.
    pub disabled: bool,
    /// Whether the account still has an enabled MFA method.
    pub has_mfa: bool,
    /// Current groups from the database.
    pub groups: Vec<GroupId>,
}
/// Loads session, account, factors and groups in one query; conditionally coalesces activity writes.
///
/// # Errors
/// Returns a database connection, statement or constraint failure.
pub async fn load(c: &mut PgConnection, hash: &[u8]) -> Result<Option<Loaded>, DbError> {
    let row=sqlx::query!(r#"WITH touched AS (
 UPDATE sessions SET last_seen_at=clock_timestamp() WHERE token_hash=$1
 AND last_seen_at<=clock_timestamp()-interval '5 minutes'
 AND expires_at>clock_timestamp() AND created_at<=last_seen_at
 AND last_seen_at>clock_timestamp()-CASE WHEN (SELECT is_admin FROM users WHERE id=sessions.user_id) THEN interval '12 hours' ELSE interval '14 days' END
 AND created_at>clock_timestamp()-CASE WHEN (SELECT is_admin FROM users WHERE id=sessions.user_id) THEN interval '7 days' ELSE interval '90 days' END
 RETURNING last_seen_at
)
SELECT s.id,s.user_id,s.admin,s.created_at,COALESCE((SELECT last_seen_at FROM touched),s.last_seen_at) as "last_seen_at!",s.expires_at,s.reauth_at,s.mfa_at,s.ip_hash,s.ua_class,s.token_hash,
u.must_change_password,u.is_admin,(u.disabled_at IS NOT NULL) as "disabled!",
EXISTS(SELECT FROM user_mfa_list(u.id) WHERE kind IN ('totp','passkey')) as "has_mfa!",
ARRAY(SELECT group_id FROM user_groups WHERE user_id=u.id) as "groups!"
FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=$1"#,hash).fetch_optional(c).await?;
    Ok(row.map(|r| Loaded {
        session: Session {
            id: r.id,
            user_id: UserId(r.user_id),
            admin: r.admin,
            created_at: r.created_at,
            last_seen_at: r.last_seen_at,
            expires_at: r.expires_at,
            reauth_at: r.reauth_at,
            mfa_at: r.mfa_at,
            ip_hash: r.ip_hash,
            ua_class: r.ua_class,
            token_hash: r.token_hash,
        },
        must_change_password: r.must_change_password,
        is_admin: r.is_admin,
        disabled: r.disabled,
        has_mfa: r.has_mfa,
        groups: r.groups.into_iter().map(GroupId).collect(),
    }))
}

/// Revokes password-only sessions when enrollment establishes MFA on another bearer.
/// Caller holds the account authentication lock and rotates the enrolling session.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn revoke_setup(c: &mut PgConnection, user: UserId) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar!(
        "DELETE FROM sessions WHERE user_id=$1 AND mfa_at IS NULL RETURNING id",
        user as _
    )
    .fetch_all(c)
    .await?)
}

/// A validated cache eviction received from PostgreSQL, never bearer material.
pub enum Invalidation {
    /// One session changed or was deleted.
    Session(Uuid),
    /// Account authentication or privilege changed.
    User(UserId),
}
/// Dedicated LISTEN connection, independent of request pool capacity.
pub struct Invalidations(sqlx::postgres::PgListener);
impl Invalidations {
    /// Subscribes before requests can warm caches, using this pool's restricted login.
    ///
    /// # Errors
    /// Returns database connectivity or subscription errors.
    pub async fn connect(db: &crate::Db) -> Result<Self, DbError> {
        // A separate one-connection pool keeps LISTEN from occupying request slots.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(db.pool().connect_options().as_ref().clone())
            .await?;
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
        listener.eager_reconnect(false);
        listener.listen("session_invalidate").await?;
        Ok(Self(listener))
    }
    /// Receives one bounded, validated row id. Disconnects and malformed messages
    /// return an error so callers discard caches before resubscribing.
    ///
    /// # Errors
    /// Returns a connection error or invalid notification payload.
    pub async fn recv(&mut self) -> Result<Invalidation, DbError> {
        let notification = self.0.try_recv().await?.ok_or(DbError::InvalidInput)?;
        let (kind, id) = notification
            .payload()
            .split_once(':')
            .ok_or(DbError::InvalidInput)?;
        let id = Uuid::parse_str(id).map_err(|_| DbError::InvalidInput)?;
        match kind {
            "session" => Ok(Invalidation::Session(id)),
            "user" => Ok(Invalidation::User(UserId(id))),
            _ => Err(DbError::InvalidInput),
        }
    }
}
