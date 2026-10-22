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
use time::OffsetDateTime;
use yhotos_auth::{PasswordHash, Token, TokenKind};
use yhotos_core::{
    AlbumId, GroupId, PhotoId, ShareLinkId, UserId,
    access::{AlbumAccess, Grant, PhotoAccess, Principal, ShareTarget, Visibility, can_view_photo},
};
use yhotos_db::{
    audit, flags,
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
fn cookie_name(id: ShareLinkId) -> String {
    format!("yl_{}", yhotos_core::ShortId::from_photo_id(PhotoId(id.0)))
}
pub(crate) async fn resolve(state: &AppState, headers: &HeaderMap) -> Result<Links, HttpError> {
    let Some(auth) = &state.auth else {
        return Ok(Links::default());
    };
    let mut links = Links::default();
    let mut count = 0;
    for h in headers.get_all(header::COOKIE) {
        let Ok(h) = h.to_str() else { continue };
        for pair in h.split(';') {
            let Some((name, _)) = pair.trim().split_once('=') else {
                continue;
            };
            if !name.starts_with("yl_") || name.len() != 15 {
                continue;
            }
            count += 1;
            if count > 16 {
                return Err(HttpError::BadRequest);
            }
            let Some(hash) =
                auth::cookie(headers, name).and_then(|v| digest(v, TokenKind::Session))
            else {
                continue;
            };
            let Some(session) = auth.shares.session(state, hash).await? else {
                continue;
            };
            if cookie_name(session.link_id) != name
                || session.expires_at <= OffsetDateTime::now_utc()
                || session.last_seen_at <= OffsetDateTime::now_utc() - time::Duration::days(14)
                || session.created_at <= OffsetDateTime::now_utc() - time::Duration::days(90)
            {
                continue;
            }
            let Ok(hash) = session.link_hash.as_slice().try_into() else {
                continue;
            };
            let Some(snapshot) = auth.shares.link(state, hash).await? else {
                continue;
            };
            if active(&snapshot.landing.link) {
                links.0.push(scoped(&snapshot.landing.link));
            }
        }
    }
    Ok(links)
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/s/{token}", get(landing).post(unlock))
        .route("/s/{token}/unlock", post(unlock))
        .route("/api/v1/photos/{id}/like", post(like))
        .route("/api/v1/photos/{id}/flag", post(flag))
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
    let mut c = auth::required(&state)?
        .db(Audience(false))?
        .pool()
        .acquire()
        .await?;
    share_links::record_open(&mut c, s.landing.link.id, &visitor, None, None).await?;
    drop(c);
    if s.landing.link.passphrase_hash.is_some()
        && !links.0.iter().any(|l| l.link_id == s.landing.link.id)
    {
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
    if !links.0.iter().any(|l| l.link_id == s.landing.link.id) {
        issue_session(
            &state,
            &s.landing.link,
            request.headers(),
            auth::ip(&state, &request)?,
            &mut response,
        )
        .await?;
    }
    Ok(response)
}
async fn issue_session(
    state: &AppState,
    link: &ShareLinkRow,
    headers: &HeaderMap,
    ip: [u8; 32],
    response: &mut Response,
) -> Result<(), HttpError> {
    let token = Token::generate();
    let hash = *token.hash(TokenKind::Session).as_bytes();
    let name = cookie_name(link.id);
    let mut c = auth::required(state)?
        .db(Audience(false))?
        .pool()
        .acquire()
        .await?;
    let mut tx = c.begin().await?;
    if let Some(old) = auth::token_hash(headers, &name, TokenKind::Session) {
        share_links::rotate_session(&mut tx, link.id, &old).await?;
    }
    let shares = &auth::required(state)?.shares;
    let epoch = shares.epoch();
    let session = share_links::create_session(&mut tx, link, &hash, &ip, auth::ua(headers))
        .await?
        .ok_or(HttpError::NotFound)?;
    tx.commit().await?;
    shares.remember_session(epoch, hash, session).await;
    let encoded = Zeroizing::new(token.encode(TokenKind::Session));
    drop(token);
    auth::set_cookie(response, Audience(false).cookie(&name, &encoded, false)?);
    Ok(())
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
        auth.shares.link(&state, hash).await?
    } else {
        None
    };
    let phc = s
        .as_ref()
        .filter(|s| active(&s.landing.link))
        .and_then(|s| s.landing.link.passphrase_hash.clone());
    let mut c = auth.db(Audience(false))?.pool().acquire().await?;
    let mut tx = c.begin().await?;
    let mut lock = if let Some(hash) = hash {
        serde_json::from_value::<UnlockState>(sessions::lockout_get(&mut tx, &hash).await?)
            .map_err(|_| HttpError::Internal(anyhow::anyhow!("invalid unlock state")))?
    } else {
        UnlockState::default()
    };
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
    let valid = s.as_ref().is_some_and(|s| active(&s.landing.link));
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
    tx.commit().await?;
    drop(c);
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
    issue_session(
        &state,
        &s.landing.link,
        request.headers(),
        auth::ip(&state, &request)?,
        &mut response,
    )
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
        flags::flag(&mut c, id, actor).await?;
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
