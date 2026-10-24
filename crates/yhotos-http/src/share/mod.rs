//! Tokenized, capability-scoped album/photo access. Raw URL tokens are never cached.
#[cfg(feature = "admin")]
mod admin;
pub(crate) mod cache;
mod views;

use crate::{
    AppState,
    auth::{self, Audience},
    error::HttpError,
    middleware::{rate_limit::client_ip, security_headers::Nonce, session::SessionIdentity},
    pages::Options,
};
use axum::{
    Extension, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use yhotos_auth::{PasswordHash, Token, TokenKind};
use yhotos_core::{
    AlbumId, GroupId, PhotoId, ShareLinkId, UserId,
    access::{AlbumAccess, Grant, PhotoAccess, Principal, ShareTarget, Visibility, can_view_photo},
};
use yhotos_db::{
    audit,
    likes::{self, Actor},
    sessions,
    share_links::{self, LinkFlags, ShareLinkRow},
};
use zeroize::Zeroizing;

/// A verified capability set and scope, carried to every action without a photo lookup.
#[derive(Clone, Debug)]
pub(crate) struct ScopedLink {
    pub target: ShareTarget,
    pub link_id: ShareLinkId,
    pub caps: LinkFlags,
    pub passphrase_version: i64,
}
impl ScopedLink {
    pub(crate) fn principal(&self) -> Principal<'_> {
        Principal::ShareLink {
            target: self.target,
        }
    }
}
/// All valid link sessions coexist with the independent account identity.
#[derive(Clone, Debug, Default)]
pub(crate) struct Links(pub Vec<ScopedLink>);

#[derive(Deserialize)]
pub(crate) struct Photo {
    pub id: PhotoId,
    pub title: String,
    visibility: String,
    pub original_hash: String,
    pub image: Option<String>,
    pub width: i32,
    pub height: i32,
    users: Vec<UserId>,
    groups: Vec<GroupId>,
    albums: Vec<Level>,
}
#[derive(Deserialize)]
struct Level {
    id: AlbumId,
    visibility: String,
    users: Vec<UserId>,
    groups: Vec<GroupId>,
}
fn visibility(value: &str) -> Result<Visibility, HttpError> {
    match value {
        "public" => Ok(Visibility::Public),
        "restricted" => Ok(Visibility::Restricted),
        "inherit" => Ok(Visibility::Inherit),
        _ => Err(HttpError::Internal(anyhow::anyhow!(
            "invalid stored visibility"
        ))),
    }
}
fn grants(users: &[UserId], groups: &[GroupId]) -> Vec<Grant> {
    users
        .iter()
        .copied()
        .map(Grant::User)
        .chain(groups.iter().copied().map(Grant::Group))
        .collect()
}
impl Photo {
    pub(crate) fn may_view(&self, p: &Principal<'_>) -> bool {
        let levels: Vec<_> = self
            .albums
            .iter()
            .map(|a| grants(&a.users, &a.groups))
            .collect();
        let albums: Vec<_> = self
            .albums
            .iter()
            .zip(&levels)
            .filter_map(|(a, g)| {
                Some(AlbumAccess {
                    id: a.id,
                    visibility: visibility(&a.visibility).ok()?,
                    grants: g,
                    ancestors: &[],
                })
            })
            .collect();
        let Some((leaf, parents)) = albums.split_first() else {
            return false;
        };
        let album = AlbumAccess {
            ancestors: parents,
            ..leaf.clone()
        };
        let Ok(vis) = visibility(&self.visibility) else {
            return false;
        };
        can_view_photo(
            p,
            &PhotoAccess {
                id: self.id,
                album: &album,
                visibility: vis,
                grants: &grants(&self.users, &self.groups),
            },
        )
    }
}
pub(crate) fn decode_photos(value: serde_json::Value) -> Result<Vec<Photo>, HttpError> {
    let photos: Vec<Photo> = serde_json::from_value(value)
        .map_err(|_| HttpError::Internal(anyhow::anyhow!("invalid stored share metadata")))?;
    for p in &photos {
        let valid_image = p.image.as_ref().is_none_or(|image| {
            let Some(path) = image.strip_prefix("/i/") else {
                return false;
            };
            let Some((hash, name)) = path.split_once('/') else {
                return false;
            };
            hash.parse::<yhotos_storage::ContentHash>().is_ok()
                && name.len() <= 32
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.')
        });
        if p.original_hash
            .parse::<yhotos_storage::ContentHash>()
            .is_err()
            || !valid_image
            || p.title.len() > 4096
            || p.width <= 0
            || p.height <= 0
            || p.albums.is_empty()
            || visibility(&p.visibility).is_err()
            || p.albums.iter().any(|a| visibility(&a.visibility).is_err())
        {
            return Err(HttpError::Internal(anyhow::anyhow!(
                "invalid stored share metadata"
            )));
        }
    }
    Ok(photos)
}
fn target(link: &ShareLinkRow) -> ShareTarget {
    match link.target_kind {
        share_links::TargetKind::Album => ShareTarget::Album(AlbumId(link.target_id)),
        share_links::TargetKind::Photo => ShareTarget::Photo(PhotoId(link.target_id)),
    }
}
fn scoped(link: &ShareLinkRow) -> ScopedLink {
    ScopedLink {
        target: target(link),
        link_id: link.id,
        passphrase_version: link.passphrase_version,
        caps: LinkFlags {
            view: link.allow_view,
            download: link.allow_download,
            like: link.allow_like,
            flag: link.allow_flag,
        },
    }
}
fn active(link: &ShareLinkRow) -> bool {
    link.revoked_at.is_none()
        && link.allow_view
        && link
            .expires_at
            .is_none_or(|t| t > OffsetDateTime::now_utc())
}
fn digest(value: &str, expected: TokenKind) -> Option<[u8; 32]> {
    let (kind, token) = Token::decode(value).ok()?;
    (kind == expected).then(|| *token.hash(kind).as_bytes())
}
/// A single bearer cookie holds no link ids or capabilities.
pub(crate) const COOKIE_NAME: &str = "__Host-yhotos-share";
fn valid_session(session: &share_links::LinkSession, hash: &[u8; 32]) -> bool {
    let now = OffsetDateTime::now_utc();
    bool::from(session.token_hash.as_slice().ct_eq(hash))
        && session.expires_at > now
        && session.last_seen_at > now - time::Duration::days(14)
        && session.created_at > now - time::Duration::days(90)
        && session.created_at <= session.last_seen_at
        && session.created_at <= now
}
pub(crate) async fn resolve(state: &AppState, headers: &HeaderMap) -> Result<Links, HttpError> {
    let Some(auth) = &state.auth else {
        return Ok(Links::default());
    };
    let Some(hash) = auth::cookie(headers, COOKIE_NAME).and_then(|v| digest(v, TokenKind::Session))
    else {
        return Ok(Links::default());
    };
    let Some(session) = auth
        .shares
        .session(state, hash)
        .await?
        .filter(|s| valid_session(s, &hash))
    else {
        return Ok(Links::default());
    };
    let mut links = Links::default();
    for member in &session.links {
        let scope = auth.shares.scope(member).await;
        if scope.passphrase_version != member.passphrase_version
            || !scope.flags.view
            || scope.expires_at.is_some_and(|t| {
                i128::from(t) <= OffsetDateTime::now_utc().unix_timestamp_nanos() / 1000
            })
        {
            continue;
        }
        links.0.push(ScopedLink {
            target: match scope.target_kind {
                share_links::TargetKind::Album => ShareTarget::Album(AlbumId(scope.target_id)),
                share_links::TargetKind::Photo => ShareTarget::Photo(PhotoId(scope.target_id)),
            },
            link_id: scope.link_id,
            passphrase_version: member.passphrase_version,
            caps: LinkFlags {
                view: scope.flags.view,
                download: scope.flags.download,
                like: scope.flags.like,
                flag: scope.flags.flag,
            },
        });
    }
    Ok(links)
}
// Ignore all unknown values. Legacy names are accepted only as cookie names for
// deletion; they never trigger token decoding, a query, or an authorization.
fn legacy_names(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|p| p.trim().split_once('=').map(|(name, _)| name))
        .filter(|name| {
            name.starts_with("yl_")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        })
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/s/{token}", get(landing).post(unlock))
        .route("/s/{token}/unlock", post(unlock))
        .route("/s/photos/{id}/like", post(like))
        .route("/s/photos/{id}/flag", post(flag))
}
async fn snapshot(
    state: &AppState,
    token: String,
    number: u32,
    sort: yhotos_db::photos::PageSort,
) -> Result<Arc<cache::Snapshot>, HttpError> {
    let token = Zeroizing::new(token);
    let hash = digest(&token, TokenKind::ShareLink).ok_or(HttpError::NotFound)?;
    drop(token);
    let s = auth::required(state)?
        .shares
        .page(state, hash, number, sort)
        .await?
        .ok_or(HttpError::NotFound)?;
    if !active(&s.landing.link) {
        return Err(HttpError::NotFound);
    }
    Ok(s)
}
async fn landing(
    State(state): State<AppState>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    Extension(links): Extension<Links>,
    Extension(nonce): Extension<Nonce>,
    Query(options): Query<Options>,
    request: Request,
) -> Result<Response, HttpError> {
    let Path(token) = path.map_err(|_| HttpError::NotFound)?;
    let number = options.number()?;
    let sort = options.sort();
    let s = snapshot(&state, token, number, sort).await?;
    let ip = client_ip(&request, state.config.trust_proxy)?;
    let visitor = auth::required(&state)?.shares.visitor(
        ip,
        auth::ua(request.headers()),
        OffsetDateTime::now_utc(),
    )?;
    let existing = carried_session(&state, request.headers()).await?;
    let member = existing.as_ref().and_then(|(_, session)| {
        session.links.iter().find(|l| {
            l.link_id == s.landing.link.id
                && l.passphrase_version == s.landing.link.passphrase_version
                && links.0.iter().any(|scope| {
                    scope.link_id == l.link_id && scope.passphrase_version == l.passphrase_version
                })
        })
    });
    let auth = auth::required(&state)?;
    if s.landing.link.passphrase_hash.is_some() && member.is_none() {
        let mut c = auth.db(Audience(false))?.pool().acquire().await?;
        share_links::record_open(&mut c, s.landing.link.id, &visitor, None, None).await?;
        return views::unlock(request.headers(), false);
    }
    let mut response = views::page(
        &s,
        request.headers(),
        &state,
        &nonce.to_string(),
        number,
        sort,
    )?;
    if let (Some((_, session)), Some(member)) = (&existing, member) {
        // One required statistics statement, including a coalesced recency
        // update when due. A GET can never insert a protected membership.
        auth.shares
            .record_open(&state, session.id, member, &visitor)
            .await?;
    } else {
        // Only passphrase-free links reach this insertion path. The version
        // guard in SQL also rejects a passphrase added after the page lookup.
        let mut c = auth.db(Audience(false))?.pool().acquire().await?;
        let mut tx = c.begin().await?;
        share_links::record_open(&mut tx, s.landing.link.id, &visitor, None, None).await?;
        let issued = issue_session(
            &state,
            &mut tx,
            &s.landing.link,
            existing,
            auth::ip(&state, &request)?,
            auth::ua(request.headers()),
        )
        .await?;
        tx.commit().await?;
        issued
            .finish(&state, request.headers(), &mut response)
            .await?;
    }
    Ok(response)
}
async fn carried_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<([u8; 32], Arc<share_links::LinkSession>)>, HttpError> {
    let Some(hash) = auth::cookie(headers, COOKIE_NAME).and_then(|v| digest(v, TokenKind::Session))
    else {
        return Ok(None);
    };
    Ok(auth::required(state)?
        .shares
        .session(state, hash)
        .await?
        .filter(|s| valid_session(s, &hash))
        .map(|s| (hash, s)))
}
struct IssuedSession {
    epoch: u64,
    revision: u64,
    hash: [u8; 32],
    session: share_links::LinkSession,
    token: Option<Token>,
}
impl IssuedSession {
    async fn finish(
        self,
        state: &AppState,
        headers: &HeaderMap,
        response: &mut Response,
    ) -> Result<(), HttpError> {
        auth::required(state)?
            .shares
            .remember_session(self.epoch, self.revision, self.hash, self.session)
            .await;
        let encoded = Zeroizing::new(if let Some(token) = self.token {
            token.encode(TokenKind::Session)
        } else {
            auth::cookie(headers, COOKIE_NAME)
                .ok_or(HttpError::NotFound)?
                .to_owned()
        });
        auth::set_cookie(
            response,
            Audience(false).cookie(COOKIE_NAME, &encoded, false)?,
        );
        for name in legacy_names(headers) {
            auth::set_cookie(response, Audience(false).cookie(name, "", true)?);
        }
        Ok(())
    }
}
// Called in the transaction holding the verified link lock for protected links,
// or with a passphrase-free snapshot whose version SQL checks before insertion.
async fn issue_session(
    state: &AppState,
    c: &mut sqlx::PgConnection,
    link: &ShareLinkRow,
    existing: Option<([u8; 32], Arc<share_links::LinkSession>)>,
    ip: [u8; 32],
    ua: &str,
) -> Result<IssuedSession, HttpError> {
    let auth = auth::required(state)?;
    let epoch = auth.shares.epoch();
    let (hash, session, token) = if let Some((hash, existing)) = existing {
        if !share_links::unlock_session_link(c, existing.id, link).await? {
            return Err(HttpError::NotFound);
        }
        let session = share_links::load_session(c, &hash)
            .await?
            .ok_or(HttpError::NotFound)?;
        (hash, session, None)
    } else {
        let token = Token::generate();
        let hash = *token.hash(TokenKind::Session).as_bytes();
        let session = share_links::create_session(c, link, &hash, &ip, ua)
            .await?
            .ok_or(HttpError::NotFound)?;
        (hash, session, Some(token))
    };
    let revision = auth.shares.session_revision(hash);
    Ok(IssuedSession {
        epoch,
        revision,
        hash,
        session,
        token,
    })
}
// Prehashing permits the scope's "any string" passphrases without weakening or
// changing the shared Argon2 primitive's 8..=256-byte input contract.
fn passphrase(value: &str) -> SecretString {
    let mut h = blake3::Hasher::new_derive_key("yhotos share passphrase v1");
    h.update(value.as_bytes());
    SecretString::from(h.finalize().to_hex().to_string())
}
#[derive(Deserialize)]
struct Unlock {
    passphrase: String,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct UnlockState {
    failures: u8,
    locked_until: Option<i64>,
}
impl UnlockState {
    fn locked(&self, now: i64) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }
    fn failure(&mut self, now: i64) -> bool {
        if self.locked_until.is_some_and(|until| until <= now) {
            *self = Self::default();
        }
        self.failures = self.failures.saturating_add(1).min(10);
        if self.failures == 10 {
            self.locked_until = Some(now.saturating_add(3600));
            true
        } else {
            false
        }
    }
}
#[allow(
    clippy::too_many_lines,
    reason = "keep the serialized lockout and verification transaction together"
)]
async fn unlock(
    State(state): State<AppState>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    Extension(nonce): Extension<Nonce>,
    request: Request,
) -> Result<Response, HttpError> {
    use axum::extract::FromRequest;
    let Path(token) = path.map_err(|_| HttpError::NotFound)?;
    let raw = Zeroizing::new(token);
    let hash = digest(&raw, TokenKind::ShareLink);
    drop(raw);
    let peer = client_ip(&request, state.config.trust_proxy)?;
    let (parts, body) = request.into_parts();
    let headers = parts.headers.clone();
    let axum::Form(input) =
        axum::Form::<Unlock>::from_request(Request::from_parts(parts, body), &state)
            .await
            .map_err(|_| HttpError::BadRequest)?;
    let input = Zeroizing::new(input.passphrase);
    let auth = auth::required(&state)?;
    // A bounded blocking pool equalizes unknown/malformed/locked/wrong paths.
    let permit = auth
        .password_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| HttpError::RateLimited)?;
    let s = if let Some(hash) = hash {
        auth.shares
            .page(&state, hash, 1, yhotos_db::photos::PageSort::Taken)
            .await?
    } else {
        None
    };
    let existing = carried_session(&state, &headers).await?;
    let mut c = auth.db(Audience(false))?.pool().acquire().await?;
    let mut tx = c.begin().await?;
    let mut lock = if let Some(hash) = hash {
        serde_json::from_value::<UnlockState>(sessions::lockout_get(&mut tx, &hash).await?)
            .map_err(|_| HttpError::Internal(anyhow::anyhow!("invalid unlock state")))?
    } else {
        UnlockState::default()
    };
    let current = if let Some(s) = &s {
        share_links::lock_for_unlock(&mut tx, s.landing.link.id).await?
    } else {
        None
    };
    let phc = current
        .as_ref()
        .and_then(|link| link.passphrase_hash.clone());
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let locked = lock.locked(now);
    let password = passphrase(&input);
    drop(input);
    let passwords = auth.passwords;
    let success = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if !locked
            && let Some(phc) = phc
            && let Ok(phc) = PasswordHash::parse(&phc)
        {
            return yhotos_auth::verify_password(&password, &phc).unwrap_or(false);
        }
        passwords.dummy_verify();
        false
    })
    .await
    .map_err(|_| HttpError::Internal(anyhow::anyhow!("passphrase verification unavailable")))?;
    let valid = current.as_ref().is_some_and(active);
    if !success || !valid {
        if valid
            && !locked
            && let Some(hash) = hash
        {
            if lock.failure(now) {
                audit::write(
                    &mut tx,
                    None,
                    "share_unlock_locked",
                    s.as_ref().map(|s| s.landing.link.id.0),
                    &serde_json::json!({"hours":1}),
                )
                .await?;
            }
            sessions::lockout_set(&mut tx, &hash, &serde_json::to_value(lock)?).await?;
        }
        tx.commit().await?;
        if !valid {
            return Err(HttpError::NotFound);
        }
        return views::unlock(&headers, true);
    }
    if let Some(hash) = hash {
        sessions::lockout_set(
            &mut tx,
            &hash,
            &serde_json::to_value(UnlockState::default())?,
        )
        .await?;
    }
    let s = s.ok_or(HttpError::NotFound)?;
    let mut response = views::page(
        &s,
        &headers,
        &state,
        &nonce.to_string(),
        1,
        yhotos_db::photos::PageSort::Taken,
    )?;
    // Only the validated network information and coarse UA are needed here.
    let mut request = Request::new(axum::body::Body::empty());
    *request.headers_mut() = headers;
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
            peer, 0,
        )));
    let issued = issue_session(
        &state,
        &mut tx,
        &current.ok_or(HttpError::NotFound)?,
        existing,
        auth::ip(&state, &request)?,
        auth::ua(request.headers()),
    )
    .await?;
    tx.commit().await?;
    issued
        .finish(&state, request.headers(), &mut response)
        .await?;
    Ok(response)
}
async fn interact(
    state: &AppState,
    id: PhotoId,
    identity: &SessionIdentity,
    links: &Links,
    is_flag: bool,
) -> Result<StatusCode, HttpError> {
    let mut c = auth::required(state)?
        .db(Audience(false))?
        .pool()
        .acquire()
        .await?;
    let photos = decode_photos(share_links::photo_context(&mut c, id).await?)?;
    let photo = photos.first().ok_or(HttpError::NotFound)?;
    let actor = links
        .0
        .iter()
        .find(|l| {
            l.caps.view
                && (if is_flag { l.caps.flag } else { l.caps.like })
                && photo.may_view(&l.principal())
        })
        .map(|l| Actor::ShareLink(l.link_id))
        .or_else(|| {
            if let SessionIdentity::User { id, .. } = identity
                && photo.may_view(&identity.principal())
            {
                Some(Actor::User(*id))
            } else {
                None
            }
        })
        .ok_or(HttpError::NotFound)?;
    if is_flag {
        let mut tx = c.begin().await?;
        crate::api::flag_once(&mut tx, id, actor).await?;
        tx.commit().await?;
    } else {
        likes::like(&mut c, id, actor).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn like(
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
) -> Result<StatusCode, HttpError> {
    interact(&state, PhotoId(id), &identity, &links, false).await
}
async fn flag(
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
) -> Result<StatusCode, HttpError> {
    interact(&state, PhotoId(id), &identity, &links, true).await
}

#[cfg(feature = "admin")]
pub(crate) fn admin_routes() -> Router<AppState> {
    admin::routes()
}

#[cfg(test)]
mod tests;

#[cfg(feature = "admin")]
pub(crate) fn admin_paths() -> Vec<&'static str> {
    admin::route_table()
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}
