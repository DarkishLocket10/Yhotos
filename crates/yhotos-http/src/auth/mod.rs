//! Password and MFA HTTP ceremonies with opaque cookies and server-side state.
mod login;
pub(crate) mod security;
mod views;

use crate::{
    AppState,
    error::HttpError,
    middleware::{csrf::CsrfToken, rate_limit::client_ip},
};
use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use moka::sync::Cache;
use secrecy::{ExposeSecret, SecretString};
use std::{sync::Arc, time::Duration};
use yhotos_auth::{Keyring, Token, TokenKind, webauthn::Passkeys};
pub(crate) use yhotos_db::PgConnection;
use yhotos_db::{Db, sessions};

/// Shared cryptographic configuration and bounded, thirty-second session cache.
pub struct Auth {
    public_db: Arc<Db>,
    admin_db: Option<Arc<Db>>,
    pub(crate) cache: Cache<Vec<u8>, (u64, Arc<sessions::Loaded>)>,
    generation: std::sync::atomic::AtomicU64,
    pub(crate) keys: Keyring,
    public_passkeys: Passkeys,
    admin_passkeys: Option<Passkeys>,
    cache_ready: std::sync::atomic::AtomicBool,
    listener_stop: Arc<tokio::sync::Notify>,
    pub(crate) password_slots: Arc<tokio::sync::Semaphore>,
    fingerprint_key: [u8; 32],
}
impl Auth {
    /// Validates cryptographic settings once; no secrets enter error messages.
    ///
    /// # Errors
    /// Rejects invalid key, relying-party configuration or LISTEN connection.
    pub async fn new(
        config: &yhotos_config::Config,
        public_db: Arc<Db>,
        admin_db: Option<Arc<Db>>,
    ) -> Result<Arc<Self>, HttpError> {
        // Configuration permits keys longer than 32 bytes. Derive a fixed-size,
        // domain-separated encryption key instead of truncating key material.
        use base64::{Engine, engine::general_purpose::STANDARD};
        let raw = zeroize::Zeroizing::new(
            STANDARD
                .decode(config.session_key.expose_secret())
                .map_err(|_| HttpError::BadRequest)?,
        );
        let derived = zeroize::Zeroizing::new(blake3::derive_key("yhotos MFA encryption v1", &raw));
        let master = SecretString::from(STANDARD.encode(derived.as_ref()));
        let public_passkeys = passkeys_for(&config.public_base_url)?;
        let admin_passkeys = config
            .admin_base_url
            .as_ref()
            .map(passkeys_for)
            .transpose()?;
        if admin_db.is_some() && admin_passkeys.is_none() {
            return Err(HttpError::BadRequest);
        }
        let listener = sessions::Invalidations::connect(&public_db).await?;
        let auth = Arc::new(Self {
            public_db,
            admin_db,
            generation: std::sync::atomic::AtomicU64::new(0),
            cache: Cache::builder()
                .max_capacity(16_384)
                .time_to_live(Duration::from_secs(30))
                .build(),
            keys: Keyring::from_master(&master)?,
            public_passkeys,
            admin_passkeys,
            cache_ready: std::sync::atomic::AtomicBool::new(true),
            listener_stop: Arc::new(tokio::sync::Notify::new()),
            password_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            fingerprint_key: blake3::derive_key("yhotos login fingerprint v1", &raw),
        });
        Self::listen(&auth, listener);
        let weak = Arc::downgrade(&auth);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let Some(auth) = weak.upgrade() else { break };
                if let Ok(mut c) = auth.public_db.pool().acquire().await {
                    let _ = sessions::cleanup(&mut c).await;
                }
            }
        });
        Ok(auth)
    }
    /// Invalidates local cache entries immediately after transactional revocation.
    pub fn invalidate(&self, hash: &[u8]) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cache.invalidate(hash);
    }
    /// Loads cached session context, issuing exactly one query on a miss.
    ///
    /// # Errors
    /// Returns a generic infrastructure error if the query fails.
    pub async fn load(&self, hash: &[u8]) -> Result<Option<Arc<sessions::Loaded>>, HttpError> {
        let generation = self.generation.load(std::sync::atomic::Ordering::SeqCst);
        if self.cache_ready.load(std::sync::atomic::Ordering::SeqCst)
            && let Some((version, s)) = self.cache.get(hash)
            && version == generation
        {
            return Ok(Some(s));
        }
        let mut c = self.public_db.pool().acquire().await?;
        let loaded = sessions::load(&mut c, hash).await?.map(Arc::new);
        if let Some(s) = &loaded
            && self.cache_ready.load(std::sync::atomic::Ordering::SeqCst)
            && generation == self.generation.load(std::sync::atomic::Ordering::SeqCst)
        {
            self.cache.insert(hash.to_vec(), (generation, s.clone()));
        }
        Ok(loaded)
    }
    pub(crate) fn invalidate_all(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cache.invalidate_all();
    }
    pub(crate) fn db(&self, a: Audience) -> Result<&Db, HttpError> {
        if a.0 {
            self.admin_db.as_deref().ok_or(HttpError::NotFound)
        } else {
            Ok(&self.public_db)
        }
    }
    pub(crate) fn passkeys(&self, a: Audience) -> Result<&Passkeys, HttpError> {
        if a.0 {
            self.admin_passkeys.as_ref().ok_or(HttpError::NotFound)
        } else {
            Ok(&self.public_passkeys)
        }
    }
    fn listen(auth: &Arc<Self>, mut listener: sessions::Invalidations) {
        let weak = Arc::downgrade(auth);
        let stop = auth.listener_stop.clone();
        tokio::spawn(async move {
            loop {
                // Do not keep Auth (and its pools) alive while waiting for messages.
                let result = tokio::select! {
                    () = stop.notified() => break,
                    result = listener.recv() => result,
                };
                let Some(auth) = weak.upgrade() else { break };
                if result.is_ok() {
                    auth.invalidate_all();
                    continue;
                }
                auth.cache_ready
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                auth.invalidate_all();
                // Re-LISTEN before enabling cache reads again; missed
                // notifications during a disconnect can never revive entries.
                loop {
                    match sessions::Invalidations::connect(&auth.public_db).await {
                        Ok(new) => {
                            listener = new;
                            auth.invalidate_all();
                            auth.cache_ready
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                        Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                    }
                }
            }
        });
    }
    fn fingerprint(&self, domain: &str, value: impl AsRef<[u8]>) -> [u8; 32] {
        let mut h = blake3::Hasher::new_keyed(&self.fingerprint_key);
        h.update(domain.as_bytes());
        h.update(&[0]);
        h.update(value.as_ref());
        *h.finalize().as_bytes()
    }
}
impl Drop for Auth {
    fn drop(&mut self) {
        self.listener_stop.notify_one();
    }
}
fn passkeys_for(url: &yhotos_auth::webauthn::Url) -> Result<Passkeys, HttpError> {
    Passkeys::new(
        url.host_str().ok_or(HttpError::BadRequest)?,
        &url.origin().ascii_serialization(),
    )
    .map_err(Into::into)
}
pub(crate) fn required(state: &AppState) -> Result<&Arc<Auth>, HttpError> {
    state
        .auth
        .as_ref()
        .ok_or_else(|| HttpError::Internal(anyhow::anyhow!("authentication unavailable")))
}

/// Listener context is supplied by router construction, never request input.
#[derive(Clone, Copy)]
pub struct Audience(pub bool);
impl Audience {
    pub(crate) fn base_url(self, state: &AppState) -> Option<&yhotos_auth::webauthn::Url> {
        if self.0 {
            state.config.admin_base_url.as_ref()
        } else {
            Some(&state.config.public_base_url)
        }
    }
    pub(crate) fn root(self) -> &'static str {
        if self.0 { "/admin/" } else { "/" }
    }
    pub(crate) fn login(self) -> &'static str {
        if self.0 { "/admin/login" } else { "/login" }
    }
    pub(crate) fn password(self) -> &'static str {
        if self.0 {
            "/admin/u/password"
        } else {
            "/u/password"
        }
    }
    pub(crate) fn security(self) -> &'static str {
        if self.0 {
            "/admin/u/security"
        } else {
            "/u/security"
        }
    }
    pub(crate) fn pending_cookie(self) -> &'static str {
        if self.0 { "yac" } else { "yc" }
    }
    pub(crate) fn cookie_name(self, state: &AppState) -> &'static str {
        if self.0 {
            "ya"
        } else if state.config.public_base_url.path() == "/" {
            "__Host-ys"
        } else {
            "ys"
        }
    }
    pub(crate) fn cookie(
        self,
        name: &str,
        value: &str,
        clear: bool,
    ) -> Result<HeaderValue, HttpError> {
        HeaderValue::from_str(&format!(
            "{name}={value}; Path=/; Secure; HttpOnly; SameSite={}{}",
            if self.0 { "Strict" } else { "Lax" },
            if clear { "; Max-Age=0" } else { "" }
        ))
        .map_err(|e| HttpError::Internal(e.into()))
    }
}
pub(crate) fn routes(a: Audience) -> Router<AppState> {
    let prefix = if a.0 { "/admin" } else { "" };
    Router::new()
        .route(
            &format!("{prefix}/login"),
            get(login::form).post(login::password),
        )
        .route(
            &format!("{prefix}/login/mfa"),
            get(login::mfa_form).post(login::mfa_finish),
        )
        .route(
            &format!("{prefix}/login/mfa/passkey/start"),
            post(login::passkey_start),
        )
        .route(
            &format!("{prefix}/login/mfa/passkey/finish"),
            post(login::passkey_finish),
        )
        .route(&format!("{prefix}/logout"), post(security::logout))
        .route(
            &format!("{prefix}/u/password"),
            get(security::password_form).post(security::password),
        )
        .route(&format!("{prefix}/u/security"), get(security::index))
        .route(
            &format!("{prefix}/u/security/reauth"),
            post(security::reauth),
        )
        .route(
            &format!("{prefix}/u/security/sessions/{{id}}/revoke"),
            post(security::revoke),
        )
        .route(
            &format!("{prefix}/u/security/totp"),
            get(security::totp_form).post(security::totp_finish),
        )
        .route(
            &format!("{prefix}/u/security/passkey/start"),
            post(security::passkey_start),
        )
        .route(
            &format!("{prefix}/u/security/passkey/finish"),
            post(security::passkey_finish),
        )
        .route(
            &format!("{prefix}/u/security/mfa/{{id}}/remove"),
            post(security::remove),
        )
}
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut found = None;
    for h in headers.get_all(header::COOKIE) {
        for pair in h.to_str().ok()?.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=')
                && key == name
            {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}
pub(crate) fn token_hash(headers: &HeaderMap, name: &str, kind: TokenKind) -> Option<Vec<u8>> {
    let (actual, t) = Token::decode(cookie(headers, name)?).ok()?;
    (actual == kind).then(|| t.hash(kind).as_bytes().to_vec())
}
pub(crate) fn now() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}
pub(crate) fn next_path(value: &str) -> Option<&str> {
    (value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.contains('%')
        && !value.chars().any(char::is_control))
    .then_some(value)
}
pub(crate) fn redirect(path: &str) -> Response {
    Redirect::to(path).into_response()
}
pub(crate) fn set_cookie(response: &mut Response, cookie: HeaderValue) {
    response.headers_mut().append(header::SET_COOKIE, cookie);
}
pub(crate) fn csrf(response: &mut Response, a: Audience) -> Result<String, HttpError> {
    let token = CsrfToken::generate()?;
    set_cookie(response, token.cookie(a.0)?);
    Ok(token.form_value().to_owned())
}
pub(crate) fn ip(state: &AppState, request: &Request) -> Result<[u8; 32], HttpError> {
    Ok(required(state)?.fingerprint(
        "ip",
        client_ip(request, state.config.trust_proxy)?.to_string(),
    ))
}
pub(crate) fn ua(headers: &HeaderMap) -> &'static str {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ua.contains("Android") || ua.contains("iPhone") {
        "mobile"
    } else if ua.contains("Mozilla") {
        "browser"
    } else {
        "other"
    }
}
