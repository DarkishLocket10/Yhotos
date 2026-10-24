#![forbid(unsafe_code)]

//! Isolated public and admin routers, compiled templates and request controls.

#[cfg(feature = "admin")]
pub mod admin;
mod api;
pub mod auth;
pub mod error;
pub mod files;
pub mod middleware;
pub mod pages;
pub mod search;
mod share;
mod state;
pub mod zip;
pub use state::{AppResources, AppState};
mod static_assets;

use axum::{
    Extension, Router,
    http::{StatusCode, header},
    middleware::{from_fn, from_fn_with_state},
    response::IntoResponse,
    routing::get,
};
use error::HttpError;
use middleware::rate_limit::RateLimiter;
use std::time::Duration;
use tower_http::trace::TraceLayer;

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
    let router = gallery_routes()
        .merge(auth::routes(auth::Audience(false)))
        .merge(share::routes())
        .merge(api::routes())
        .route("/healthz", get(health).with_state(()))
        .route("/static/{*path}", get(static_assets::serve))
        .merge(file_routes());
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

fn gallery_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(pages::home))
        .route("/a/{*path}", get(pages::album))
        .route("/p/{id}", get(pages::photo))
        .route("/search", get(search::page))
        .route("/map", get(pages::map::page))
        .route("/map/tiles.json", get(pages::map::tiles))
        .route("/map/photos.json", get(pages::map::point_photos))
        .route("/places/{place_id}", get(pages::map::place))
        .route("/calendar", get(pages::calendar::page))
        .route("/calendar/{year}", get(pages::calendar::page))
        .route("/calendar/{year}/{month}", get(pages::calendar::page))
        .route("/calendar/{year}/{month}/{day}", get(pages::calendar::page))
        .route("/legacy/{*path}", get(pages::legacy))
        .route("/index.php", get(pages::legacy))
        .route("/picture.php", get(pages::legacy))
        .route_layer(from_fn(pages::privacy))
}
fn file_routes() -> Router<AppState> {
    Router::new()
        .route("/i/{hash}/{name}", get(files::derivative))
        .route("/o/{hash}/{name}", get(files::original))
        .route("/v/{hash}/scrub.vtt", get(files::index_response))
        .route("/v/{hash}/{name}", get(files::video))
        .route("/zip", get(zip::album).post(zip::selection))
}

/// Compiles only with `admin`, and must be served on the separate admin socket.
#[cfg(feature = "admin")]
pub fn admin_router(state: AppState) -> Router {
    admin::upload::with_method_override(stack(
        Router::new()
            .merge(gallery_routes())
            .merge(file_routes())
            .merge(api::routes())
            .merge(auth::routes(auth::Audience(true)))
            .merge(admin::upload::routes())
            .merge(share::admin_routes())
            .merge(search::people::routes())
            .route("/admin/healthz", get(health))
            .route("/admin/", get(health))
            .route("/admin/setup-mfa", get(auth::security::setup))
            .route("/static/{*path}", get(static_assets::serve)),
        state,
        auth::Audience(true),
    ))
}

fn stack(router: Router<AppState>, state: AppState, audience: auth::Audience) -> Router {
    let limits = RateLimiter::new(state.config.rate_limits, state.config.trust_proxy);
    router
        .fallback(|| async { HttpError::NotFound })
        .layer(from_fn_with_state(state.clone(), middleware::session::load))
        .layer(from_fn_with_state(state.clone(), middleware::csrf::protect))
        .layer(Extension(audience))
        .layer(from_fn_with_state(limits, middleware::rate_limit::limit))
        .layer(from_fn_with_state(
            (state.clone(), audience),
            request_limits,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(middleware::trace::span)
                .on_request(())
                .on_response(middleware::trace::response)
                .on_failure(()),
        )
        // Wrap short-circuiting layers as well as handlers, so 413/408/429 and
        // fallback responses have exactly the same security guarantees.
        .layer(from_fn(api::envelope))
        .layer(from_fn_with_state(
            state.clone(),
            middleware::security_headers::secure,
        ))
        .layer(from_fn(middleware::request_id::assign))
        .with_state(state)
}
async fn request_limits(
    axum::extract::State((state, audience)): axum::extract::State<(AppState, auth::Audience)>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let upload = cfg!(feature = "admin")
        && audience.0
        && request.method() == axum::http::Method::PATCH
        && request
            .uri()
            .path()
            .strip_prefix("/admin/upload/")
            .is_some_and(|id| id.parse::<uuid::Uuid>().is_ok());
    let cap = if upload {
        state.config.upload.chunk_bytes as usize
    } else {
        1024 * 1024
    };
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > cap as u64)
    {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Request too large").into_response();
    }
    let finish = cfg!(feature = "admin")
        && audience.0
        && request.method() == axum::http::Method::POST
        && request
            .uri()
            .path()
            .strip_prefix("/admin/upload/")
            .and_then(|path| path.strip_suffix("/finish"))
            .is_some_and(|id| id.parse::<uuid::Uuid>().is_ok());
    let deadline = if upload || finish {
        Duration::from_secs(u64::from(state.config.worker.codec_timeout) + 120)
    } else {
        Duration::from_secs(10)
    };
    let (parts, body) = request.into_parts();
    let body = if upload {
        body
    } else {
        axum::body::Body::new(http_body_util::Limited::new(body, cap))
    };
    tokio::time::timeout(
        deadline,
        next.run(axum::extract::Request::from_parts(parts, body)),
    )
    .await
    .unwrap_or_else(|_| (StatusCode::REQUEST_TIMEOUT, "Request timed out").into_response())
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
#[cfg(test)]
async fn test_form() -> Result<axum::response::Response, HttpError> {
    let token = middleware::csrf::CsrfToken::generate()?;
    Ok((
        [(header::SET_COOKIE, token.cookie(false)?)],
        axum::response::Html(format!(
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
