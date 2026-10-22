use super::{
    AppState, Audience, Auth, ExposeSecret, HttpError, IntoResponse, Request, Response,
    SecretString, login, now, redirect, required, sessions, set_cookie, ua, views,
};
use super::{HeaderMap, HeaderValue, PgConnection, Token, TokenKind};
use crate::middleware::session::Current;
use axum::{
    Extension, Form, Json,
    extract::{FromRequest, Path, State},
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
        let mut tx = auth.db(a)?.pool().begin().await?;
        let revoked = sessions::revoke(&mut tx, c.0.session.user_id, c.0.session.id).await?;
        audit::write(
            &mut tx,
            Some(c.0.session.user_id),
            "session.revoke",
            Some(c.0.session.id),
            &json!({"outcome":"logout"}),
        )
        .await?;
        tx.commit().await?;
        if revoked.is_some() {
            auth.invalidate(c.0.session.id);
        }
    }
    let mut response = redirect(a.login());
    set_cookie(&mut response, a.cookie(a.cookie_name(&state), "", true)?);
    set_cookie(&mut response, a.cookie(a.pending_cookie(), "", true)?);
    let _ = super::csrf(&mut response, a)?;
    Ok(response)
}
pub(crate) async fn password_form(
    Extension(a): Extension<Audience>,
    current: Current,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    views::password(a, &headers, current.0.must_change_password)
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
    request: Request,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db(a)?.pool().begin().await?;
    let network = super::ip(&state, &request)?;
    let agent = ua(request.headers());
    let Form(input) = Form::<PasswordChange>::from_request(request, &state)
        .await
        .map_err(|_| HttpError::BadRequest)?;
    let user = if current.0.must_change_password {
        users::lock_auth(&mut tx, current.0.session.user_id).await?;
        let user = users::get_by_id(&mut tx, current.0.session.user_id)
            .await?
            .ok_or(HttpError::NotFound)?;
        // A concurrent password change must not turn a stale setup session into
        // a way to bypass current-password verification.
        if !user.must_change_password || user.disabled_at.is_some() {
            return Ok(login::failed());
        }
        user
    } else {
        match login::check_password(
            &mut tx,
            auth,
            a,
            login::PasswordTarget::Account(current.0.session.user_id),
            input.current_password,
            &network,
        )
        .await?
        {
            login::PasswordCheck::Accepted(user) => user,
            login::PasswordCheck::Refused(response) => {
                tx.commit().await?;
                return Ok(response);
            }
        }
    };
    let new = input.new_password;
    let username = user.username.clone();
    let permit = auth
        .password_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| HttpError::BadRequest)?;
    let passwords = auth.passwords;
    let hash = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        yhotos_auth::check_password_policy(&new, &username)?;
        passwords.hash_password(&new)
    })
    .await
    .map_err(|_| HttpError::BadRequest)??;
    // Current predates password hashing. Consume the authorizing bearer in the
    // same transaction as the password write and replacement session so a
    // concurrent logout or revocation cannot resurrect it.
    let Some(current_hash) = sessions::revoke(&mut tx, user.id, current.0.session.id).await? else {
        return Ok(login::failed());
    };
    users::set_password(&mut tx, user.id, &SecretString::from(hash.as_str())).await?;
    let mut revoked = sessions::revoke_all(&mut tx, user.id).await?;
    revoked.push(current_hash);
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
        a,
        &state,
        &changed,
        None,
        &network,
        agent,
        None,
        current.0.session.mfa_at,
        None,
    )
    .await?;
    tx.commit().await?;
    auth.invalidate_user(changed.id);
    Ok(response)
}
pub(crate) async fn index(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Extension(nonce): Extension<crate::middleware::security_headers::Nonce>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db(a)?.pool().acquire().await?;
    let sessions = sessions::list(&mut c, current.0.session.user_id).await?;
    let rp = a.rp_id(&state)?;
    let methods = mfa::list(&mut c, current.0.session.user_id)
        .await?
        .into_iter()
        .filter(|method| method.rp_id.as_deref().is_none_or(|id| id == rp))
        .collect::<Vec<_>>();
    views::security(
        a,
        &headers,
        &sessions,
        &methods,
        current.0.session.id,
        nonce,
    )
}
#[cfg(feature = "admin")]
pub(crate) async fn setup(
    Extension(a): Extension<Audience>,
    _current: Current,
    Extension(nonce): Extension<crate::middleware::security_headers::Nonce>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    views::setup(a, &headers, nonce)
}
#[derive(Deserialize)]
pub(crate) struct Reauth {
    password: SecretString,
}
pub(crate) async fn reauth(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    request: Request,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let network = super::ip(&state, &request)?;
    let Form(input) = Form::<Reauth>::from_request(request, &state)
        .await
        .map_err(|_| HttpError::BadRequest)?;
    let mut tx = auth.db(a)?.pool().begin().await?;
    let user = match login::check_password(
        &mut tx,
        auth,
        a,
        login::PasswordTarget::Account(current.0.session.user_id),
        input.password,
        &network,
    )
    .await?
    {
        login::PasswordCheck::Accepted(user) => user,
        login::PasswordCheck::Refused(response) => {
            tx.commit().await?;
            return Ok(response);
        }
    };
    sessions::reauthenticate(&mut tx, current.0.session.id, user.id).await?;
    tx.commit().await?;
    auth.invalidate(current.0.session.id);
    Ok(redirect(a.security()))
}
pub(crate) async fn revoke(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db(a)?.pool().begin().await?;
    let _hash = sessions::revoke(&mut tx, current.0.session.user_id, id)
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
    auth.invalidate(id);
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
async fn enrollment_methods(
    c: &mut PgConnection,
    current: &Current,
) -> Result<Vec<mfa::Method>, HttpError> {
    let methods = mfa::list(c, current.0.session.user_id).await?;
    if !methods.is_empty() && current.0.session.mfa_at.is_none() {
        return Err(HttpError::Authentication);
    }
    Ok(methods)
}
pub(crate) async fn totp_form(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db(a)?.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    enrollment_methods(&mut tx, &current).await?;
    let user = users::get_by_id(&mut tx, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let secret = totp::generate_secret();
    let id = Uuid::now_v7();
    let sealed = auth.keys.seal(id.as_bytes(), secret.expose_secret())?;
    sessions::ceremony_put(
        &mut tx,
        current.0.session.id,
        user.id,
        "totp",
        &serde_json::to_value(TotpEnrollment { id, sealed })?,
    )
    .await?;
    tx.commit().await?;
    let uri = totp::provisioning_uri("Yhotos", &user.username, &secret);
    let qr = qrcode::QrCode::new(uri.expose_secret())
        .map_err(|_| HttpError::BadRequest)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build();
    views::totp(a, &headers, uri.expose_secret(), &qr)
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
    let mut tx = auth.db(a)?.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    enrollment_methods(&mut tx, &current).await?;
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
            rp_id: None,
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
        let cookie = rotate(&mut tx, a, &state, &current, true).await?;
        tx.commit().await?;
        auth.invalidate_user(current.0.session.user_id);
        let mut response = redirect(a.security());
        set_cookie(&mut response, cookie);
        let _ = super::csrf(&mut response, a)?;
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
    let mut tx = auth.db(a)?.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    let user = users::get_by_id(&mut tx, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let methods = enrollment_methods(&mut tx, &current).await?;
    let rp = a.rp_id(&state)?;
    let excluded = methods
        .iter()
        .filter(|m| m.rp_id.as_deref() == Some(rp))
        .filter_map(|m| m.credential_id.clone().map(Into::into))
        .collect::<Vec<_>>();
    let (challenge, ceremony) =
        auth.passkeys(a)?
            .start_registration(user.id.0, &user.username, &excluded, a.0)?;
    sessions::ceremony_put(
        &mut tx,
        current.0.session.id,
        user.id,
        "register",
        &serde_json::to_value(ceremony)?,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(challenge).into_response())
}
pub(crate) async fn passkey_finish(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Json(input): Json<RegisterPublicKeyCredential>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut c = auth.db(a)?.pool().acquire().await?;
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
    let key: Passkey = auth.passkeys(a)?.finish_registration(ceremony, &input)?;
    let method = mfa::Method {
        id: Uuid::now_v7(),
        kind: "passkey".into(),
        secret_enc: None,
        credential_id: Some(key.cred_id().as_slice().to_vec()),
        public_key: Some(serde_json::to_vec(&key)?),
        sign_count: 0,
        label: "Passkey".into(),
        last_used_step: None,
        rp_id: Some(a.rp_id(&state)?.to_owned()),
    };
    drop(c);
    let mut tx = auth.db(a)?.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    enrollment_methods(&mut tx, &current).await?;
    mfa::add(&mut tx, current.0.session.user_id, &method).await?;
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "mfa.enroll",
        Some(method.id),
        &json!({"kind":"passkey"}),
    )
    .await?;
    let cookie = rotate(&mut tx, a, &state, &current, true).await?;
    tx.commit().await?;
    auth.invalidate_user(current.0.session.user_id);
    let mut response = Json(json!({"next":a.security()})).into_response();
    set_cookie(&mut response, cookie);
    let _ = super::csrf(&mut response, a)?;
    Ok(response)
}
pub(crate) async fn remove(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let mut tx = auth.db(a)?.pool().begin().await?;
    users::lock_auth(&mut tx, current.0.session.user_id).await?;
    let rp = a.rp_id(&state)?;
    // Password-only access on another listener cannot remove this RP's factor.
    // Unbound legacy credentials are unusable and may still be cleaned up.
    if !mfa::list(&mut tx, current.0.session.user_id)
        .await?
        .iter()
        .any(|method| method.id == id && method.rp_id.as_deref().is_none_or(|id| id == rp))
    {
        return Err(HttpError::NotFound);
    }
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
    let cookie = rotate(&mut tx, a, &state, &current, false).await?;
    tx.commit().await?;
    auth.invalidate_user(current.0.session.user_id);
    let mut response = redirect(a.security());
    set_cookie(&mut response, cookie);
    let _ = super::csrf(&mut response, a)?;
    Ok(response)
}

// MFA enrollment/removal changes the session's privileges. Replace the bearer
// while preserving its original absolute lifetime and password-check timestamp.
async fn rotate(
    c: &mut PgConnection,
    a: Audience,
    state: &AppState,
    current: &Current,
    enrolled: bool,
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
    if enrolled {
        session.mfa_at = Some(time::OffsetDateTime::now_utc());
        for id in sessions::revoke_setup(c, old.user_id).await? {
            audit::write(
                c,
                Some(old.user_id),
                "session.revoke",
                Some(id),
                &json!({"outcome":"mfa_enrollment"}),
            )
            .await?;
        }
    }
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
