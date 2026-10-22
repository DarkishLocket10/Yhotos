//! The only mapping of application failures to public responses.

use crate::middleware::request_id::REQUEST_ID;
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

/// Public errors conceal authorization state and infrastructure details.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// An unknown or concealed resource.
    #[error("not found")]
    NotFound,
    /// Rendered identically to a missing resource.
    #[error("forbidden")]
    Forbidden,
    /// Invalid external input, without echoing it.
    #[error("bad request")]
    BadRequest,
    /// A per-IP request budget was exhausted.
    #[error("rate limited")]
    RateLimited,
    /// Details are logged only, correlated by the generated request id.
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let mut response = match self {
            Self::NotFound | Self::Forbidden => {
                (StatusCode::NOT_FOUND, "Not found").into_response()
            }
            Self::BadRequest => (StatusCode::BAD_REQUEST, "Bad request").into_response(),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "1")],
                "Too many requests",
            )
                .into_response(),
            Self::Internal(error) => {
                let request_id = REQUEST_ID
                    .try_with(ToString::to_string)
                    .unwrap_or_else(|_| "unavailable".into());
                tracing::error!(%request_id, error = %format_args!("{error:#}"), "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Internal server error. Request id: {request_id}"),
                )
                    .into_response()
            }
        };
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        response
    }
}
