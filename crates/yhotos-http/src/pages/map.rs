//! Browser-only tiles and visible photo clusters. No outbound HTTP client exists.
use super::{Options as PageOptions, bindings, cache_response, cached, context};
use crate::{
    AppState,
    error::HttpError,
    middleware::{security_headers::Nonce, session::SessionIdentity},
    share::Links,
    static_assets,
};
use askama::Template;
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::HeaderMap,
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
pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let mut conn = state.pool.acquire().await?;
    let places = map::places_for(&mut conn, &bindings(&identity, &links))
        .await?
        .into_iter()
        .map(|place| {
            let href = format!("/places/{}", place.id);
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
    cached(html, &headers, &identity, "map,visibility")
}
#[derive(serde::Deserialize)]
pub(crate) struct Options {
    bbox: String,
    zoom: String,
}
pub(crate) async fn tiles(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
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
    let clusters = map::clusters_for(&mut conn, &bindings(&identity, &links), bounds, zoom).await?;
    let features=clusters.into_iter().map(|c|serde_json::json!({"type":"Feature","geometry":{"type":"Point","coordinates":[c.lon,c.lat]},"properties":{"count":c.count,"short_id":c.short_id,"thumb_hash":c.thumb_hash,"terminal":c.terminal}})).collect::<Vec<_>>();
    let response =
        Json(serde_json::json!({"type":"FeatureCollection","features":features})).into_response();
    cache_response(
        response,
        &headers,
        &identity,
        "map,visibility",
        "public, max-age=300",
    )
}

/// Validated at the HTTP boundary before SQL, as with the cluster bbox.
#[derive(serde::Deserialize)]
pub(crate) struct PointOptions {
    lon: f64,
    lat: f64,
    #[serde(flatten)]
    page: PageOptions,
}
pub(crate) async fn point_photos(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Query(options): Query<PointOptions>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    if !options.lon.is_finite()
        || !options.lat.is_finite()
        || !(-180.0..=180.0).contains(&options.lon)
        || !(-85.051_129..=85.051_129).contains(&options.lat)
    {
        return Err(HttpError::BadRequest);
    }
    let number = options.page.number()?;
    let mut conn = state.pool.acquire().await?;
    let page = map::point_photos_for(
        &mut conn,
        &bindings(&identity, &links),
        options.lon,
        options.lat,
        number,
    )
    .await?
    .ok_or(HttpError::NotFound)?;
    cache_response(
        Json(page).into_response(),
        &headers,
        &identity,
        "map,visibility",
        "public, max-age=300",
    )
}
pub(crate) async fn place(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Path(id): Path<String>,
    Query(options): Query<PageOptions>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let place = id.parse::<uuid::Uuid>().map_err(|_| HttpError::NotFound)?;
    let number = options.number()?;
    let mut conn = state.pool.acquire().await?;
    let page = yhotos_db::places::page_for(&mut conn, &bindings(&identity, &links), place, number)
        .await?
        .ok_or(HttpError::NotFound)?;
    let nonce = nonce.to_string();
    let html = super::view::render_place(&page, place, number, &context(&nonce, &state)?)?;
    cached(
        html,
        &headers,
        &identity,
        &format!("places,place:{place},visibility"),
    )
}
