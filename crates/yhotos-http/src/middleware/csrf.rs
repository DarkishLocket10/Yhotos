//! Double-submit form protection. The host-only cookie is Secure and `HttpOnly`;
//! templates receive the token directly rather than reading it with JavaScript.

use crate::error::HttpError;
use axum::{
    Form,
    body::{Body, to_bytes},
    extract::{FromRequest, Request},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use subtle::ConstantTimeEq;

const COOKIE: &str = "__Host-yhotos-csrf";

/// A fresh random form token; its secret value is never Debug-printed.
#[derive(Clone)]
pub struct CsrfToken(String);
impl CsrfToken {
    /// Generates 256 random bits for a future form's hidden token field.
    ///
    /// # Errors
    /// Fails closed if OS entropy is unavailable.
    pub fn generate() -> Result<Self, HttpError> {
        let mut random = [0; 32];
        getrandom::fill(&mut random)
            .map_err(|e| HttpError::Internal(anyhow::anyhow!("CSRF entropy unavailable: {e}")))?;
        Ok(Self(URL_SAFE_NO_PAD.encode(random)))
    }
    /// Borrows the token solely for rendering the form's hidden field.
    #[must_use]
    pub fn form_value(&self) -> &str {
        &self.0
    }
    /// Produces a host-only cookie; admin forms use the stricter same-site mode.
    ///
    /// # Errors
    /// Returns an internal error if a generated token is not header-safe.
    pub fn cookie(&self, admin: bool) -> Result<HeaderValue, HttpError> {
        let same_site = if admin { "Strict" } else { "Lax" };
        HeaderValue::from_str(&format!(
            "{COOKIE}={}; Path=/; Secure; HttpOnly; SameSite={same_site}",
            self.0
        ))
        .map_err(|e| HttpError::Internal(e.into()))
    }
}
#[derive(Deserialize)]
struct FormToken {
    #[serde(rename = "_csrf")]
    token: String,
}
fn cookie_token(request: &Request) -> Option<&str> {
    let mut found = None;
    for header in request.headers().get_all(header::COOKIE) {
        for pair in header.to_str().ok()?.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == COOKIE {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}
fn canonical_token(value: &str) -> bool {
    // The decoder reserves the rounded-up base64 output size (33), even
    // though canonical 43-character tokens contain exactly 32 bytes.
    let mut bytes = [0; 33];
    value.len() == 43
        && URL_SAFE_NO_PAD
            .decode_slice(value, &mut bytes)
            .is_ok_and(|len| len == 32)
}
pub(crate) async fn protect(request: Request, next: Next) -> Response {
    let state_changing = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if !state_changing
        || request.uri().path().starts_with("/api/")
        || request.uri().path().starts_with("/admin/api/")
    {
        return next.run(request).await;
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    // No multipart forms exist yet: reject unsupported forms rather than skip
    // validation. JSON API authorization is supplied with those future routes.
    if content_type.split(';').next() != Some("application/x-www-form-urlencoded") {
        return HttpError::BadRequest.into_response();
    }
    let Some(cookie) = cookie_token(&request)
        .filter(|s| canonical_token(s))
        .map(str::to_owned)
    else {
        return HttpError::BadRequest.into_response();
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, 1024 * 1024).await else {
        return HttpError::BadRequest.into_response();
    };
    let mut copy = Request::new(Body::from(bytes.clone()));
    *copy.method_mut() = Method::POST;
    copy.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let Ok(Form(token)) = Form::<FormToken>::from_request(copy, &()).await else {
        return HttpError::BadRequest.into_response();
    };
    if !canonical_token(&token.token)
        || !bool::from(cookie.as_bytes().ct_eq(token.token.as_bytes()))
    {
        return HttpError::BadRequest.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}
