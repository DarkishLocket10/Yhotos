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
        .route("/admin/people/{id}/merge", post(merge))
}
#[derive(serde::Deserialize)]
struct PersonQuery {
    q: String,
}
async fn search(
    State(state): State<AppState>,
    current: Current,
    Query(query): Query<PersonQuery>,
    Extension(nonce): Extension<Nonce>,
) -> Result<Response, HttpError> {
    guard(&current)?;
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
        faces::photos(&mut conn, id).await?
    } else {
        faces::named_photos(&mut conn, name).await?
    };
    render(&state, &nonce.to_string(), &query.q, photos)
}
#[derive(Template)]
#[template(path = "default/people.html")]
struct PeopleTemplate<'a> {
    people: Vec<faces::Person>,
    csrf: &'a str,

    stylesheet: &'static str,
}
async fn index(
    State(state): State<AppState>,
    current: Current,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    guard(&current)?;
    let mut c = auth::required(&state)?
        .db(Audience(true))?
        .pool()
        .acquire()
        .await?;
    let csrf = CsrfToken::for_page(&headers)?;
    let body = PeopleTemplate {
        people: faces::people(&mut c).await?,
        csrf: csrf.form_value(),
        stylesheet: static_assets::stylesheet_url()?,
    }
    .render()
    .map_err(|e| HttpError::Internal(e.into()))?;
    let mut response = ([(header::CACHE_CONTROL, "private, no-store")], Html(body)).into_response();
    auth::set_cookie(&mut response, csrf.cookie(true)?);
    Ok(response)
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
    Extension(nonce): Extension<Nonce>,
) -> Result<Response, HttpError> {
    guard(&current)?;
    let mut conn = auth::required(&state)?
        .db(Audience(true))?
        .pool()
        .acquire()
        .await?;
    render(
        &state,
        &nonce.to_string(),
        &format!("person:{id}"),
        faces::photos(&mut conn, id).await?,
    )
}

fn render(
    state: &AppState,
    nonce: &str,
    query: &str,
    photos: Vec<yhotos_db::photos::PagePhoto>,
) -> Result<Response, HttpError> {
    let results = super::Results {
        total: photos.len(),
        page: 1,
        parsed: super::query::Query::default(),
        rows: photos
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
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn person_results_are_a_private_escaped_photo_grid() {
        use http_body_util::BodyExt;
        let state = crate::tests::state(false, 600);
        let response = render(
            &state,
            "nonce",
            "person:<script>",
            vec![crate::search::tests::candidate(1).photo],
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
