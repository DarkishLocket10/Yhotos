//! Ignores client correlation ids and scopes errors to a fresh server `UUIDv7`.

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use uuid::Uuid;

tokio::task_local! { pub(crate) static REQUEST_ID: Uuid; }

pub(crate) async fn assign(mut request: Request, next: Next) -> Response {
    let id = Uuid::now_v7();
    let mut buffer = Uuid::encode_buffer();
    if let Ok(header) = HeaderValue::from_str(id.hyphenated().encode_lower(&mut buffer)) {
        request.headers_mut().insert("x-request-id", header.clone());
        request.extensions_mut().insert(id);
        let mut response = REQUEST_ID.scope(id, next.run(request)).await;
        response.headers_mut().insert("x-request-id", header);
        response
    } else {
        // A UUID is always header-safe; fail closed if that invariant changes.
        crate::error::HttpError::Internal(anyhow::anyhow!("request id encoding failed"))
            .into_response()
    }
}
use axum::response::IntoResponse;
