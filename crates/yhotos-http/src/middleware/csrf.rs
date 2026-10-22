//! Double-submit form protection. The host-only cookie is Secure and `HttpOnly`;
//! templates receive the token directly rather than reading it with JavaScript.

use crate::error::HttpError;
use axum::{
    Form,
    body::{Body, to_bytes},
    extract::{FromRequest, Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use subtle::ConstantTimeEq;

const COOKIE: &str = "__Host-yhotos-csrf";

/// A canonical 256-bit form token; its secret value is never Debug-printed.
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
    /// Reuses a single valid cookie so rendering another page preserves open forms.
    pub(crate) fn for_page(headers: &HeaderMap) -> Result<Self, HttpError> {
        match cookie_token(headers).filter(|value| canonical_token(value)) {
            Some(value) => Ok(Self(value.to_owned())),
            None => Self::generate(),
        }
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
fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
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
pub(crate) async fn protect(
    State(state): State<crate::AppState>,
    request: Request,
    next: Next,
) -> Response {
    let state_changing = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if !state_changing {
        return next.run(request).await;
    }
    if request.uri().path().starts_with("/api/v1/") {
        let headers = request.headers();
        let single = |name: &str| {
            let mut values = headers.get_all(name).iter();
            let value = values.next().and_then(|v| v.to_str().ok());
            if values.next().is_some() { None } else { value }
        };
        let expected = state.config.public_base_url.origin().ascii_serialization();
        let valid = single("x-requested-with") == Some("yhotos")
            && (!headers.contains_key("sec-fetch-site")
                || single("sec-fetch-site") == Some("same-origin"))
            && (!headers.contains_key(header::ORIGIN)
                || single("origin") == Some(expected.as_str()));
        return if valid {
            next.run(request).await
        } else {
            HttpError::Csrf.into_response()
        };
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if content_type.split(';').next() == Some("application/json")
        || request.uri().path().starts_with("/api/")
        || request.uri().path().starts_with("/admin/api/")
    {
        let audience = request.extensions().get::<crate::auth::Audience>().copied();
        let Some(url) = audience.and_then(|a| a.base_url(&state)) else {
            return HttpError::Csrf.into_response();
        };
        let expected = url.origin().ascii_serialization();
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        let site = request
            .headers()
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok());
        let token = request
            .headers()
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok());
        let valid = site == Some("same-origin")
            && origin == Some(expected.as_str())
            && token
                .zip(cookie_token(request.headers()))
                .is_some_and(|(t, c)| {
                    canonical_token(t) && bool::from(t.as_bytes().ct_eq(c.as_bytes()))
                });
        return if valid {
            next.run(request).await
        } else {
            HttpError::Csrf.into_response()
        };
    }
    // No multipart forms exist yet: reject unsupported forms rather than skip
    // validation. JSON API authorization is supplied with those future routes.
    if content_type.split(';').next() != Some("application/x-www-form-urlencoded") {
        return HttpError::Csrf.into_response();
    }
    let Some(cookie) = cookie_token(request.headers())
        .filter(|s| canonical_token(s))
        .map(str::to_owned)
    else {
        return HttpError::Csrf.into_response();
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, 1024 * 1024).await else {
        return HttpError::Csrf.into_response();
    };
    let mut copy = Request::new(Body::from(bytes.clone()));
    *copy.method_mut() = Method::POST;
    copy.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let Ok(Form(token)) = Form::<FormToken>::from_request(copy, &()).await else {
        return HttpError::Csrf.into_response();
    };
    if !canonical_token(&token.token)
        || !bool::from(cookie.as_bytes().ct_eq(token.token.as_bytes()))
    {
        return HttpError::Csrf.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{CsrfToken, HeaderMap, HeaderValue, canonical_token, header};

    #[test]
    fn pages_reuse_only_a_single_canonical_cookie() {
        let valid = CsrfToken::generate().unwrap();
        let cookie = format!("__Host-yhotos-csrf={}", valid.form_value());
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        assert_eq!(
            CsrfToken::for_page(&headers).unwrap().form_value(),
            valid.form_value()
        );

        let mut noncanonical = valid.form_value().to_owned();
        noncanonical.pop();
        noncanonical.push('B'); // Nonzero unused bits in the final base64 symbol.
        for value in [
            String::new(),
            "malformed".into(),
            format!("{}=", valid.form_value()),
            noncanonical,
            format!("{}; {cookie}", valid.form_value()),
        ] {
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("__Host-yhotos-csrf={value}")).unwrap(),
            );
            let token = CsrfToken::for_page(&headers).unwrap();
            assert!(canonical_token(token.form_value()));
            assert_ne!(token.form_value(), valid.form_value());
        }
        headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        headers.append(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        assert_ne!(
            CsrfToken::for_page(&headers).unwrap().form_value(),
            valid.form_value()
        );
        assert!(canonical_token(
            CsrfToken::for_page(&HeaderMap::new()).unwrap().form_value()
        ));
    }
}
