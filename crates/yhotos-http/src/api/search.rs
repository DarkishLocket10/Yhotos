//! Public JSON contains search metadata only, with no biometric fields.
use crate::{
    AppState, error::HttpError, middleware::session::SessionIdentity, search, share::Links,
};
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
pub(super) async fn search(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Query(options): Query<search::Options>,
) -> Result<Response, HttpError> {
    let results = search::execute(&state, &identity, &links, &options).await?;
    let data = serde_json::json!({"total":results.total,"page":results.page,"photos":results.rows.iter().map(|r|serde_json::json!({"id":r.candidate.photo.short_id,"title":r.candidate.photo.title,"text":r.candidate.text,"score":r.score})).collect::<Vec<_>>()});
    // Escape HTML delimiters even in JSON, so model text remains inert if embedded.
    let body = serde_json::to_string(&data)
        .map_err(|e| HttpError::Internal(e.into()))?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        body,
    )
        .into_response())
}
#[derive(serde::Deserialize)]
pub(super) struct Prefix {
    q: String,
}
pub(super) async fn suggest(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Query(prefix): Query<Prefix>,
) -> Result<Response, HttpError> {
    if prefix.q.chars().count() > 80 || prefix.q.chars().any(char::is_control) {
        return Err(HttpError::BadRequest);
    }
    let p = yhotos_db::access_sql::visible_photos(&identity.principal())
        .with_links(links.0.iter().filter(|l| l.caps.view).map(|l| l.target));
    let rows = yhotos_db::search::suggest(&state.pool, &p, &prefix.q)
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(rows)).into_response())
}
