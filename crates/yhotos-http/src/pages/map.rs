//! Browser-only tiles and visible photo clusters. No outbound HTTP client exists.
use super::{cached, context};
use crate::{
    AppState,
    error::HttpError,
    middleware::{security_headers::Nonce, session::SessionIdentity},
    static_assets,
};
use askama::Template;
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use yhotos_db::map::{self, Bounds, PlaceCount};

#[derive(Template)]
#[template(path = "default/map.html")]
struct MapTemplate<'a> {
    context: super::PageContext<'a>,
    title: &'static str,
    description: &'static str,
    styles: &'static str,
    places: Vec<(PlaceCount, String)>,
    tiles: &'a str,
    map_script: &'static str,
    map_css: &'static str,
    worker: &'static str,
}
fn place_url(name: &str) -> String {
    use std::fmt::Write;
    let mut href = String::from("/search?place=");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            href.push(char::from(byte));
        } else {
            let _ = write!(href, "%{byte:02X}");
        }
    }
    href
}
pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let mut conn = state.pool.acquire().await?;
    let places = map::places(&mut conn, &identity.principal())
        .await?
        .into_iter()
        .map(|place| {
            let href = place_url(&place.name);
            (place, href)
        })
        .collect();
    let nonce = nonce.to_string();
    let html = MapTemplate {
        context: context(&nonce, &state)?,
        title: "Photo map",
        description: "Browse photos by location",
        styles: "",
        places,
        tiles: &state.config.map_tiles.template,
        map_script: static_assets::map_script_url()?,
        map_css: static_assets::map_css_url()?,
        worker: static_assets::map_worker_url()?,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))?;
    cached(html, &headers, &identity, "")
}
#[derive(serde::Deserialize)]
pub(crate) struct Options {
    bbox: String,
    zoom: String,
}
pub(crate) async fn tiles(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Query(options): Query<Options>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let bounds = Bounds::parse(&options.bbox).ok_or(HttpError::BadRequest)?;
    let zoom = options
        .zoom
        .parse::<u8>()
        .ok()
        .filter(|z| *z <= 20)
        .ok_or(HttpError::BadRequest)?;
    let mut conn = state.pool.acquire().await?;
    let clusters = map::clusters(&mut conn, &identity.principal(), bounds, zoom).await?;
    let features=clusters.into_iter().map(|c|serde_json::json!({"type":"Feature","geometry":{"type":"Point","coordinates":[c.lon,c.lat]},"properties":{"count":c.count,"short_id":c.short_id,"thumb_hash":c.thumb_hash}})).collect::<Vec<_>>();
    let mut response =
        Json(serde_json::json!({"type":"FeatureCollection","features":features})).into_response();
    let private =
        headers.contains_key(header::COOKIE) || !matches!(identity, SessionIdentity::Anonymous);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if private {
            "private, no-store"
        } else {
            "public, max-age=300"
        }),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    #[test]
    fn place_filter_uses_encoded_text_without_internal_ids() {
        assert_eq!(
            super::place_url("São Paulo & coast"),
            "/search?place=S%C3%A3o%20Paulo%20%26%20coast"
        );
    }
}
