//! Routes compiled only into the isolated administrative listener.
pub mod cache;
pub mod upload;
mod views;
use crate::{
    AppState,
    auth::{self, Audience},
    error::HttpError,
    middleware::{csrf::CsrfToken, security_headers::Nonce, session::Current},
};
use axum::{
    Extension, Json, Router,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;
use yhotos_db::{library, library_pages};

/// Authoritative library router table, also used to generate listener-isolation tests.
pub const PAGE_ROUTES: &[&str] = &[
    "/admin",
    "/admin/",
    "/admin/albums",
    "/admin/albums/{id}",
    "/admin/photos/{short_id}",
    "/admin/trash",
    "/admin/flags",
    "/admin/likes",
    "/admin/users",
    "/admin/groups",
    "/admin/links",
    "/admin/jobs",
    "/admin/audit",
];
/// All library mutations share CSRF/same-origin middleware and the MFA admin guard.
pub const API_ROUTES: &[&str] = &["/admin/api/actions", "/admin/api/users/temporary-password"];
/// Builds only library pages and transactional JSON actions for the admin listener.
fn route_table() -> Vec<(&'static str, axum::routing::MethodRouter<AppState>)> {
    let mut entries: Vec<_> = PAGE_ROUTES.iter().map(|path| (*path, get(page))).collect();
    entries.extend([
        ("/admin/api/actions", post(mutate)),
        (
            "/admin/api/users/temporary-password",
            post(temporary_password),
        ),
        ("/admin/api/photos/{short_id}/original", get(original)),
        ("/admin/api/photos/{short_id}", get(photo_json)),
    ]);
    entries
}
/// Builds library pages and transactional actions on the administrative listener only.
pub fn routes() -> Router<AppState> {
    route_table()
        .into_iter()
        .fold(Router::new(), |router, (path, methods)| {
            router.route(path, methods)
        })
}
/// Enumerates the same route tables used to construct every administrative handler.
#[must_use]
pub fn route_paths() -> Vec<String> {
    let mut paths: Vec<_> = route_table()
        .into_iter()
        .map(|(path, _)| path.to_owned())
        .collect();
    paths.extend(
        upload::route_table()
            .into_iter()
            .map(|(path, _)| path.to_owned()),
    );
    paths.extend(crate::share::admin_paths().into_iter().map(str::to_owned));
    paths.extend(
        auth::route_table(Audience(true))
            .into_iter()
            .map(|(path, _)| path),
    );
    paths.extend(["/admin/healthz".to_owned(), "/admin/setup-mfa".to_owned()]);
    paths
}
pub(crate) fn pool(state: &AppState) -> Result<&sqlx::PgPool, HttpError> {
    Ok(auth::required(state)?.db(Audience(true))?.pool())
}
pub(crate) fn guard(current: &Current) -> Result<(), HttpError> {
    if current.0.is_admin
        && current.0.session.admin
        && current.0.session.mfa_at.is_some()
        && current.0.has_mfa
        && !current.0.must_change_password
        && !current.0.disabled
    {
        Ok(())
    } else {
        Err(HttpError::NotFound)
    }
}
async fn mutate(
    State(state): State<AppState>,
    current: Current,
    Json(command): Json<library::Command>,
) -> Result<Json<Value>, HttpError> {
    guard(&current)?;
    let change = library::execute(pool(&state)?, current.0.session.user_id, &command)
        .await
        .map_err(input_error)?;
    cache::changed(&state);
    Ok(Json(change.value))
}
fn input_error(e: yhotos_db::DbError) -> HttpError {
    match e {
        yhotos_db::DbError::InvalidInput => HttpError::BadRequest,
        yhotos_db::DbError::Sqlx(sqlx::Error::RowNotFound) => HttpError::NotFound,
        yhotos_db::DbError::Sqlx(sqlx::Error::Database(ref d))
            if d.is_unique_violation()
                || d.is_foreign_key_violation()
                || d.is_check_violation() =>
        {
            HttpError::BadRequest
        }
        _ => e.into(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Temporary {
    id: Option<Uuid>,
    username: Option<String>,
}
async fn temporary_password(
    State(state): State<AppState>,
    current: Current,
    Json(input): Json<Temporary>,
) -> Result<Json<Value>, HttpError> {
    guard(&current)?;
    let auth = auth::required(&state)?;
    let permit = auth
        .password_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| HttpError::BadRequest)?;
    let hasher = auth.passwords;
    let (password, hash) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let password = yhotos_auth::generate_temp_password();
        let hash = hasher.hash_password(&password)?;
        Ok::<_, yhotos_auth::AuthError>((password, secrecy::SecretString::from(hash.as_str())))
    })
    .await
    .map_err(|e| HttpError::Internal(e.into()))?
    .map_err(|_| HttpError::BadRequest)?;
    let mut tx = pool(&state)?.begin().await?;
    let (id, before) = if let Some(id) = input.id {
        if id == current.0.session.user_id.0 {
            return Err(HttpError::BadRequest);
        }
        yhotos_db::users::lock_auth(&mut tx, yhotos_core::UserId(id)).await?;
        let before = library::user_state(&mut tx, id).await?;
        let n = reset_password(&mut tx, id, &hash).await?;
        if n == 0 {
            return Err(HttpError::BadRequest);
        }
        (id, before)
    } else {
        let user = yhotos_db::users::create(
            &mut tx,
            input.username.as_deref().ok_or(HttpError::BadRequest)?,
            &hash,
            true,
        )
        .await
        .map_err(input_error)?;
        (user.id.0, Value::Null)
    };
    let after = library::user_state(&mut tx, id).await?;
    yhotos_db::audit::write_target(
        &mut tx,
        Some(current.0.session.user_id),
        "user_temporary_password",
        "user",
        Some(id),
        &json!({"before":before,"after":after}),
    )
    .await?;
    library::outbox(&mut tx, json!({}), library::aggregate_tags()).await?;
    tx.commit().await?;
    cache::changed(&state);
    Ok(Json(
        json!({"id":id,"temporary_password":password.expose_secret()}),
    ))
}
// SQL belongs to yhotos-db, including password-reset state and session revocation.
async fn reset_password(
    c: &mut yhotos_db::PgConnection,
    id: Uuid,
    hash: &secrecy::SecretString,
) -> Result<u64, HttpError> {
    Ok(library::reset_password(c, id, hash).await?)
}
#[derive(Default, Deserialize)]
struct Filter {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: u32,
}
async fn page(
    State(state): State<AppState>,
    current: Current,
    Extension(nonce): Extension<Nonce>,
    OriginalUri(uri): OriginalUri,
    params: Option<Path<String>>,
    Query(filter): Query<Filter>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    guard(&current)?;
    if filter.q.len() > 128 {
        return Err(HttpError::BadRequest);
    }
    let path = uri.path().trim_end_matches('/');
    let name = if path == "/admin" {
        "dashboard"
    } else if path.starts_with("/admin/albums/") {
        "album"
    } else if path.starts_with("/admin/photos/") {
        "photo"
    } else {
        path.strip_prefix("/admin/").ok_or(HttpError::NotFound)?
    };
    let mut c = pool(&state)?.acquire().await?;
    let id = if name == "photo" {
        let short = params
            .ok_or(HttpError::NotFound)?
            .0
            .parse::<yhotos_core::ShortId>()
            .map_err(|_| HttpError::NotFound)?;
        Some(
            yhotos_db::photos::get_by_short_id(&mut c, &short)
                .await?
                .ok_or(HttpError::NotFound)?
                .id
                .0,
        )
    } else if name == "album" {
        Some(
            params
                .ok_or(HttpError::NotFound)?
                .0
                .parse()
                .map_err(|_| HttpError::NotFound)?,
        )
    } else {
        None
    };
    let data = library_pages::page(&mut c, name, id, &filter.q, i64::from(filter.page) * 100)
        .await
        .map_err(input_error)?;
    let choices = library_pages::choices(&mut c).await?;
    drop(c);
    let csrf = CsrfToken::for_page(&headers)?;
    let html = views::render(
        name,
        &data,
        &choices,
        &csrf,
        &nonce.to_string(),
        &filter.q,
        filter.page,
        state.config.upload.chunk_bytes,
    )?;
    let mut r = ([(header::CACHE_CONTROL, "private, no-store")], Html(html)).into_response();
    auth::set_cookie(&mut r, csrf.cookie(true)?);
    Ok(r)
}

async fn original(
    State(state): State<AppState>,
    current: Current,
    Path(short): Path<String>,
) -> Result<Response, HttpError> {
    guard(&current)?;
    let short = short
        .parse::<yhotos_core::ShortId>()
        .map_err(|_| HttpError::NotFound)?;
    let photo = yhotos_db::photos::get_by_short_id(&mut *pool(&state)?.acquire().await?, &short)
        .await?
        .ok_or(HttpError::NotFound)?;
    let hash = photo
        .original_hash
        .parse()
        .map_err(|_| HttpError::NotFound)?;
    let file = state
        .store
        .open_read(yhotos_storage::Area::Originals, &hash, None)
        .await
        .map_err(|_| HttpError::NotFound)?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
        ],
        axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

/// Renders the administrative album model with compile-time escaping and controls.
/// Callers must authenticate the viewer before exposing this unfiltered read model.
/// # Errors
/// Returns embedded asset or template failures without exposing internal data.
pub fn render_album(
    data: &Value,
    choices: &Value,
    csrf: &CsrfToken,
    nonce: &str,
) -> Result<String, HttpError> {
    views::render("album", data, choices, csrf, nonce, "", 0, 8 * 1024 * 1024)
}

async fn photo_json(
    State(state): State<AppState>,
    current: Current,
    Path(short): Path<String>,
) -> Result<Json<Value>, HttpError> {
    guard(&current)?;
    let short = short
        .parse::<yhotos_core::ShortId>()
        .map_err(|_| HttpError::NotFound)?;
    let mut c = pool(&state)?.acquire().await?;
    let photo = yhotos_db::photos::get_by_short_id(&mut c, &short)
        .await?
        .ok_or(HttpError::NotFound)?;
    Ok(Json(
        library_pages::page(&mut c, "photo", Some(photo.id.0), "", 0).await?,
    ))
}
