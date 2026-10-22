use super::{
    AppState, Audience, Auth, ExposeSecret, HeaderMap, HttpError, IntoResponse, Response,
    SecretString, login, now, redirect, required, sessions, set_cookie, ua, views,
};
use super::{HeaderValue, PgConnection, Token, TokenKind};
use crate::middleware::session::Current;
use axum::{
    Extension, Form, Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use yhotos_auth::{
    totp,
    webauthn::{Passkey, RegisterPublicKeyCredential, RegistrationState},
};
use yhotos_db::{audit, mfa, users};

pub(crate) async fn logout(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Option<Extension<Current>>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    if let Some(Extension(c)) = current {
        let mut tx = auth.db.pool().begin().await?;
        let hash = sessions::revoke(&mut tx, c.0.session.user_id, c.0.session.id).await?;
        audit::write(
            &mut tx,
            Some(c.0.session.user_id),
            "session.revoke",
            Some(c.0.session.id),
            &json!({"outcome":"logout"}),
        )
        .await?;
        tx.commit().await?;
        if let Some(hash) = hash {
            auth.invalidate(&hash);
        }
    }
    let mut response = redirect(a.login());
    set_cookie(&mut response, a.cookie(a.cookie_name(&state), "", true)?);
    set_cookie(&mut response, a.cookie(a.pending_cookie(), "", true)?);
    Ok(response)
}
pub(crate) async fn password_form(
    Extension(a): Extension<Audience>,
    current: Current,
) -> Result<Response, HttpError> {
    views::password(a, current.0.must_change_password)
}
#[derive(Deserialize)]
pub(crate) struct PasswordChange {
    #[serde(default)]
    current_password: SecretString,
    new_password: SecretString,
}
pub(crate) async fn password(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    headers: HeaderMap,
    Form(input): Form<PasswordChange>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    let user = users::get_by_id(&mut tx, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    if !user.must_change_password
        && !login::verify(
            auth,
            input.current_password,
            Some(user.password_hash.clone()),
        )
        .await?
    {
        return Ok(login::failed());
    }
    let new = input.new_password;
    let username = user.username.clone();
    let permit = auth
        .password_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| HttpError::BadRequest)?;
    let hash = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        yhotos_auth::check_password_policy(&new, &username)?;
        yhotos_auth::hash_password(&new)
    })
    .await
    .map_err(|_| HttpError::BadRequest)??;
    users::set_password(&mut tx, user.id, &SecretString::from(hash.as_str())).await?;
    let revoked = sessions::revoke_all(&mut tx, user.id).await?;
    for _ in &revoked {
        audit::write(
            &mut tx,
            Some(user.id),
            "session.revoke",
            None,
            &json!({"outcome":"password_change"}),
        )
        .await?;
    }
    audit::write(
        &mut tx,
        Some(user.id),
        "password.change",
        None,
        &json!({"outcome":"success"}),
    )
    .await?;
    let mut changed = user;
    changed.must_change_password = false;
    let response = login::establish(
        &mut tx,
        auth,
        a,
        &state,
        &changed,
        None,
        &current.0.session.ip_hash,
        ua(&headers),
        None,
        None,
    )
    .await?;
    tx.commit().await?;
    for hash in revoked {
        auth.invalidate(&hash);
    }
    Ok(response)
}
pub(crate) async fn index(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Extension(nonce): Extension<crate::middleware::security_headers::Nonce>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db.pool().acquire().await?;
    let sessions = sessions::list(&mut c, current.0.session.user_id).await?;
    let methods = mfa::list(&mut c, current.0.session.user_id).await?;
    views::security(a, &sessions, &methods, current.0.session.id, nonce)
}
#[cfg(feature = "admin")]
pub(crate) async fn setup(
    Extension(a): Extension<Audience>,
    _current: Current,
    Extension(nonce): Extension<crate::middleware::security_headers::Nonce>,
) -> Result<Response, HttpError> {
    views::setup(a, nonce)
}
#[derive(Deserialize)]
pub(crate) struct Reauth {
    password: SecretString,
}
pub(crate) async fn reauth(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Form(input): Form<Reauth>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db.pool().acquire().await?;
    let user = users::get_by_id(&mut c, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    if !login::verify(auth, input.password, Some(user.password_hash)).await? {
        return Ok(login::failed());
    }
    sessions::reauthenticate(&mut c, current.0.session.id, user.id).await?;
    auth.invalidate(&current.0.session.token_hash);
    Ok(redirect(a.security()))
}
pub(crate) async fn revoke(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db.pool().begin().await?;
    let hash = sessions::revoke(&mut tx, current.0.session.user_id, id)
        .await?
        .ok_or(HttpError::NotFound)?;
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "session.revoke",
        Some(id),
        &json!({"outcome":"revoked"}),
    )
    .await?;
    tx.commit().await?;
    auth.invalidate(&hash);
    let mut response = redirect(a.security());
    if id == current.0.session.id {
        set_cookie(&mut response, a.cookie(a.cookie_name(&state), "", true)?);
    }
    Ok(response)
}
#[derive(Serialize, Deserialize)]
struct TotpEnrollment {
    id: Uuid,
    sealed: Vec<u8>,
}
pub(crate) fn open_totp(auth: &Auth, id: Uuid, sealed: &[u8]) -> Result<totp::Secret, HttpError> {
    let raw = zeroize::Zeroizing::new(auth.keys.open(id.as_bytes(), sealed)?);
    let bytes = raw
        .as_slice()
        .try_into()
        .map_err(|_| HttpError::BadRequest)?;
    Ok(totp::Secret::from_bytes(bytes))
}
pub(crate) async fn totp_form(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db.pool().acquire().await?;
    let user = users::get_by_id(&mut c, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let secret = totp::generate_secret();
    let id = Uuid::now_v7();
    let sealed = auth.keys.seal(id.as_bytes(), secret.expose_secret())?;
    sessions::ceremony_put(
        &mut c,
        current.0.session.id,
        user.id,
        "totp",
        &serde_json::to_value(TotpEnrollment { id, sealed })?,
    )
    .await?;
    let uri = totp::provisioning_uri("Yhotos", &user.username, &secret);
    let qr = qrcode::QrCode::new(uri.expose_secret())
        .map_err(|_| HttpError::BadRequest)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build();
    views::totp(a, uri.expose_secret(), &qr)
}
#[derive(Deserialize)]
pub(crate) struct Code {
    code: String,
}
pub(crate) async fn totp_finish(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Form(input): Form<Code>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    let value = sessions::ceremony_take(
        &mut tx,
        current.0.session.id,
        current.0.session.user_id,
        "totp",
    )
    .await?
    .ok_or(HttpError::BadRequest)?;
    let enrollment: TotpEnrollment = serde_json::from_value(value)?;
    let secret = open_totp(auth, enrollment.id, &enrollment.sealed)?;
    let valid = totp::verify(&secret, &input.code, now(), None);
    if let Ok(step) = valid {
        let method = mfa::Method {
            id: enrollment.id,
            kind: "totp".into(),
            secret_enc: Some(enrollment.sealed),
            credential_id: None,
            public_key: None,
            sign_count: 0,
            label: "Authenticator app".into(),
            last_used_step: Some(i64::try_from(step).map_err(|_| HttpError::BadRequest)?),
        };
        mfa::add(&mut tx, current.0.session.user_id, &method).await?;
        audit::write(
            &mut tx,
            Some(current.0.session.user_id),
            "mfa.enroll",
            Some(method.id),
            &json!({"kind":"totp"}),
        )
        .await?;
        let cookie = rotate(&mut tx, a, &state, &current).await?;
        tx.commit().await?;
        auth.invalidate(&current.0.session.token_hash);
        let mut response = redirect(a.security());
        set_cookie(&mut response, cookie);
        Ok(response)
    } else {
        tx.commit().await?;
        Ok(login::failed())
    }
}
pub(crate) async fn passkey_start(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db.pool().acquire().await?;
    let user = users::get_by_id(&mut c, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let methods = mfa::list(&mut c, user.id).await?;
    let excluded = methods
        .iter()
        .filter_map(|m| m.credential_id.clone().map(Into::into))
        .collect::<Vec<_>>();
    let (challenge, ceremony) =
        auth.passkeys
            .start_registration(user.id.0, &user.username, &excluded, a.0)?;
    sessions::ceremony_put(
        &mut c,
        current.0.session.id,
        user.id,
        "register",
        &serde_json::to_value(ceremony)?,
    )
    .await?;
    Ok(Json(challenge).into_response())
}
pub(crate) async fn passkey_finish(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Json(input): Json<RegisterPublicKeyCredential>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db.pool().acquire().await?;
    // Autocommit consumption ensures even an invalid signature cannot reuse state.
    let value = sessions::ceremony_take(
        &mut c,
        current.0.session.id,
        current.0.session.user_id,
        "register",
    )
    .await?
    .ok_or(HttpError::BadRequest)?;
    let ceremony: RegistrationState = serde_json::from_value(value)?;
    let key: Passkey = auth.passkeys.finish_registration(ceremony, &input)?;
    let method = mfa::Method {
        id: Uuid::now_v7(),
        kind: "passkey".into(),
        secret_enc: None,
        credential_id: Some(key.cred_id().as_slice().to_vec()),
        public_key: Some(serde_json::to_vec(&key)?),
        sign_count: 0,
        label: "Passkey".into(),
        last_used_step: None,
    };
    drop(c);
    let mut tx = auth.db.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    mfa::add(&mut tx, current.0.session.user_id, &method).await?;
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "mfa.enroll",
        Some(method.id),
        &json!({"kind":"passkey"}),
    )
    .await?;
    let cookie = rotate(&mut tx, a, &state, &current).await?;
    tx.commit().await?;
    auth.invalidate(&current.0.session.token_hash);
    let mut response = Json(json!({"next":a.security()})).into_response();
    set_cookie(&mut response, cookie);
    Ok(response)
}
pub(crate) async fn remove(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    if !mfa::remove(&mut tx, current.0.session.user_id, id, current.0.session.id).await? {
        return Err(HttpError::ReauthRequired);
    }
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "mfa.remove",
        Some(id),
        &json!({"outcome":"removed"}),
    )
    .await?;
    let cookie = rotate(&mut tx, a, &state, &current).await?;
    tx.commit().await?;
    auth.invalidate(&current.0.session.token_hash);
    let mut response = redirect(a.security());
    set_cookie(&mut response, cookie);
    Ok(response)
}

// MFA enrollment/removal changes the session's privileges. Replace the bearer
// while preserving its original absolute lifetime and password-check timestamp.
async fn rotate(
    c: &mut PgConnection,
    a: Audience,
    state: &AppState,
    current: &Current,
) -> Result<HeaderValue, HttpError> {
    let old = &current.0.session;
    let Some(_) = sessions::revoke(c, old.user_id, old.id).await? else {
        return Err(HttpError::Authentication);
    };
    let token = Token::generate();
    let mut session = old.clone();
    session.id = Uuid::now_v7();
    session.last_seen_at = time::OffsetDateTime::now_utc();
    session.token_hash = token.hash(TokenKind::Session).as_bytes().to_vec();
    sessions::create(c, &session).await?;
    audit::write(
        c,
        Some(old.user_id),
        "session.revoke",
        Some(old.id),
        &json!({"outcome":"privilege_change"}),
    )
    .await?;
    a.cookie(
        a.cookie_name(state),
        &token.encode(TokenKind::Session),
        false,
    )
}
