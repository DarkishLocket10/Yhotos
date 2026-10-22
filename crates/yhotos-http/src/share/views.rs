use super::{Photo, cache::Snapshot, scoped};
use crate::{auth, error::HttpError, middleware::csrf::CsrfToken};
use askama::Template;
use axum::{
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};

#[derive(Template)]
#[template(path = "default/share/unlock.html")]
struct UnlockTemplate<'a> {
    csrf: &'a str,
    failed: bool,
}
pub(super) fn unlock(headers: &HeaderMap, failed: bool) -> Result<Response, HttpError> {
    let csrf = CsrfToken::for_page(headers)?;
    let html = UnlockTemplate {
        csrf: csrf.form_value(),
        failed,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))?;
    let mut response = (
        if failed {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        },
        Html(html),
    )
        .into_response();
    auth::set_cookie(&mut response, csrf.cookie(false)?);
    Ok(response)
}
#[derive(Template)]
#[template(path = "default/share/page.html")]
struct PageTemplate<'a> {
    stylesheet: &'static str,
    title: &'a str,
    photos: Vec<&'a Photo>,
    csrf: &'a str,
    download: bool,
    like: bool,
    flag: bool,
}
pub(super) fn page(s: &Snapshot, headers: &HeaderMap) -> Result<Response, HttpError> {
    let csrf = CsrfToken::for_page(headers)?;
    let principal = scoped(&s.landing.link);
    let html = PageTemplate {
        stylesheet: crate::static_assets::stylesheet_url()?,
        title: &s.landing.title,
        photos: s
            .photos
            .iter()
            .filter(|p| p.may_view(&principal.principal()))
            .collect(),
        csrf: csrf.form_value(),
        download: principal.caps.download,
        like: principal.caps.like,
        flag: principal.caps.flag,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))?;
    let mut response = Html(html).into_response();
    auth::set_cookie(&mut response, csrf.cookie(false)?);
    Ok(response)
}
