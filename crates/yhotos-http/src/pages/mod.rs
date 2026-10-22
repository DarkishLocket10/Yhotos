//! Server-rendered public gallery, with authorization performed inside page SQL.
pub mod grid;
mod view;
pub use view::{PageContext, render_album, render_home, render_photo};

use crate::{
    AppState,
    error::HttpError,
    middleware::{security_headers::Nonce, session::SessionIdentity},
    static_assets,
};
use askama::Template;
use axum::{
    Extension,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{Html, IntoResponse, Response},
};
use yhotos_core::{ShortId, Slug};
use yhotos_db::{
    albums,
    photos::{self, PageSort},
    redirects,
};

#[derive(Default, serde::Deserialize)]
pub(crate) struct Options {
    page: Option<String>,
    sort: Option<String>,
}
impl Options {
    fn number(&self) -> Result<u32, HttpError> {
        self.page
            .as_deref()
            .unwrap_or("1")
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(HttpError::NotFound)
    }
    fn sort(&self) -> PageSort {
        PageSort::parse(self.sort.as_deref().unwrap_or("taken"))
    }
}
fn page_path(path: &str) -> Result<Vec<Slug>, HttpError> {
    if path.len() > 4096 {
        return Err(HttpError::NotFound);
    }
    path.split('/')
        .map(|part| Slug::new(part).map_err(|_| HttpError::NotFound))
        .collect()
}
pub(crate) async fn privacy(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let private = request.headers().contains_key(header::COOKIE)
        || request
            .extensions()
            .get::<SessionIdentity>()
            .is_some_and(|identity| !matches!(identity, SessionIdentity::Anonymous));
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    if private {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
    }
    response
}
fn db_error(error: yhotos_db::DbError) -> HttpError {
    HttpError::Internal(error.into())
}
fn context<'a>(nonce: &'a str, state: &'a AppState) -> Result<PageContext<'a>, HttpError> {
    Ok(PageContext {
        nonce,
        stylesheet: static_assets::stylesheet_url()?,
        script: static_assets::script_url()?,
        public_base_url: state.config.public_base_url.as_str(),
    })
}
fn cached(
    html: String,
    headers: &HeaderMap,
    identity: &SessionIdentity,
    tags: &str,
) -> Result<Response, HttpError> {
    let private =
        headers.contains_key(header::COOKIE) || !matches!(identity, SessionIdentity::Anonymous);
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if private {
            "private, no-store"
        } else {
            "public, max-age=60, stale-while-revalidate=600"
        }),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    if !tags.is_empty() {
        response.headers_mut().insert(
            "cache-tag",
            HeaderValue::from_str(tags).map_err(|e| HttpError::Internal(e.into()))?,
        );
    }
    Ok(response)
}
pub(crate) async fn home(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let rows = albums::home(&mut conn, &identity.principal())
        .await
        .map_err(db_error)?;
    let nonce = nonce.to_string();
    let html = render_home(&rows, &context(&nonce, &state)?)?;
    let tags = rows
        .iter()
        .map(|a| format!("album:{}", a.id))
        .collect::<Vec<_>>()
        .join(",");
    cached(html, &headers, &identity, &tags)
}
pub(crate) async fn album(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    Path(path): Path<String>,
    Query(options): Query<Options>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let number = options.number()?;
    let path = page_path(&path)?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let row = albums::page(
        &mut conn,
        &identity.principal(),
        &path,
        number,
        options.sort(),
    )
    .await
    .map_err(db_error)?
    .ok_or(HttpError::NotFound)?;
    let nonce = nonce.to_string();
    let html = render_album(&row, number, options.sort(), &context(&nonce, &state)?)?;
    cached(
        html,
        &headers,
        &identity,
        &format!("album:{}", row.album.id),
    )
}
pub(crate) async fn photo(
    State(state): State<AppState>,
    Extension(nonce): Extension<Nonce>,
    Extension(identity): Extension<SessionIdentity>,
    Path(id): Path<String>,
    Query(options): Query<Options>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let id = id.parse::<ShortId>().map_err(|_| HttpError::NotFound)?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let row = photos::page(&mut conn, &identity.principal(), &id, options.sort())
        .await
        .map_err(db_error)?
        .ok_or(HttpError::NotFound)?;
    let nonce = nonce.to_string();
    let html = render_photo(&row, options.sort(), &context(&nonce, &state)?)?;
    cached(
        html,
        &headers,
        &identity,
        &format!("album:{},photo:{}", row.photo.album_id, row.photo.id),
    )
}
fn positive_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}
fn legacy_patterns(uri: &Uri) -> Option<Vec<String>> {
    if let Some(rest) = uri.path().strip_prefix("/legacy/") {
        if rest.len() > 4096
            || rest.is_empty()
            || rest.contains(['\\', '\0'])
            || uri.query().is_some()
        {
            return None;
        }
        return Some(vec![uri.path().to_owned(), format!("/{rest}")]);
    }
    let query = uri.query()?;
    match uri.path() {
        "/index.php" => {
            let category = query.strip_prefix("/category/")?;
            let (id, slug) = category
                .split_once('-')
                .map_or((category, None), |(id, slug)| (id, Some(slug)));
            if !positive_id(id)
                || slug.is_some_and(|slug| {
                    slug.is_empty()
                        || slug.len() > 1024
                        || slug.contains(['/', '\\', '?', '#', '&'])
                        || slug.chars().any(char::is_control)
                })
            {
                return None;
            }
            let mut patterns = vec![
                format!("/index.php?{query}"),
                format!("/category/{category}"),
                format!("/category/{id}"),
            ];
            if let Some(slug) = slug {
                patterns.push(format!("/{slug}"));
            }
            Some(patterns)
        }
        "/picture.php" => {
            let rest = query.strip_prefix('/')?;
            let (id, category) = rest.split_once("/category/")?;
            if !positive_id(id) || !positive_id(category) {
                return None;
            }
            Some(vec![
                format!("/picture.php?{query}"),
                format!("/picture/{id}/category/{category}"),
                format!("/picture/{id}"),
            ])
        }
        _ => None,
    }
}
pub(crate) async fn legacy(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, HttpError> {
    let patterns = legacy_patterns(&uri).ok_or(HttpError::NotFound)?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let destination = redirects::destination(&mut conn, &identity.principal(), &patterns)
        .await
        .map_err(db_error)?
        .ok_or(HttpError::NotFound)?;
    let location = HeaderValue::from_str(&destination).map_err(|_| HttpError::NotFound)?;
    Ok((
        StatusCode::MOVED_PERMANENTLY,
        [
            (header::LOCATION, location),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            ),
        ],
    )
        .into_response())
}
#[derive(Template)]
#[template(path = "default/404.html")]
struct NotFoundTemplate {
    stylesheet: &'static str,
}
/// A deterministic body for both restricted and unknown resources, without a nonce.
pub(crate) fn not_found() -> Response {
    let html = NotFoundTemplate {
        stylesheet: static_assets::stylesheet_url().unwrap_or(""),
    }
    .render()
    .unwrap_or_else(|_| "Not found".into());
    (
        StatusCode::NOT_FOUND,
        [(header::VARY, "Cookie")],
        Html(html),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
