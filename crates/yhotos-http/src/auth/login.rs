use super::{
    AppState, Audience, Auth, ExposeSecret, HeaderMap, HttpError, IntoResponse, PgConnection,
    Request, Response, SecretString, Token, TokenKind, csrf, header, ip, next_path, now, redirect,
    required, sessions, set_cookie, token_hash, ua, views,
};
use crate::middleware::session::Current;
use axum::{
    Extension, Form, Json,
    extract::{FromRequest, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use yhotos_auth::{
    LockoutPolicy, LockoutState, PasswordHash,
    webauthn::{AuthenticationState, Passkey, PublicKeyCredential},
};
use yhotos_db::{audit, mfa, users};

#[derive(Default, Deserialize)]
pub(crate) struct Destination {
    #[serde(default)]
    next: String,
}
pub(crate) async fn form(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Option<Extension<Current>>,
    Query(query): Query<Destination>,
) -> Result<Response, HttpError> {
    if current.is_some() {
        return Ok(redirect(a.root()));
    }
    let _ = required(&state)?;
    views::login(a, next_path(&query.next).unwrap_or(""))
}
#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: SecretString,
    #[serde(default)]
    next: String,
}
pub(crate) fn failed() -> Response {
    (StatusCode::UNAUTHORIZED, "Sign in failed").into_response()
}
fn locked(seconds: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, seconds.to_string())],
        "Sign in failed",
    )
        .into_response()
}
pub(crate) async fn verify(
    auth: &Auth,
    password: SecretString,
    hash: Option<String>,
) -> Result<bool, HttpError> {
    let permit = auth
        .password_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| HttpError::BadRequest)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Some(hash) = hash.and_then(|v| PasswordHash::parse(&v).ok())
            && (8..=256).contains(&password.expose_secret().len())
        {
            return yhotos_auth::verify_password(&password, &hash).unwrap_or(false);
        }
        yhotos_auth::dummy_verify();
        false
    })
    .await
    .map_err(|_| HttpError::Internal(anyhow::anyhow!("password verification unavailable")))
}
#[allow(
    clippy::too_many_lines,
    reason = "keep the serialized password transaction and its audit outcomes together"
)]
pub(crate) async fn password(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Option<Extension<Current>>,
    request: Request,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let network = ip(&state, &request)?;
    let agent = ua(request.headers());
    let Form(input) = Form::<Credentials>::from_request(request, &state)
        .await
        .map_err(|_| HttpError::BadRequest)?;
    let username = input.username.trim().to_owned();
    if username.is_empty() || username.len() > 80 || username.chars().any(char::is_control) {
        return Ok(failed());
    }
    let mut tx = auth.db(a)?.pool().begin().await?;
    // Let citext perform case folding. Account IDs bind known-user lockouts
    // across every spelling accepted by the database, including Unicode.
    let mut user = users::get_by_username(&mut tx, &username).await?;
    let account_key = user.as_ref().map_or_else(
        || auth.fingerprint("username", username.to_lowercase()),
        |u| auth.fingerprint("account", u.id.0.as_bytes()),
    );
    let mut keys = [
        account_key.to_vec(),
        auth.fingerprint("ip-lock", network).to_vec(),
    ];
    keys.sort();
    // Serialize attempts across processes for both account and network. Bounded
    // Argon2 work runs on the blocking pool while these two row locks are held.
    let mut locks = Vec::new();
    for key in &keys {
        let value = sessions::lockout_get(&mut tx, key).await?;
        let lock: LockoutState = if value == json!({}) {
            LockoutState::default()
        } else {
            serde_json::from_value(value).map_err(|_| HttpError::BadRequest)?
        };
        if let Err(until) = lock.check(now()) {
            let seconds = until.0.saturating_sub(now()).as_secs() + 1;
            audit::write(
                &mut tx,
                None,
                "login.failure",
                None,
                &json!({"username":username,"ip_hash":network,"outcome":"locked"}),
            )
            .await?;
            tx.commit().await?;
            return Ok(locked(seconds));
        }
        locks.push(lock);
    }
    if let Some(found) = &user {
        users::lock_auth(&mut tx, found.id).await?;
        user = users::get_by_id(&mut tx, found.id).await?;
    }
    let valid = verify(
        auth,
        input.password,
        user.as_ref().map(|u| u.password_hash.clone()),
    )
    .await?;
    let user = user.filter(|u| valid && u.disabled_at.is_none() && (!a.0 || u.is_admin));
    if let Some(user) = user {
        for (key, mut lock) in keys.iter().zip(locks) {
            lock.record_success();
            sessions::lockout_set(&mut tx, key, &serde_json::to_value(lock)?).await?;
        }
        let methods = mfa::list(&mut tx, user.id).await?;
        let destination = next_path(&input.next);
        if !methods.is_empty() {
            let token = Token::generate();
            let hash = token.hash(TokenKind::PendingLogin);
            sessions::pending_create(
                &mut tx,
                uuid::Uuid::now_v7(),
                user.id,
                hash.as_bytes(),
                a.0,
                destination,
                blake3::hash(user.password_hash.as_bytes()).as_bytes(),
            )
            .await?;
            tx.commit().await?;
            let mut response = redirect(&format!("{}/mfa", a.login()));
            set_cookie(
                &mut response,
                a.cookie(
                    a.pending_cookie(),
                    &token.encode(TokenKind::PendingLogin),
                    false,
                )?,
            );
            return Ok(response);
        }
        let response = establish(
            &mut tx,
            auth,
            a,
            &state,
            &user,
            current.as_ref().map(|c| &c.0),
            &network,
            agent,
            None,
            None,
            destination,
        )
        .await?;
        tx.commit().await?;
        auth.invalidate_all();
        Ok(response)
    } else {
        let mut retry = None;
        for (key, mut lock) in keys.iter().zip(locks) {
            // Task 009 locks immediately after the fifth failure. The primitive's
            // default permits five free failures, so this flow permits four.
            lock.record_failure(
                now(),
                LockoutPolicy {
                    free_attempts: 4,
                    ..LockoutPolicy::default()
                },
            );
            if let Err(until) = lock.check(now()) {
                retry = Some(until.0.saturating_sub(now()).as_secs() + 1);
            }
            sessions::lockout_set(&mut tx, key, &serde_json::to_value(lock)?).await?;
        }
        audit::write(
            &mut tx,
            None,
            "login.failure",
            None,
            &json!({"username":username,"ip_hash":network,"outcome":"failed"}),
        )
        .await?;
        tx.commit().await?;
        Ok(retry.map_or_else(failed, locked))
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn establish(
    c: &mut PgConnection,
    auth: &Auth,
    a: Audience,
    state: &AppState,
    user: &users::UserRow,
    old: Option<&Current>,
    network: &[u8],
    agent: &str,
    reauth_at: Option<time::OffsetDateTime>,
    mfa_at: Option<time::OffsetDateTime>,
    destination: Option<&str>,
) -> Result<Response, HttpError> {
    if let Some(old) = old
        && let Some(hash) = sessions::revoke(c, old.0.session.user_id, old.0.session.id).await?
    {
        auth.invalidate(&hash);
        audit::write(
            c,
            Some(old.0.session.user_id),
            "session.revoke",
            Some(old.0.session.id),
            &json!({"outcome":"login_rotation"}),
        )
        .await?;
    }
    let token = Token::generate();
    let policy = if user.is_admin {
        yhotos_auth::SessionPolicy::admin()
    } else {
        yhotos_auth::SessionPolicy::default()
    };
    let now = time::OffsetDateTime::now_utc();
    let s = sessions::Session {
        id: uuid::Uuid::now_v7(),
        user_id: user.id,
        admin: a.0,
        created_at: now,
        last_seen_at: now,
        expires_at: now
            + time::Duration::try_from(policy.absolute).map_err(|_| HttpError::BadRequest)?,
        reauth_at: Some(reauth_at.unwrap_or(now)),
        mfa_at,
        ip_hash: network.to_vec(),
        ua_class: agent.to_owned(),
        token_hash: token.hash(TokenKind::Session).as_bytes().to_vec(),
    };
    sessions::create(c, &s).await?;
    users::touch_login(c, user.id).await?;
    audit::write(
        c,
        Some(user.id),
        "login.success",
        None,
        &json!({"username":user.username,"ip_hash":network,"outcome":"success"}),
    )
    .await?;
    let target = if user.must_change_password {
        a.password()
    } else if a.0 && mfa_at.is_none() {
        "/admin/setup-mfa"
    } else {
        destination.unwrap_or(a.root())
    };
    let mut response = redirect(target);
    set_cookie(
        &mut response,
        a.cookie(
            a.cookie_name(state),
            &token.encode(TokenKind::Session),
            false,
        )?,
    );
    set_cookie(&mut response, a.cookie(a.pending_cookie(), "", true)?);
    // Privilege changes get an independent double-submit token as well.
    let _ = csrf(&mut response, a)?;
    Ok(response)
}
pub(crate) async fn mfa_form(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    headers: HeaderMap,
    Extension(nonce): Extension<crate::middleware::security_headers::Nonce>,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let Some(hash) = token_hash(&headers, a.pending_cookie(), TokenKind::PendingLogin) else {
        return Ok(redirect(a.login()));
    };
    let mut tx = auth.db(a)?.pool().begin().await?;
    if sessions::pending_get(&mut tx, &hash, a.0).await?.is_none() {
        return Ok(redirect(a.login()));
    }
    views::mfa(a, nonce)
}
#[derive(Deserialize)]
struct Code {
    code: String,
}
pub(crate) async fn mfa_finish(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Option<Extension<Current>>,
    request: Request,
) -> Result<Response, HttpError> {
    let passkey = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("application/json"));
    finish(state, a, current, request, passkey).await
}
pub(crate) async fn passkey_finish(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    current: Option<Extension<Current>>,
    request: Request,
) -> Result<Response, HttpError> {
    finish(state, a, current, request, true).await
}
#[allow(
    clippy::too_many_lines,
    reason = "keep single-use MFA consumption and session creation in one transaction"
)]
async fn finish(
    state: AppState,
    a: Audience,
    current: Option<Extension<Current>>,
    request: Request,
    passkey: bool,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let network = ip(&state, &request)?;
    let agent = ua(request.headers());
    let Some(hash) = token_hash(
        request.headers(),
        a.pending_cookie(),
        TokenKind::PendingLogin,
    ) else {
        return Ok(failed());
    };
    let mut tx = auth.db(a)?.pool().begin().await?;
    let Some(pending) = sessions::pending_get(&mut tx, &hash, a.0).await? else {
        return Ok(failed());
    };
    users::lock_auth(&mut tx, pending.user_id).await?;
    let user = users::get_by_id(&mut tx, pending.user_id)
        .await?
        .filter(|u| u.disabled_at.is_none() && (!a.0 || u.is_admin));
    let password_current = user
        .as_ref()
        .zip(pending.password_version.as_ref())
        .is_some_and(|(u, v)| {
            bool::from(
                blake3::hash(u.password_hash.as_bytes())
                    .as_bytes()
                    .as_slice()
                    .ct_eq(v.as_slice()),
            )
        });
    let methods = mfa::list(&mut tx, pending.user_id).await?;
    if methods.iter().all(|m| m.kind == "email") {
        return Err(yhotos_auth::AuthError::Unsupported.into());
    }
    let valid = if passkey {
        // Consume before parsing: malformed responses consume the challenge too.
        let ceremony =
            sessions::ceremony_take(&mut tx, pending.id, pending.user_id, "authenticate").await?;
        let response = Json::<PublicKeyCredential>::from_request(request, &state).await;
        if let (Some(ceremony), Ok(Json(response))) = (ceremony, response) {
            let state: AuthenticationState = serde_json::from_value(ceremony)?;
            if let Ok(result) = auth.passkeys(a)?.finish_authentication(state, &response) {
                if let Some(m) = methods
                    .iter()
                    .find(|m| m.credential_id.as_deref() == Some(result.credential_id.as_slice()))
                {
                    let mut key: Passkey = serde_json::from_slice(
                        m.public_key.as_deref().ok_or(HttpError::BadRequest)?,
                    )?;
                    result.update_passkey(&mut key)?;
                    mfa::use_passkey(
                        &mut tx,
                        pending.user_id,
                        m,
                        i64::from(result.new_sign_count),
                        &serde_json::to_vec(&key)?,
                    )
                    .await?
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    } else if let Ok(Form(code)) = Form::<Code>::from_request(request, &state).await {
        let mut valid = false;
        for m in methods.iter().filter(|m| m.kind == "totp") {
            let secret = super::security::open_totp(
                auth,
                m.id,
                m.secret_enc.as_deref().ok_or(HttpError::BadRequest)?,
            )?;
            let last = m
                .last_used_step
                .map(u64::try_from)
                .transpose()
                .map_err(|_| HttpError::BadRequest)?;
            if let Ok(step) = yhotos_auth::totp::verify(&secret, &code.code, now(), last) {
                valid = mfa::use_totp(
                    &mut tx,
                    pending.user_id,
                    m.id,
                    i64::try_from(step).map_err(|_| HttpError::BadRequest)?,
                )
                .await?;
                if valid {
                    break;
                }
            }
        }
        valid
    } else {
        false
    };
    let username = user.as_ref().map(|u| u.username.clone());
    if let Some(user) = user.filter(|_| valid && password_current) {
        sessions::pending_delete(&mut tx, pending.id).await?;
        let response = establish(
            &mut tx,
            auth,
            a,
            &state,
            &user,
            current.as_ref().map(|c| &c.0),
            &network,
            agent,
            Some(pending.password_at),
            Some(time::OffsetDateTime::now_utc()),
            pending.next_path.as_deref().and_then(next_path),
        )
        .await?;
        tx.commit().await?;
        auth.invalidate_all();
        Ok(response)
    } else {
        sessions::pending_failure(&mut tx, pending.id).await?;
        audit::write(
            &mut tx,
            Some(pending.user_id),
            "login.failure",
            None,
            &json!({"username":username,"ip_hash":network,"outcome":"mfa_failed"}),
        )
        .await?;
        tx.commit().await?;
        let mut response = failed();
        if pending.attempts >= 4 {
            set_cookie(&mut response, a.cookie(a.pending_cookie(), "", true)?);
        }
        Ok(response)
    }
}
pub(crate) async fn passkey_start(
    State(state): State<AppState>,
    Extension(a): Extension<Audience>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let auth = required(&state)?;
    let Some(hash) = token_hash(&headers, a.pending_cookie(), TokenKind::PendingLogin) else {
        return Ok(failed());
    };
    let mut tx = auth.db(a)?.pool().begin().await?;
    let Some(p) = sessions::pending_get(&mut tx, &hash, a.0).await? else {
        return Ok(failed());
    };
    let methods = mfa::list(&mut tx, p.user_id).await?;
    let keys = methods
        .iter()
        .filter(|m| m.kind == "passkey")
        .map(|m| serde_json::from_slice::<Passkey>(m.public_key.as_deref().unwrap_or_default()))
        .collect::<Result<Vec<_>, _>>()?;
    let (challenge, ceremony) = auth.passkeys(a)?.start_authentication(&keys, a.0)?;
    sessions::ceremony_put(
        &mut tx,
        p.id,
        p.user_id,
        "authenticate",
        &serde_json::to_value(ceremony)?,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(challenge).into_response())
}
