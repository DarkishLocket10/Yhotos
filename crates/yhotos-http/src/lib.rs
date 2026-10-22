#![forbid(unsafe_code)]

//! Isolated public and admin routers, compiled templates and request controls.

pub mod auth;
pub mod error;
pub mod files;
pub mod middleware;
mod state;
pub use state::{AppResources, AppState};
mod static_assets;

use askama::Template;
use axum::{
    Extension, Router,
    extract::State,
    http::{StatusCode, header},
    middleware::{from_fn, from_fn_with_state},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use error::HttpError;
use middleware::{rate_limit::RateLimiter, security_headers::Nonce};
use std::time::Duration;
use tower_http::{limit::RequestBodyLimitLayer, timeout::TimeoutLayer, trace::TraceLayer};

/// Builds only public routes, even when the admin feature is enabled.
///
/// The serving transport must supply `ConnectInfo<SocketAddr>`.
///
#[cfg_attr(
    not(feature = "admin"),
    doc = "With default features the admin router does not exist:
```compile_fail
use yhotos_http::admin_router;
```"
)]
pub fn public_router(state: AppState) -> Router {
    let files = Router::new()
        .route("/i/{hash}/{name}", get(files::derivative))
        .route("/o/{hash}/{name}", get(files::original))
        .route("/v/{hash}/{name}", get(files::video));
    let router = Router::new()
        .merge(auth::routes(auth::Audience(false)))
        .route("/", get(home))
        .route("/healthz", get(health).with_state(()))
        .route("/static/{*path}", get(static_assets::serve))
        .merge(files);
    #[cfg(test)]
    let router = router
        .route(
            "/__test/internal",
            get(|| async {
                HttpError::Internal(
                    anyhow::anyhow!("private database detail").context("private operation"),
                )
            }),
        )
        .route("/__test/forbidden", get(|| async { HttpError::Forbidden }))
        .route("/__test/not-found", get(|| async { HttpError::NotFound }))
        .route(
            "/__test/server",
            get(|| async { ([(header::SERVER, "private server")], "ok") }),
        )
        .route("/__test/form", get(test_form).post(test_submit))
        .route(
            "/__test/body",
            axum::routing::post(|_: axum::body::Bytes| async { "read" }),
        )
        .route(
            "/__test/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(20)).await;
                "late"
            }),
        );
    stack(router, state, auth::Audience(false))
}

/// Compiles only with `admin`, and must be served on the separate admin socket.
#[cfg(feature = "admin")]
pub fn admin_router(state: AppState) -> Router {
    stack(
        Router::new()
            .merge(auth::routes(auth::Audience(true)))
            .route("/admin/healthz", get(health))
            .route("/admin/", get(health))
            .route("/admin/setup-mfa", get(auth::security::setup))
            .route("/static/{*path}", get(static_assets::serve)),
        state,
        auth::Audience(true),
    )
}

fn stack(router: Router<AppState>, state: AppState, audience: auth::Audience) -> Router {
    let limits = RateLimiter::new(state.config.rate_limits, state.config.trust_proxy);
    router
        .fallback(|| async { HttpError::NotFound })
        .layer(from_fn_with_state(state.clone(), middleware::session::load))
        .layer(from_fn_with_state(state.clone(), middleware::csrf::protect))
        .layer(Extension(audience))
        .layer(from_fn_with_state(limits, middleware::rate_limit::limit))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(10),
        ))
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(middleware::trace::span)
                .on_request(())
                .on_response(middleware::trace::response)
                .on_failure(()),
        )
        // Wrap short-circuiting layers as well as handlers, so 413/408/429 and
        // fallback responses have exactly the same security guarantees.
        .layer(from_fn(middleware::security_headers::secure))
        .layer(from_fn(middleware::request_id::assign))
        .with_state(state)
}
async fn health() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "ok",
    )
}
#[derive(Template)]
#[template(path = "default/home.html")]
struct HomeTemplate<'a> {
    nonce: Nonce,
    stylesheet: &'a str,
    script: &'a str,
}
async fn home(
    State(_state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
) -> Result<Response, HttpError> {
    let html = HomeTemplate {
        nonce,
        stylesheet: static_assets::stylesheet_url()?,
        script: static_assets::script_url()?,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))?;
    Ok((
        [(
            header::CACHE_CONTROL,
            "public, max-age=60, stale-while-revalidate=600",
        )],
        Html(html),
    )
        .into_response())
}
#[cfg(test)]
async fn test_form() -> Result<Response, HttpError> {
    let token = middleware::csrf::CsrfToken::generate()?;
    Ok((
        [(header::SET_COOKIE, token.cookie(false)?)],
        Html(format!(
            "<form method=post><input name=_csrf value=\"{}\"></form>",
            token.form_value()
        )),
    )
        .into_response())
}
#[cfg(test)]
async fn test_submit(
    axum::Form(fields): axum::Form<std::collections::BTreeMap<String, String>>,
) -> StatusCode {
    if fields.contains_key("_csrf") {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    }
}

#[cfg(test)]
mod tests;
