//! Private listener only; every operation checks the authenticated admin session.
use crate::{
    AppState,
    auth::{self, Audience},
    error::HttpError,
    middleware::{csrf::CsrfToken, security_headers::Nonce, session::Current},
    static_assets,
};
use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use uuid::Uuid;
use yhotos_db::{audit, faces};
mod previews;

fn guard(current: &Current) -> Result<(), HttpError> {
    if current.0.is_admin
        && current.0.session.admin
        && current.0.session.mfa_at.is_some()
        && current.0.has_mfa
    {
        Ok(())
    } else {
        Err(HttpError::NotFound)
    }
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/people", get(index))
        .route("/admin/people/search", get(search))
        .route("/admin/people/faces/{face}", get(previews::serve))
        .route("/admin/people/rebuild", post(rebuild))
        .route("/admin/people/{id}", post(update).get(person))
        .route("/admin/people/{id}/merge", get(merge_targets).post(merge))
}
#[derive(serde::Deserialize)]
struct PersonQuery {
    q: String,
    page: Option<u32>,
}
async fn search(
    State(state): State<AppState>,
    current: Current,
    Query(query): Query<PersonQuery>,
    Extension(nonce): Extension<Nonce>,
) -> Result<Response, HttpError> {
    guard(&current)?;
    let page = page_number(query.page)?;
    let name = query
        .q
        .strip_prefix("person:")
        .ok_or(HttpError::BadRequest)?
        .trim_matches('"');
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(HttpError::BadRequest);
    }
    let mut conn = auth::required(&state)?
        .db(Audience(true))?
        .pool()
        .acquire()
        .await?;
    let photos = if let Ok(id) = name.parse::<Uuid>() {
        faces::photos(&mut conn, id, page).await?
    } else {
        faces::named_photos(&mut conn, name, page).await?
    };
    render(&state, &nonce.to_string(), &query.q, photos, page)
}
#[derive(Template)]
#[template(path = "default/people.html")]
struct PeopleTemplate<'a> {
    people: Vec<faces::Person>,
    csrf: &'a str,
    merge_source: Option<Uuid>,
    previous: Option<String>,
    next: Option<String>,
    stylesheet: &'static str,
}
async fn index(
    State(state): State<AppState>,
    current: Current,
    headers: HeaderMap,
    Query(options): Query<super::Options>,
) -> Result<Response, HttpError> {
    listing(&state, &current, &headers, options.page, None).await
}
async fn merge_targets(
    State(state): State<AppState>,
    current: Current,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(options): Query<super::Options>,
) -> Result<Response, HttpError> {
    listing(&state, &current, &headers, options.page, Some(id)).await
}
async fn listing(
    state: &AppState,
    current: &Current,
    headers: &HeaderMap,
    page: Option<u32>,
    merge_source: Option<Uuid>,
) -> Result<Response, HttpError> {
    guard(current)?;
    let page = page_number(page)?;
    let mut c = auth::required(state)?
        .db(Audience(true))?
        .pool()
        .acquire()
        .await?;
    if let Some(source) = merge_source
        && faces::photos(&mut c, source, 1).await?.total == 0
    {
        return Err(HttpError::NotFound);
    }
    let people = faces::people(&mut c, page, merge_source).await?;
    let csrf = CsrfToken::for_page(headers)?;
    let body = render_listing(people, page, merge_source, csrf.form_value())?;
    let mut response = ([(header::CACHE_CONTROL, "private, no-store")], Html(body)).into_response();
    auth::set_cookie(&mut response, csrf.cookie(true)?);
    Ok(response)
}
fn render_listing(
    results: faces::PeoplePage,
    page: u32,
    merge_source: Option<Uuid>,
    csrf: &str,
) -> Result<String, HttpError> {
    if page == 0 || (page > 1 && results.people.is_empty()) {
        return Err(HttpError::NotFound);
    }
    let action = merge_source.map_or_else(
        || "/admin/people".to_owned(),
        |id| format!("/admin/people/{id}/merge"),
    );
    let next = (u64::from(page) * 60 < u64::try_from(results.total).unwrap_or(u64::MAX))
        .then(|| format!("{action}?page={}", page.saturating_add(1)));
    PeopleTemplate {
        people: results.people,
        merge_source,
        previous: (page > 1).then(|| format!("{action}?page={}", page - 1)),
        next,
        csrf,
        stylesheet: static_assets::stylesheet_url()?,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))
}
#[derive(serde::Deserialize)]
struct Update {
    name: String,
    #[serde(default)]
    hide: bool,
}
async fn update(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
    Form(input): Form<Update>,
) -> Result<Redirect, HttpError> {
    guard(&current)?;
    if input.name.chars().count() > 80 || input.name.chars().any(char::is_control) {
        return Err(HttpError::BadRequest);
    }
    let pool = auth::required(&state)?.db(Audience(true))?.pool();
    let mut tx = pool.begin().await?;
    if !faces::update(&mut tx, id, &input.name, input.hide).await? {
        return Err(HttpError::NotFound);
    }
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "person.update",
        Some(id),
        &serde_json::json!({"hidden":input.hide}),
    )
    .await?;
    tx.commit().await?;
    Ok(Redirect::to("/admin/people"))
}
#[derive(serde::Deserialize)]
struct Merge {
    target: Uuid,
}
async fn merge(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
    Form(input): Form<Merge>,
) -> Result<Redirect, HttpError> {
    guard(&current)?;
    let mut tx = auth::required(&state)?
        .db(Audience(true))?
        .pool()
        .begin()
        .await?;
    faces::merge(&mut tx, id, input.target).await?;
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "person.merge",
        Some(id),
        &serde_json::json!({"target":input.target}),
    )
    .await?;
    tx.commit().await?;
    Ok(Redirect::to("/admin/people"))
}
async fn rebuild(State(state): State<AppState>, current: Current) -> Result<Redirect, HttpError> {
    guard(&current)?;
    let mut tx = auth::required(&state)?
        .db(Audience(true))?
        .pool()
        .begin()
        .await?;
    yhotos_jobs::Queue::enqueue_in(&mut tx, &yhotos_jobs::Job::MlCluster, -20, None)
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    audit::write(
        &mut tx,
        Some(current.0.session.user_id),
        "person.rebuild",
        None,
        &serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Redirect::to("/admin/people"))
}
async fn person(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
    Query(options): Query<super::Options>,
    Extension(nonce): Extension<Nonce>,
) -> Result<Response, HttpError> {
    guard(&current)?;
    let page = page_number(options.page)?;
    let mut conn = auth::required(&state)?
        .db(Audience(true))?
        .pool()
        .acquire()
        .await?;
    render(
        &state,
        &nonce.to_string(),
        &format!("person:{id}"),
        faces::photos(&mut conn, id, page).await?,
        page,
    )
}

fn page_number(page: Option<u32>) -> Result<u32, HttpError> {
    match page.unwrap_or(1) {
        0 => Err(HttpError::NotFound),
        page => Ok(page),
    }
}

fn render(
    state: &AppState,
    nonce: &str,
    query: &str,
    photos: faces::PhotoPage,
    page: u32,
) -> Result<Response, HttpError> {
    if page > 1 && photos.photos.is_empty() {
        return Err(HttpError::NotFound);
    }
    let results = super::Results {
        total: photos.total,
        page,
        parsed: super::query::Query::default(),
        rows: photos
            .photos
            .into_iter()
            .map(|photo| super::Ranked {
                score: 0.0,
                candidate: yhotos_db::search::Candidate {
                    photo,
                    text: String::new(),
                },
            })
            .collect(),
    };
    let html = crate::pages::render_search(
        &results,
        query,
        None,
        &crate::pages::context(nonce, state)?,
        "/admin/people/search",
    )?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Html(html)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
    fn cluster_and_merge_navigation_cover_every_page_and_escape_names() {
        for source in [None, Some(Uuid::from_u128(1))] {
            for page in 1..=4 {
                let id = Uuid::from_u128(205);
                let html = render_listing(
                    faces::PeoplePage {
                        people: vec![faces::Person {
                            id,
                            name: Some("<script>name</script>".into()),
                            count: 1,
                            samples: vec![],
                        }],
                        total: 205,
                    },
                    page,
                    source,
                    "csrf",
                )
                .unwrap();
                let action = source.map_or_else(
                    || "/admin/people".to_owned(),
                    |id| format!("/admin/people/{id}/merge"),
                );
                assert_eq!(html.contains("rel=\"prev\""), page > 1);
                assert_eq!(html.contains("rel=\"next\""), page < 4);
                if page > 1 {
                    assert!(html.contains(&format!("href=\"{action}?page={}\"", page - 1)));
                }
                if page < 4 {
                    assert!(html.contains(&format!("href=\"{action}?page={}\"", page + 1)));
                }
                assert!(!html.contains("<script>name</script>"));
                if let Some(source) = source {
                    assert!(html.contains(&format!("action=\"/admin/people/{source}/merge\"")));
                    assert!(html.contains(&format!("name=\"target\" value=\"{id}\"")));
                    assert!(html.contains("name=\"_csrf\" value=\"csrf\""));
                } else {
                    assert!(html.contains(&format!("href=\"/admin/people/{id}/merge\"")));
                }
            }
            assert!(
                render_listing(
                    faces::PeoplePage {
                        people: vec![],
                        total: 205
                    },
                    5,
                    source,
                    "csrf"
                )
                .is_err()
            );
        }
    }
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn person_results_are_a_private_escaped_photo_grid() {
        use http_body_util::BodyExt;
        let state = crate::tests::state(false, 600);
        let response = render(
            &state,
            "nonce",
            "person:<script>",
            faces::PhotoPage {
                photos: vec![crate::search::tests::candidate(1).photo],
                total: 1,
            },
            1,
        )
        .unwrap();
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("action=\"/admin/people/search\""));
        assert!(html.contains("class=\"photo-grid\""));
        assert!(!html.contains("person:<script>"));
    }
}
