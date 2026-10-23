//! Visibility-filtered JSON islands and exactly-attributed interactions.
mod interactions;
pub(crate) use interactions::flag_once;

use crate::{
    AppState, error::HttpError, middleware::session::SessionIdentity, pages, share::Links,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::Response,
    routing::{get, post},
};
use yhotos_core::{ShortId, Slug, access::ShareTarget};
use yhotos_db::{
    albums,
    photos::{self, PagePhoto},
};

#[derive(Clone, Copy, serde::Serialize)]
pub(crate) struct Caps {
    pub download: bool,
    pub like: bool,
    pub flag: bool,
}
pub(crate) fn caps(photo: &PagePhoto, identity: &SessionIdentity, links: &Links) -> Caps {
    let account = photo.account_view && matches!(identity, SessionIdentity::User { .. });
    let covering = |action: fn(&crate::share::ScopedLink) -> bool| {
        links.0.iter().any(|link| {
            link.caps.view
                && match link.target {
                    ShareTarget::Photo(id) => photo.photo_link_view && id == photo.id,
                    ShareTarget::Album(id) => photo.shared_albums.contains(&id),
                }
                && action(link)
        })
    };
    Caps {
        download: (links.0.is_empty() && matches!(identity, SessionIdentity::Anonymous))
            || account
            || covering(|l| l.caps.download),
        like: account || covering(|l| l.caps.like),
        flag: account || covering(|l| l.caps.flag),
    }
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/albums/{id}/photos", get(album))
        .route("/api/v1/photos/{id}", get(photo))
        .route(
            "/api/v1/photos/{id}/like",
            post(interactions::like).delete(interactions::unlike),
        )
        .route("/api/v1/photos/{id}/flag", post(interactions::flag))
}
pub(crate) fn bindings(
    identity: &SessionIdentity,
    links: &Links,
) -> yhotos_db::access_sql::Bindings {
    yhotos_db::access_sql::visible_photos(&identity.principal())
        .with_links(links.0.iter().filter(|l| l.caps.view).map(|l| l.target))
}
async fn album(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Path(id): Path<String>,
    Query(options): Query<pages::Options>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let id = id.parse::<uuid::Uuid>().map_err(|_| HttpError::NotFound)?;
    let number = options.number()?;
    let bindings = bindings(&identity, &links);
    let mut conn = state.pool.acquire().await?;
    let row = yhotos_db::visible_page_query!(
        r#"SELECT path AS "path!" FROM visible_albums WHERE id=$6"#,
        bindings,
        id
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| HttpError::Internal(e.into()))?
    .ok_or(HttpError::NotFound)?;
    let path = row
        .path
        .split('/')
        .map(|s| Slug::new(s).map_err(|_| HttpError::NotFound))
        .collect::<Result<Vec<_>, _>>()?;
    let page = albums::page_for(&mut conn, &bindings, &path, number, options.sort())
        .await?
        .ok_or(HttpError::NotFound)?;
    Ok(Json(pages::island(
        &page,
        number,
        options.sort(),
        &identity,
        &links,
    )?))
}
async fn photo(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Path(id): Path<String>,
    Query(options): Query<pages::Options>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let id = id.parse::<ShortId>().map_err(|_| HttpError::NotFound)?;
    let mut conn = state.pool.acquire().await?;
    let row = photos::page_for(&mut conn, &bindings(&identity, &links), &id, options.sort())
        .await?
        .ok_or(HttpError::NotFound)?;
    let mut value = pages::island_photo(&row.photo, caps(&row.photo, &identity, &links))?;
    let object = value.as_object_mut().ok_or(HttpError::BadRequest)?;
    object.insert(
        "state".into(),
        serde_json::to_value(interactions::state(&mut conn, &row.photo, &identity, &links).await?)?,
    );
    object.insert("previous".into(), serde_json::to_value(row.previous)?);
    object.insert("next".into(), serde_json::to_value(row.next)?);
    Ok(Json(value))
}
/// Normalize even extractor, middleware and fallback errors without revealing internals.
pub(crate) async fn envelope(request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/v1/");
    let private = request.headers().contains_key(header::COOKIE);
    let mut response = next.run(request).await;
    if !api {
        return response;
    }
    let status = response.status();
    if (status.is_client_error() || status.is_server_error())
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_none_or(|v| v != "application/json")
    {
        let code = match status {
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {
                if status == StatusCode::FORBIDDEN {
                    "request_rejected"
                } else {
                    "not_found"
                }
            }
            StatusCode::TOO_MANY_REQUESTS => "rate_limited",
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "bad_request",
            StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
            _ => "unavailable",
        };
        *response.body_mut() = axum::body::Body::from(format!("{{\"error\":\"{code}\"}}"));
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
    }
    response
        .headers_mut()
        .insert(header::VARY, header::HeaderValue::from_static("Cookie"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(if private {
            "private, no-store"
        } else if status.is_success() {
            "public, max-age=60"
        } else {
            "no-store"
        }),
    );
    response
}

#[cfg(test)]
mod tests;
