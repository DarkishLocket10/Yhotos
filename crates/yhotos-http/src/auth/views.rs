use super::{Audience, CsrfToken, HttpError, IntoResponse, Response, sessions, set_cookie};
use crate::middleware::security_headers::Nonce;
use askama::Template;
use axum::response::Html;
use std::fmt::Write;

#[derive(Template)]
#[template(path = "default/auth/login.html")]
struct Login<'a> {
    csrf: &'a str,
    action: &'a str,
    next: &'a str,
}
pub(super) fn login(a: Audience, next: &str) -> Result<Response, HttpError> {
    page(a, |csrf| {
        Login {
            csrf,
            action: a.login(),
            next,
        }
        .render()
        .map_err(|e| HttpError::Internal(e.into()))
    })
}
#[derive(Template)]
#[template(path = "default/auth/password.html")]
struct Password<'a> {
    csrf: &'a str,
    action: &'a str,
    forced: bool,
}
pub(super) fn password(a: Audience, forced: bool) -> Result<Response, HttpError> {
    page(a, |csrf| {
        Password {
            csrf,
            action: a.password(),
            forced,
        }
        .render()
        .map_err(|e| HttpError::Internal(e.into()))
    })
}
#[derive(Template)]
#[template(path = "default/auth/totp.html")]
struct Totp<'a> {
    csrf: &'a str,
    action: String,
    uri: &'a str,
    qr: &'a str,
}
pub(super) fn totp(a: Audience, uri: &str, qr: &str) -> Result<Response, HttpError> {
    page(a, |csrf| {
        Totp {
            csrf,
            action: format!("{}/totp", a.security()),
            uri,
            qr,
        }
        .render()
        .map_err(|e| HttpError::Internal(e.into()))
    })
}
#[derive(Template)]
#[template(path = "default/auth/mfa.html")]
struct Mfa<'a> {
    csrf: &'a str,
    action: String,
    script: &'static str,
    nonce: Nonce,
}
pub(super) fn mfa(a: Audience, nonce: Nonce) -> Result<Response, HttpError> {
    page(a, |csrf| {
        Mfa {
            csrf,
            action: format!("{}/mfa", a.login()),
            script: crate::static_assets::passkey_url()?,
            nonce,
        }
        .render()
        .map_err(|e| HttpError::Internal(e.into()))
    })
}
struct SessionView {
    id: uuid::Uuid,
    created: String,
    seen: String,
    ip: String,
    ua: String,
    current: bool,
}
struct MethodView {
    id: uuid::Uuid,
    label: String,
}
#[derive(Template)]
#[template(path = "default/auth/security.html")]
struct Security<'a> {
    csrf: &'a str,
    base: &'a str,
    logout: &'a str,
    sessions: Vec<SessionView>,
    methods: Vec<MethodView>,
    script: &'static str,
    nonce: Nonce,
    setup: bool,
}
pub(super) fn security(
    a: Audience,
    sessions: &[sessions::Session],
    methods: &[yhotos_db::mfa::Method],
    id: uuid::Uuid,
    nonce: Nonce,
) -> Result<Response, HttpError> {
    page(a, |csrf| {
        Security {
            csrf,
            base: a.security(),
            logout: if a.0 { "/admin/logout" } else { "/logout" },
            sessions: sessions
                .iter()
                .map(|s| SessionView {
                    id: s.id,
                    created: s.created_at.to_string(),
                    seen: s.last_seen_at.to_string(),
                    ip: s.ip_hash.iter().take(4).fold(String::new(), |mut text, b| {
                        let _ = write!(&mut text, "{b:02x}");
                        text
                    }),
                    ua: s.ua_class.clone(),
                    current: s.id == id,
                })
                .collect(),
            methods: methods
                .iter()
                .map(|m| MethodView {
                    id: m.id,
                    label: m.label.clone(),
                })
                .collect(),
            script: crate::static_assets::passkey_url()?,
            nonce,
            setup: false,
        }
        .render()
        .map_err(|e| HttpError::Internal(e.into()))
    })
}
#[cfg(feature = "admin")]
pub(super) fn setup(a: Audience, nonce: Nonce) -> Result<Response, HttpError> {
    page(a, |csrf| {
        Security {
            csrf,
            base: a.security(),
            logout: "/admin/logout",
            sessions: Vec::new(),
            methods: Vec::new(),
            script: crate::static_assets::passkey_url()?,
            nonce,
            setup: true,
        }
        .render()
        .map_err(|e| HttpError::Internal(e.into()))
    })
}
fn page(
    a: Audience,
    render: impl FnOnce(&str) -> Result<String, HttpError>,
) -> Result<Response, HttpError> {
    let token = CsrfToken::generate()?;
    let html = render(token.form_value())?;
    let mut response = Html(html).into_response();
    set_cookie(&mut response, token.cookie(a.0)?);
    Ok(response)
}
