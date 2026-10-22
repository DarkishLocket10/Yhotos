//! Fixed header values are static. The sole dynamic header buffer contains the
//! nonce-bearing CSP; encoding the 16 random nonce bytes uses a stack buffer.

use axum::{
    extract::Request,
    http::{HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{fmt, io::Write};

const PREFIX: &str = "default-src 'none'; img-src 'self'; script-src 'nonce-";
const MIDDLE: &str = "'; style-src 'self' 'nonce-";
const SUFFIX: &str =
    "'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// A fresh, 128-bit random CSP nonce, encoded without a separate heap allocation.
#[derive(Clone, Copy)]
pub struct Nonce([u8; 24]);
impl Nonce {
    fn generate() -> Result<Self, crate::error::HttpError> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|e| {
            crate::error::HttpError::Internal(anyhow::anyhow!("nonce entropy unavailable: {e}"))
        })?;
        let mut encoded = [0; 24];
        STANDARD
            .encode_slice(random, &mut encoded)
            .map_err(|e| crate::error::HttpError::Internal(e.into()))?;
        Ok(Self(encoded))
    }
    fn policy(self) -> Result<HeaderValue, crate::error::HttpError> {
        const LEN: usize = PREFIX.len() + MIDDLE.len() + SUFFIX.len() + 48;
        let mut value = [0; LEN];
        let mut destination = value.as_mut_slice();
        for part in [
            PREFIX.as_bytes(),
            &self.0,
            MIDDLE.as_bytes(),
            &self.0,
            SUFFIX.as_bytes(),
        ] {
            destination
                .write_all(part)
                .map_err(|e| crate::error::HttpError::Internal(e.into()))?;
        }
        // Only HeaderValue owns a heap buffer. Both raw and encoded nonce and
        // the temporary policy are stack arrays; fixed headers borrow statics.
        HeaderValue::from_bytes(&value).map_err(|e| crate::error::HttpError::Internal(e.into()))
    }
}
impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Construction guarantees ASCII base64; no owned string is needed.
        let value = std::str::from_utf8(&self.0).map_err(|_| fmt::Error)?;
        f.write_str(value)
    }
}
/// Only asset namespaces may be embedded by another origin, even on errors.
#[must_use]
pub fn cross_origin_resource(path: &str) -> bool {
    ["/static/", "/i/", "/o/", "/v/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

pub(crate) async fn secure(mut request: Request, next: Next) -> Response {
    let cross_origin = cross_origin_resource(request.uri().path());
    let path = request.uri().path();
    let auth_page =
        path.starts_with("/login") || path.starts_with("/u/") || path.starts_with("/admin/");
    let share_page = path.starts_with("/s/");
    let nonce = Nonce::generate();
    let policy = nonce.as_ref().ok().and_then(|value| value.policy().ok());
    let mut response = match nonce {
        Ok(nonce) if policy.is_some() => {
            request.extensions_mut().insert(nonce);
            next.run(request).await
        }
        Ok(_) => crate::error::HttpError::Internal(anyhow::anyhow!("CSP encoding failed"))
            .into_response(),
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    headers.remove(header::SERVER);
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        policy.unwrap_or_else(|| {
            HeaderValue::from_static(
                "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            )
        }),
    );
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=63072000; includeSubDomains; preload"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert("permissions-policy", HeaderValue::from_static("accelerometer=(), ambient-light-sensor=(), autoplay=(), battery=(), bluetooth=(), browsing-topics=(), camera=(), clipboard-read=(), clipboard-write=(), display-capture=(), encrypted-media=(), fullscreen=(), gamepad=(), geolocation=(), gyroscope=(), hid=(), idle-detection=(), local-fonts=(), magnetometer=(), microphone=(), midi=(), otp-credentials=(), payment=(), picture-in-picture=(), publickey-credentials-create=(self), publickey-credentials-get=(self), screen-wake-lock=(), serial=(), speaker-selection=(), storage-access=(), usb=(), web-share=(), window-management=(), xr-spatial-tracking=()"));
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static(
            if cross_origin
                && headers
                    .get(axum::http::header::CACHE_CONTROL)
                    .is_none_or(|v| v.as_bytes() != b"private, no-store")
            {
                "cross-origin"
            } else {
                "same-origin"
            },
        ),
    );
    if auth_page {
        response.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if share_page {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        response.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
    }
    response
}
