//! Typed repository operations; authorization belongs to the caller.

use crate::DbError;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use yhotos_core::{AlbumId, PhotoId, ShareLinkId, UserId};

/// Closed values accepted by the database enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "target_kind", rename_all = "lowercase")]
pub enum TargetKind {
    /// Album value.
    Album,
    /// Photo value.
    Photo,
}

/// A database row with validated typed identifiers and UTC timestamps.
#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the row mirrors four independent capability columns"
)]
pub struct ShareLinkRow {
    /// Stored id.
    pub id: ShareLinkId,
    /// Stored created at.
    pub created_at: OffsetDateTime,
    /// Stored updated at.
    pub updated_at: OffsetDateTime,
    /// Stored token hash.
    pub token_hash: Vec<u8>,
    /// Stored target kind.
    pub target_kind: TargetKind,
    /// Stored target id.
    pub target_id: Uuid,
    /// Stored passphrase hash.
    pub passphrase_hash: Option<String>,
    /// Stored expires at.
    pub expires_at: Option<OffsetDateTime>,
    /// Stored allow view.
    pub allow_view: bool,
    /// Stored allow download.
    pub allow_download: bool,
    /// Stored allow like.
    pub allow_like: bool,
    /// Stored allow flag.
    pub allow_flag: bool,
    /// Stored revoked at.
    pub revoked_at: Option<OffsetDateTime>,
    /// Stored created by.
    pub created_by: UserId,
}

impl std::fmt::Debug for ShareLinkRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareLinkRow")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl From<ShareLinkRow> for yhotos_core::access::ShareTarget {
    fn from(row: ShareLinkRow) -> Self {
        match row.target_kind {
            TargetKind::Album => Self::Album(AlbumId(row.target_id)),
            TargetKind::Photo => Self::Photo(PhotoId(row.target_id)),
        }
    }
}
/// Link capabilities, each checked by the caller per action.
#[derive(Clone, Copy, Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four capabilities can be granted independently"
)]
pub struct LinkFlags {
    /// Allows viewing the target.
    pub view: bool,
    /// Allows downloading the target.
    pub download: bool,
    /// Allows attributing likes to the link.
    pub like: bool,
    /// Allows attributing flags to the link.
    pub flag: bool,
}
/// Validated link creation values; only hashes cross this boundary.
pub struct NewShareLink {
    /// A hash of a generated 256-bit token, never the raw token.
    pub token_hash: [u8; 32],
    /// Exact scope authorized by the link.
    pub target: yhotos_core::access::ShareTarget,
    /// Optional password hash, never a plaintext passphrase.
    pub passphrase_hash: Option<secrecy::SecretString>,
    /// UTC expiration checked by the caller.
    pub expires_at: Option<OffsetDateTime>,
    /// Initial capabilities.
    pub flags: LinkFlags,
    /// Verified administrator identity.
    pub created_by: UserId,
}

/// Stores only a token hash and optional passphrase hash with an exact typed target.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn create(conn: &mut PgConnection, link: &NewShareLink) -> Result<ShareLinkRow, DbError> {
    let (kind, target_id) = match link.target {
        yhotos_core::access::ShareTarget::Album(id) => (TargetKind::Album, id.0),
        yhotos_core::access::ShareTarget::Photo(id) => (TargetKind::Photo, id.0),
    };
    let passphrase_hash = link
        .passphrase_hash
        .as_ref()
        .map(secrecy::ExposeSecret::expose_secret);
    Ok(sqlx::query_as!(ShareLinkRow,r#"INSERT INTO share_links(id,token_hash,target_kind,target_id,passphrase_hash,expires_at,allow_view,allow_download,allow_like,allow_flag,created_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id as "id: _", created_at, updated_at, token_hash, target_kind as "target_kind: _", target_id, passphrase_hash, expires_at, allow_view, allow_download, allow_like, allow_flag, revoked_at, created_by as "created_by: _""#, ShareLinkId::new() as _, &link.token_hash[..], kind as _, target_id, passphrase_hash, link.expires_at, link.flags.view, link.flags.download, link.flags.like, link.flags.flag, link.created_by as _).fetch_one(conn).await?)
}

/// Returns only non-revoked links; caller must check expiry, passphrase and capabilities.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn get_by_token_hash(
    conn: &mut PgConnection,
    hash: &[u8; 32],
) -> Result<Option<ShareLinkRow>, DbError> {
    Ok(sqlx::query_as!(ShareLinkRow,r#"SELECT id as "id: _", created_at, updated_at, token_hash, target_kind as "target_kind: _", target_id, passphrase_hash, expires_at, allow_view, allow_download, allow_like, allow_flag, revoked_at, created_by as "created_by: _" FROM share_links WHERE token_hash=$1 AND revoked_at IS NULL"#, &hash[..]).fetch_optional(conn).await?)
}

/// Replaces the four independent capabilities without changing the link target.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn update_flags(
    conn: &mut PgConnection,
    id: ShareLinkId,
    flags: LinkFlags,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"UPDATE share_links SET allow_view=$2,allow_download=$3,allow_like=$4,allow_flag=$5 WHERE id=$1"#, id as _, flags.view, flags.download, flags.like, flags.flag).execute(conn).await?.rows_affected())
}

/// Sets a UTC expiration or clears it for a non-expiring link.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn set_expiry(
    conn: &mut PgConnection,
    id: ShareLinkId,
    expires_at: Option<OffsetDateTime>,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"UPDATE share_links SET expires_at=$2 WHERE id=$1"#,
        id as _,
        expires_at
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Marks a link revoked without removing its interactions or statistics.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn revoke(conn: &mut PgConnection, id: ShareLinkId) -> Result<u64, DbError> {
    Ok(sqlx::query!(
        r#"UPDATE share_links SET revoked_at=now() WHERE id=$1"#,
        id as _
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Records coarse location and an already hashed visitor identifier.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn record_open(
    conn: &mut PgConnection,
    id: ShareLinkId,
    visitor_hash: &[u8],
    country: Option<&str>,
    city: Option<&str>,
) -> Result<u64, DbError> {
    Ok(sqlx::query!(r#"INSERT INTO share_link_opens(id,share_link_id,visitor_hash,country,city) VALUES ($1,$2,$3,$4,$5)"#, Uuid::now_v7(), id as _, visitor_hash, country, city).execute(conn).await?.rows_affected())
}

/// Aggregate counts and UTC open timestamps; empty links have zero counts and no timestamps.
#[derive(Clone, Debug)]
pub struct ShareLinkStats {
    /// Stored opens.
    pub opens: i64,
    /// Stored unique visitors.
    pub unique_visitors: i64,
    /// Stored first open.
    pub first_open: Option<OffsetDateTime>,
    /// Stored last open.
    pub last_open: Option<OffsetDateTime>,
    /// Stored countries.
    pub countries: serde_json::Value,
}

/// Aggregates opens, unique visitor hashes, countries and first/last open timestamps.
///
/// # Errors
/// Returns invalid input or a database constraint/connection failure.
pub async fn stats(conn: &mut PgConnection, id: ShareLinkId) -> Result<ShareLinkStats, DbError> {
    Ok(sqlx::query_as!(ShareLinkStats,r#"SELECT count(*) AS "opens!", count(DISTINCT visitor_hash) AS "unique_visitors!", min(opened_at) AS first_open, max(opened_at) AS last_open, coalesce((SELECT jsonb_object_agg(country,opens) FROM (SELECT country,count(*) AS opens FROM share_link_opens WHERE share_link_id=$1 AND country IS NOT NULL GROUP BY country) c),'{}'::jsonb) AS "countries!" FROM share_link_opens WHERE share_link_id=$1"#, id as _).fetch_one(conn).await?)
}
