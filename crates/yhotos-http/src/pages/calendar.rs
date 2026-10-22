//! Calendar routes validate dates before acquiring a connection.
use super::{Options, cached, context, view::render_calendar};
use crate::{
    AppState,
    error::HttpError,
    middleware::{security_headers::Nonce, session::SessionIdentity},
};
use axum::{
    Extension,
    extract::{OriginalUri, Query, State},
    http::HeaderMap,
    response::Response,
};
use yhotos_db::calendar::{self, Selection};
pub(crate) fn selection(path: &str) -> Option<Selection> {
    let tail = path.strip_prefix("/calendar")?;
    if tail.is_empty() {
        return Selection::new(None, None, None);
    }
    let parts = tail.strip_prefix('/')?.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [y] => Selection::new(Some(y.parse().ok()?), None, None),
        [y, m] => Selection::new(Some(y.parse().ok()?), Some(m.parse().ok()?), None),
        [y, m, d] => Selection::new(
            Some(y.parse().ok()?),
            Some(m.parse().ok()?),
            Some(d.parse().ok()?),
        ),
        _ => None,
    }
}
pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    OriginalUri(uri): OriginalUri,
    Query(options): Query<Options>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let selection = selection(uri.path()).ok_or(HttpError::NotFound)?;
    let number = options.number()?;
    let mut conn = state.pool.acquire().await?;
    let page = calendar::page(&mut conn, &identity.principal(), selection, number)
        .await?
        .ok_or(HttpError::NotFound)?;
    let nonce = nonce.to_string();
    let html = render_calendar(&page, selection, number, &context(&nonce, &state)?)?;
    cached(html, &headers, &identity, "calendar,visibility")
}
