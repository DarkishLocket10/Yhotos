//! Records only the correlation id, method, route path, status and latency.

use axum::{
    body::Body,
    http::{Request, Response},
};
use std::time::Duration;
use tracing::Span;
use uuid::Uuid;

pub(crate) fn span(request: &Request<Body>) -> Span {
    let id = request
        .extensions()
        .get::<Uuid>()
        .copied()
        .unwrap_or_default();
    // Matched routes avoid logging share tokens and unknown, attacker-controlled
    // paths; query strings, headers, and bodies are deliberately never recorded.
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("<unmatched>", axum::extract::MatchedPath::as_str);
    tracing::info_span!("http", request_id = %id, method = %request.method(), path, status = tracing::field::Empty, latency_us = tracing::field::Empty)
}
pub(crate) fn response(response: &Response<Body>, latency: Duration, span: &Span) {
    span.record("status", response.status().as_u16());
    span.record("latency_us", latency.as_micros());
    tracing::info!(parent: span, "response");
}
