//! The only mapping of application failures to public responses.

use crate::middleware::request_id::REQUEST_ID;
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

/// Public errors conceal authorization state and infrastructure details.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// Upload bytes remain durable and completion can be retried after recovery.
    #[error("upload temporarily unavailable")]
    UploadUnavailable,
    /// Generic tus protocol rejection, with no external input echoed.
    #[error("upload rejected")]
    Upload(StatusCode),
    /// Reserved MFA methods are not enabled.
    #[error("unsupported authentication method")]
    Unsupported,
    /// Double-submit or same-origin validation failed.
    #[error("request rejected")]
    Csrf,
    /// The temporary password must be changed first.
    #[error("password change required")]
    PasswordChangeRequired,
    /// Factor removal needs a fresh password check.
    #[error("reauthentication required")]
    ReauthRequired,
    /// Invalid authentication input.
    #[error("sign in failed")]
    Authentication,
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
            Self::UploadUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "30")],
                "Upload temporarily unavailable. Retry later.",
            )
                .into_response(),
            Self::Upload(status) => (status, "Upload rejected").into_response(),
            Self::Unsupported => (
                StatusCode::NOT_IMPLEMENTED,
                "Authentication method unavailable",
            )
                .into_response(),
            Self::Csrf => (StatusCode::FORBIDDEN, "Request rejected").into_response(),
            Self::Authentication => (StatusCode::BAD_REQUEST, "Sign in failed").into_response(),
            Self::PasswordChangeRequired => (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({"error":"password_change_required"})),
            )
                .into_response(),
            Self::ReauthRequired => {
                (StatusCode::FORBIDDEN, "Verify your password first").into_response()
            }
            Self::NotFound | Self::Forbidden => crate::pages::not_found(),
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

impl From<yhotos_db::DbError> for HttpError {
    fn from(_: yhotos_db::DbError) -> Self {
        Self::Internal(anyhow::anyhow!("database operation failed"))
    }
}
impl From<yhotos_auth::AuthError> for HttpError {
    fn from(error: yhotos_auth::AuthError) -> Self {
        if error == yhotos_auth::AuthError::Unsupported {
            Self::Unsupported
        } else {
            Self::Authentication
        }
    }
}
impl From<serde_json::Error> for HttpError {
    fn from(_: serde_json::Error) -> Self {
        Self::BadRequest
    }
}

impl From<yhotos_db::PoolError> for HttpError {
    fn from(_: yhotos_db::PoolError) -> Self {
        Self::Internal(anyhow::anyhow!("database operation failed"))
    }
}
