//! Admin-listener-only JSON management. Returned URLs are never stored.
use super::{passphrase, scoped};
use crate::{
    AppState,
    auth::{self, Audience},
    error::HttpError,
    middleware::session::Current,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch, post},
};
use secrecy::SecretString;
use serde::{Deserialize, Deserializer};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use yhotos_auth::{Token, TokenKind};
use yhotos_core::{AlbumId, PhotoId, ShareLinkId, access::ShareTarget};
use yhotos_db::{
    audit,
    share_links::{self, LinkFlags, NewShareLink},
};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent link capabilities"
)]
struct Create {
    target_kind: String,
    target_id: uuid::Uuid,
    expires_at: Option<String>,
    passphrase: Option<String>,
    allow_view: bool,
    allow_download: bool,
    allow_like: bool,
    allow_flag: bool,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::option_option,
    reason = "PATCH distinguishes omitted, null, and a replacement value"
)]
struct Patch {
    allow_view: Option<bool>,
    allow_download: Option<bool>,
    allow_like: Option<bool>,
    allow_flag: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    expires_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    passphrase: Option<Option<String>>,
}
#[allow(
    clippy::option_option,
    reason = "preserves JSON null separately from an absent PATCH field"
)]
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}
fn expiry(value: &str) -> Result<OffsetDateTime, HttpError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| HttpError::BadRequest)
}
async fn hash(state: &AppState, value: Option<String>) -> Result<Option<SecretString>, HttpError> {
    let Some(value) = value else { return Ok(None) };
    let value = Zeroizing::new(value);
    let password = passphrase(&value);
    drop(value);
    let auth = auth::required(state)?;
    let permit = auth
        .password_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| HttpError::RateLimited)?;
    let passwords = auth.passwords;
    let hashed = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        passwords.hash_password(&password)
    })
    .await
    .map_err(|_| HttpError::Internal(anyhow::anyhow!("passphrase hashing unavailable")))??;
    Ok(Some(SecretString::from(hashed.as_str())))
}
fn guard(current: &Current) -> Result<(), HttpError> {
    if current.0.is_admin
        && current.0.session.admin
        && current.0.session.mfa_at.is_some()
        && current.0.has_mfa
    {
        Ok(())
    } else {
        Err(HttpError::NotFound)
    }
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/api/share-links", post(create))
        .route("/admin/api/share-links/{id}", patch(update))
        .route("/admin/api/share-links/{id}/revoke", post(revoke))
        .route("/admin/api/share-links/{id}/stats", get(stats))
}
async fn create(
    State(state): State<AppState>,
    current: Current,
    Json(input): Json<Create>,
) -> Result<(StatusCode, Json<serde_json::Value>), HttpError> {
    guard(&current)?;
    let target = match input.target_kind.as_str() {
        "album" => ShareTarget::Album(AlbumId(input.target_id)),
        "photo" => ShareTarget::Photo(PhotoId(input.target_id)),
        _ => return Err(HttpError::BadRequest),
    };
    let expires_at = input.expires_at.as_deref().map(expiry).transpose()?;
    let passphrase_hash = hash(&state, input.passphrase).await?;
    let auth = auth::required(&state)?;
    let mut c = auth.db(Audience(true))?.pool().acquire().await?;
    let mut tx = c.begin().await?;
    if !share_links::target_exists(&mut tx, target).await? {
        return Err(HttpError::NotFound);
    }
    let token = Token::generate();
    let link = share_links::create(
        &mut tx,
        &NewShareLink {
            token_hash: *token.hash(TokenKind::ShareLink).as_bytes(),
            target,
            passphrase_hash,
            expires_at,
            flags: LinkFlags {
                view: input.allow_view,
                download: input.allow_download,
                like: input.allow_like,
                flag: input.allow_flag,
            },
            created_by: current.0.session.user_id,
        },
    )
    .await?;
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "share_created",
        Some(link.id.0),
        &serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    let encoded = Zeroizing::new(token.encode(TokenKind::ShareLink));
    drop(token);
    let url = Zeroizing::new(format!(
        "{}/s/{}",
        state.config.public_base_url.as_str().trim_end_matches('/'),
        encoded.as_str()
    ));
    // Json serializes this one-time value into the response; no GET retrieves it.
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"id":link.id.0,"url":url.as_str()})),
    ))
}
async fn update(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<uuid::Uuid>,
    Json(input): Json<Patch>,
) -> Result<StatusCode, HttpError> {
    guard(&current)?;
    let passphrase = match input.passphrase {
        Some(value) => Some(hash(&state, value).await?),
        None => None,
    };
    let expires = input
        .expires_at
        .map(|v| v.as_deref().map(expiry).transpose())
        .transpose()?;
    let auth = auth::required(&state)?;
    let mut c = auth.db(Audience(true))?.pool().acquire().await?;
    let mut tx = c.begin().await?;
    let id = ShareLinkId(id);
    let row = share_links::get_for_update(&mut tx, id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let mut flags = scoped(&row).caps;
    if let Some(v) = input.allow_view {
        flags.view = v;
    }
    if let Some(v) = input.allow_download {
        flags.download = v;
    }
    if let Some(v) = input.allow_like {
        flags.like = v;
    }
    if let Some(v) = input.allow_flag {
        flags.flag = v;
    }
    share_links::update_flags(&mut tx, id, flags).await?;
    if let Some(expires) = expires {
        share_links::set_expiry(&mut tx, id, expires).await?;
    }
    if let Some(passphrase) = passphrase {
        share_links::set_passphrase(
            &mut tx,
            id,
            passphrase
                .as_ref()
                .map(secrecy::ExposeSecret::expose_secret),
        )
        .await?;
    }
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "share_updated",
        Some(id.0),
        &serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    auth.shares.invalidate();
    Ok(StatusCode::NO_CONTENT)
}
async fn revoke(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode, HttpError> {
    guard(&current)?;
    let auth = auth::required(&state)?;
    let mut c = auth.db(Audience(true))?.pool().acquire().await?;
    let mut tx = c.begin().await?;
    if share_links::revoke(&mut tx, ShareLinkId(id)).await? == 0 {
        return Err(HttpError::NotFound);
    }
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "share_revoked",
        Some(id),
        &serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    auth.shares.invalidate();
    Ok(StatusCode::NO_CONTENT)
}
async fn stats(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, HttpError> {
    guard(&current)?;
    let auth = auth::required(&state)?;
    let mut c = auth.db(Audience(true))?.pool().acquire().await?;
    // Do not fabricate empty statistics for an unknown id.
    let mut tx = c.begin().await?;
    if share_links::get_for_update(&mut tx, ShareLinkId(id))
        .await?
        .is_none()
    {
        return Err(HttpError::NotFound);
    }
    let s = share_links::stats(&mut tx, ShareLinkId(id)).await?;
    tx.commit().await?;
    Ok(Json(
        serde_json::json!({"opens":s.opens,"unique_visitors":s.unique_visitors,"by_country":s.countries,
        "first_open":s.first_open.map(|t|t.format(&Rfc3339)).transpose().map_err(|_|HttpError::BadRequest)?,
        "last_open":s.last_open.map(|t|t.format(&Rfc3339)).transpose().map_err(|_|HttpError::BadRequest)?}),
    ))
}
use sqlx::Connection;
