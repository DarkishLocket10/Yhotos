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

/// A link and its scoped display data, loaded in one statement.
pub struct Landing {
    /// Current link metadata, containing no bearer.
    pub link: ShareLinkRow,
    /// Untrusted stored display/access data; validated before rendering.
    pub photos: serde_json::Value,
    /// Target title; a missing or deleted target is concealed.
    pub title: String,
}

/// Fetches a live target and all candidate photos with complete access chains.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn landing(c: &mut PgConnection, hash: &[u8; 32]) -> Result<Option<Landing>, DbError> {
    let row = sqlx::query!(r#"WITH RECURSIVE chain AS (
 SELECT a.id,a.parent_id,a.visibility,a.deleted_at,ARRAY[a.id] AS visited
 FROM albums a JOIN share_links s ON s.target_kind='album' AND s.target_id=a.id WHERE s.token_hash=$1
 UNION ALL
 SELECT a.id,a.parent_id,a.visibility,a.deleted_at,c.visited || a.id
 FROM chain c JOIN albums a ON a.id=c.parent_id WHERE NOT a.id=ANY(c.visited)
)
 SELECT s.id,s.created_at,s.updated_at,s.token_hash,
 s.target_kind::text as "target_kind!",s.target_id,s.passphrase_hash,s.expires_at,
 s.allow_view,s.allow_download,s.allow_like,s.allow_flag,s.revoked_at,s.created_by,
 share_target_photos(s.target_kind,s.target_id) as "photos!",
 CASE s.target_kind WHEN 'album' THEN (SELECT title FROM albums WHERE id=s.target_id AND deleted_at IS NULL)
 ELSE (SELECT title FROM photos WHERE id=s.target_id AND deleted_at IS NULL) END as title
 FROM share_links s WHERE s.token_hash=$1 AND s.revoked_at IS NULL
 AND (s.target_kind='photo' OR (
  EXISTS(SELECT FROM chain WHERE parent_id IS NULL)
  AND NOT EXISTS(SELECT FROM chain WHERE deleted_at IS NOT NULL OR (id<>s.target_id AND visibility='restricted'))
 ))"#, &hash[..]).fetch_optional(c).await?;
    row.map(|r| {
        let target_kind = match r.target_kind.as_str() {
            "album" => TargetKind::Album,
            "photo" => TargetKind::Photo,
            _ => return Err(DbError::InvalidInput),
        };
        Ok(r.title.map(|title| Landing {
            title,
            photos: r.photos,
            link: ShareLinkRow {
                id: ShareLinkId(r.id),
                created_at: r.created_at,
                updated_at: r.updated_at,
                token_hash: r.token_hash,
                target_kind,
                target_id: r.target_id,
                passphrase_hash: r.passphrase_hash,
                expires_at: r.expires_at,
                allow_view: r.allow_view,
                allow_download: r.allow_download,
                allow_like: r.allow_like,
                allow_flag: r.allow_flag,
                revoked_at: r.revoked_at,
                created_by: UserId(r.created_by),
            },
        }))
    })
    .transpose()
    .map(Option::flatten)
}

/// Loads one photo with full access metadata, never authorizing by id alone.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn photo_context(
    c: &mut PgConnection,
    id: PhotoId,
) -> Result<serde_json::Value, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT share_target_photos('photo',$1) as "photos!""#,
        id as _
    )
    .fetch_one(c)
    .await?)
}

/// The hashed link-session binding and its current share-token digest.
#[derive(Clone)]
pub struct LinkSession {
    /// Session row id for rotation.
    pub id: Uuid,
    /// Exact link binding used to validate the cookie name.
    pub link_id: ShareLinkId,
    /// Link digest for cached scope resolution, never a URL token.
    pub link_hash: Vec<u8>,
    /// Hard session expiry, rechecked even on cached loads.
    pub expires_at: OffsetDateTime,
    /// Absolute start, checked even on cached loads.
    pub created_at: OffsetDateTime,
    /// Coalesced activity, checked even on cached loads.
    pub last_seen_at: OffsetDateTime,
}

/// Resolves only public link sessions; account and admin bearers never match.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn load_session(
    c: &mut PgConnection,
    hash: &[u8; 32],
) -> Result<Option<LinkSession>, DbError> {
    Ok(sqlx::query_as!(
        LinkSession,
        r#"WITH touched AS (
 UPDATE sessions SET last_seen_at=clock_timestamp() WHERE token_hash=$1 AND user_id IS NULL AND NOT admin
 AND last_seen_at<=clock_timestamp()-interval '5 minutes' AND expires_at>clock_timestamp()
 AND created_at<=last_seen_at AND created_at<=clock_timestamp()
 AND last_seen_at>clock_timestamp()-interval '14 days' AND created_at>clock_timestamp()-interval '90 days'
 RETURNING last_seen_at
)
 SELECT s.id,s.share_link_id as "link_id!: _",l.token_hash as link_hash,s.expires_at,s.created_at,
 coalesce((SELECT last_seen_at FROM touched),s.last_seen_at) as "last_seen_at!"
 FROM sessions s JOIN share_links l ON l.id=s.share_link_id
 WHERE s.token_hash=$1 AND s.user_id IS NULL AND NOT s.admin AND s.expires_at>clock_timestamp()
 AND s.created_at<=s.last_seen_at AND s.created_at<=clock_timestamp()
 AND s.last_seen_at>clock_timestamp()-interval '14 days'
 AND s.created_at>clock_timestamp()-interval '90 days'"#,
        &hash[..]
    )
    .fetch_optional(c)
    .await?)
}

/// Inserts a link-bound bearer only while the checked passphrase version is current.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn create_session(
    c: &mut PgConnection,
    link: &ShareLinkRow,
    hash: &[u8; 32],
    ip_hash: &[u8],
    ua: &str,
) -> Result<Option<LinkSession>, DbError> {
    Ok(sqlx::query_as!(LinkSession,r#"INSERT INTO sessions(id,user_id,share_link_id,token_hash,expires_at,ip_hash,ua_class)
 SELECT $1,NULL,id,$3,least(coalesce(expires_at,clock_timestamp()+interval '90 days'),clock_timestamp()+interval '90 days'),$4,$5
 FROM share_session_link($2,$6) WHERE id=$2 AND revoked_at IS NULL AND allow_view
 AND (expires_at IS NULL OR expires_at>clock_timestamp()) AND passphrase_hash IS NOT DISTINCT FROM $6
 RETURNING id,share_link_id as "link_id!: _",$7::bytea as "link_hash!",expires_at,created_at,last_seen_at"#,
 Uuid::now_v7(),link.id as _,&hash[..],ip_hash,ua,link.passphrase_hash,link.token_hash).fetch_optional(c).await?)
}

/// Revokes the carried link session during successful unlock, without affecting accounts.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn rotate_session(
    c: &mut PgConnection,
    link: ShareLinkId,
    hash: &[u8],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM sessions WHERE share_link_id=$1 AND token_hash=$2",
        link as _,
        hash
    )
    .execute(c)
    .await?;
    Ok(())
}

/// Validates that a new target exists and is not in trash.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn target_exists(
    c: &mut PgConnection,
    target: yhotos_core::access::ShareTarget,
) -> Result<bool, DbError> {
    let (kind, id) = match target {
        yhotos_core::access::ShareTarget::Album(id) => (TargetKind::Album, id.0),
        yhotos_core::access::ShareTarget::Photo(id) => (TargetKind::Photo, id.0),
    };
    Ok(sqlx::query_scalar!(r#"SELECT CASE WHEN $1::target_kind='album' THEN EXISTS(SELECT FROM albums WHERE id=$2 AND deleted_at IS NULL) ELSE EXISTS(SELECT FROM photos WHERE id=$2 AND deleted_at IS NULL) END as "exists!""#,kind as _,id).fetch_one(c).await?)
}

/// Locks one admin-editable link. Caller holds the transaction until changes commit.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn get_for_update(
    c: &mut PgConnection,
    id: ShareLinkId,
) -> Result<Option<ShareLinkRow>, DbError> {
    Ok(sqlx::query_as!(ShareLinkRow,r#"SELECT id as "id: _",created_at,updated_at,token_hash,target_kind as "target_kind: _",target_id,passphrase_hash,expires_at,allow_view,allow_download,allow_like,allow_flag,revoked_at,created_by as "created_by: _" FROM share_links WHERE id=$1 FOR UPDATE"#,id as _).fetch_optional(c).await?)
}

/// Changes the passphrase hash; a trigger revokes all previously unlocked sessions.
///
/// # Errors
/// Returns a database statement or connection failure.
pub async fn set_passphrase(
    c: &mut PgConnection,
    id: ShareLinkId,
    hash: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE share_links SET passphrase_hash=$2 WHERE id=$1",
        id as _,
        hash
    )
    .execute(c)
    .await?;
    Ok(())
}
