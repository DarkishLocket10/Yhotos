//! Resolves only embedded, content-addressed assets, never filesystem paths.

use crate::error::HttpError;
use axum::{
    body::{Body, Bytes},
    extract::Path,
    http::header,
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;
use std::{borrow::Cow, sync::LazyLock};

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;
struct Asset {
    url: String,
    bytes: Bytes,
    content_type: &'static str,
}
impl Asset {
    fn new(name: &str, extension: &str, source: &str, content_type: &'static str) -> Option<Self> {
        let data = Assets::get(source)?.data;
        let bytes = match data {
            Cow::Borrowed(bytes) => Bytes::from_static(bytes),
            Cow::Owned(bytes) => Bytes::from(bytes),
        };
        Some(Self {
            url: format!(
                "/static/{name}.{}.{extension}",
                blake3::hash(&bytes).to_hex()
            ),
            bytes,
            content_type,
        })
    }
}
static CSS: LazyLock<Option<Asset>> =
    LazyLock::new(|| Asset::new("app", "css", "css/app.css", "text/css; charset=utf-8"));
static JS: LazyLock<Option<Asset>> = LazyLock::new(|| {
    Asset::new(
        "thumbhash",
        "js",
        "js/thumbhash.js",
        "text/javascript; charset=utf-8",
    )
});

static PASSKEY: LazyLock<Option<Asset>> = LazyLock::new(|| {
    Asset::new(
        "passkey",
        "js",
        "js/passkey.js",
        "text/javascript; charset=utf-8",
    )
});
pub(crate) fn passkey_url() -> Result<&'static str, HttpError> {
    Ok(&required(PASSKEY.as_ref())?.url)
}

static MAP: LazyLock<Option<Asset>> =
    LazyLock::new(|| Asset::new("map", "js", "js/map.js", "text/javascript; charset=utf-8"));
static MAP_WORKER: LazyLock<Option<Asset>> = LazyLock::new(|| {
    Asset::new(
        "map-worker",
        "js",
        "js/map-worker.js",
        "text/javascript; charset=utf-8",
    )
});
static MAP_CSS: LazyLock<Option<Asset>> =
    LazyLock::new(|| Asset::new("map", "css", "js/maplibre.css", "text/css; charset=utf-8"));
pub(crate) fn map_script_url() -> Result<&'static str, HttpError> {
    Ok(&required(MAP.as_ref())?.url)
}
pub(crate) fn map_worker_url() -> Result<&'static str, HttpError> {
    Ok(&required(MAP_WORKER.as_ref())?.url)
}
pub(crate) fn map_css_url() -> Result<&'static str, HttpError> {
    Ok(&required(MAP_CSS.as_ref())?.url)
}
macro_rules! island_asset {
    ($static:ident, $function:ident, $name:literal) => {
        static $static: LazyLock<Option<Asset>> = LazyLock::new(|| {
            Asset::new(
                $name,
                "js",
                concat!("js/", $name, ".js"),
                "text/javascript; charset=utf-8",
            )
        });
        pub(crate) fn $function() -> Result<&'static str, HttpError> {
            Ok(&required($static.as_ref())?.url)
        }
    };
}
island_asset!(LIGHTBOX, lightbox_url, "lightbox");
island_asset!(PREFETCH, prefetch_url, "prefetch");
island_asset!(TRANSITIONS, transitions_url, "transitions");
island_asset!(LIKES, likes_url, "likes");
island_asset!(VIDEO, video_url, "video");
#[cfg(feature = "admin")]
static UPLOAD: LazyLock<Option<Asset>> = LazyLock::new(|| {
    Asset::new(
        "upload",
        "js",
        "js/upload.js",
        "text/javascript; charset=utf-8",
    )
});
#[cfg(feature = "admin")]
pub(crate) fn upload_url() -> Result<&'static str, HttpError> {
    Ok(&required(UPLOAD.as_ref())?.url)
}

fn required(asset: Option<&'static Asset>) -> Result<&'static Asset, HttpError> {
    asset.ok_or_else(|| HttpError::Internal(anyhow::anyhow!("embedded asset missing")))
}
pub(crate) fn stylesheet_url() -> Result<&'static str, HttpError> {
    Ok(&required(CSS.as_ref())?.url)
}
pub(crate) fn script_url() -> Result<&'static str, HttpError> {
    Ok(&required(JS.as_ref())?.url)
}
pub(crate) async fn serve(Path(path): Path<String>) -> Result<Response, HttpError> {
    for asset in [
        #[cfg(feature = "admin")]
        required(UPLOAD.as_ref())?,
        required(CSS.as_ref())?,
        required(JS.as_ref())?,
        required(PASSKEY.as_ref())?,
        required(MAP.as_ref())?,
        required(MAP_WORKER.as_ref())?,
        required(MAP_CSS.as_ref())?,
        required(LIGHTBOX.as_ref())?,
        required(PREFETCH.as_ref())?,
        required(TRANSITIONS.as_ref())?,
        required(LIKES.as_ref())?,
        required(VIDEO.as_ref())?,
    ] {
        if asset.url.strip_prefix("/static/") == Some(path.as_str()) {
            return Ok((
                [
                    (header::CONTENT_TYPE, asset.content_type),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                ],
                Body::from(asset.bytes.clone()),
            )
                .into_response());
        }
    }
    Err(HttpError::NotFound)
}
