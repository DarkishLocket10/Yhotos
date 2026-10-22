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
        required(CSS.as_ref())?,
        required(JS.as_ref())?,
        required(PASSKEY.as_ref())?,
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
