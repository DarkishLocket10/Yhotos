use super::cache::Snapshot;
use crate::{AppState, auth, error::HttpError, middleware::csrf::CsrfToken, pages};
use askama::Template;
use axum::{
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use yhotos_db::{photos::PageSort, share_links::LandingPage};

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
pub(super) fn page(
    s: &Snapshot,
    headers: &HeaderMap,
    state: &AppState,
    nonce: &str,
    number: u32,
    sort: PageSort,
) -> Result<Response, HttpError> {
    let csrf = CsrfToken::for_page(headers)?;
    let context = pages::context(nonce, state)?;
    let html = match &s.landing.page {
        LandingPage::Album(page) => pages::render_album(page, number, sort, &context)?,
        LandingPage::Photo(page) => {
            let mut page = page.clone();
            page.chain.clear();
            pages::render_scoped_photo(
                &page,
                sort,
                &context,
                pages::PhotoControls {
                    csrf: csrf.form_value(),
                    download: s.landing.link.allow_download,
                    like: s.landing.link.allow_like,
                    flag: s.landing.link.allow_flag,
                },
            )?
        }
    };
    let mut response = Html(html).into_response();
    auth::set_cookie(&mut response, csrf.cookie(false)?);
    Ok(response)
}
