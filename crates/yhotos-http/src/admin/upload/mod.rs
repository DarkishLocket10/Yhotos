//! Admin-only tus 1.0 creation, resumable streaming, termination and completion.
pub mod headers;
pub mod stream;
use crate::{
    AppState,
    error::HttpError,
    middleware::{csrf::CsrfToken, security_headers::Nonce, session::Current},
};
use askama::Template;
use axum::{
    Extension, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{Next, from_fn},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use std::time::Duration;
use uuid::Uuid;
use yhotos_core::{AlbumId, ShortId};
use yhotos_db::{photos, uploads};
use yhotos_jobs::{Job, Queue};
use yhotos_pipeline::Ingest;
use yhotos_storage::TempId;

/// Mounts only on the authenticated admin listener.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/upload", get(page).post(create).options(options))
        .route(
            "/admin/upload/{id}",
            axum::routing::head(head).patch(patch).delete(terminate),
        )
        .route("/admin/api/photos/{id}/status", get(status))
        .layer(from_fn(protocol))
}
pub(crate) fn with_method_override(router: Router) -> Router {
    Router::new()
        .fallback_service(router)
        .layer(from_fn(override_method))
}
async fn override_method(mut request: Request, next: Next) -> Response {
    if (request.uri().path() == "/admin/upload"
        || request.uri().path().starts_with("/admin/upload/"))
        && request
            .headers()
            .get_all("x-http-method-override")
            .iter()
            .count()
            == 1
        && let Some(method) = request
            .headers()
            .get("x-http-method-override")
            .and_then(|v| Method::from_bytes(v.as_bytes()).ok())
    {
        *request.method_mut() = method;
    }
    next.run(request).await
}
async fn protocol(request: Request, next: Next) -> Response {
    let invalid_override = request
        .headers()
        .get_all("x-http-method-override")
        .iter()
        .count()
        > 1
        || request
            .headers()
            .get("x-http-method-override")
            .is_some_and(|v| Method::from_bytes(v.as_bytes()).is_err());
    let tus = request.method() != Method::GET && request.method() != Method::OPTIONS;
    let valid = request.headers().get_all("tus-resumable").iter().count() == 1
        && request
            .headers()
            .get("tus-resumable")
            .is_some_and(|v| v == "1.0.0");
    let mut response = if invalid_override {
        HttpError::BadRequest.into_response()
    } else if tus && !valid {
        HttpError::Upload(StatusCode::PRECONDITION_FAILED).into_response()
    } else {
        next.run(request).await
    };
    if response.status() == StatusCode::PRECONDITION_FAILED {
        response
            .headers_mut()
            .insert("tus-version", HeaderValue::from_static("1.0.0"));
    }
    response
        .headers_mut()
        .insert("tus-resumable", HeaderValue::from_static("1.0.0"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn field<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, HttpError> {
    if headers.get_all(name).iter().count() != 1 {
        return Err(HttpError::BadRequest);
    }
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .ok_or(HttpError::BadRequest)
}
fn number(headers: &HeaderMap, name: &str) -> Result<i64, HttpError> {
    headers::number(field(headers, name)?).ok_or(HttpError::BadRequest)
}
fn internal<E>(_: E) -> HttpError {
    HttpError::Internal(anyhow::anyhow!("upload operation failed"))
}
fn add(
    response: &mut Response,
    key: &'static str,
    value: impl std::fmt::Display,
) -> Result<(), HttpError> {
    response.headers_mut().insert(
        key,
        HeaderValue::from_str(&value.to_string()).map_err(internal)?,
    );
    Ok(())
}
fn expires(time: time::OffsetDateTime) -> String {
    // RFC 9110 IMF-fixdate, fixed English names and UTC.
    let day = match time.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let month = match time.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    format!(
        "{day}, {:02} {month} {} {:02}:{:02}:{:02} GMT",
        time.day(),
        time.year(),
        time.hour(),
        time.minute(),
        time.second()
    )
}
async fn options(State(state): State<AppState>) -> Result<Response, HttpError> {
    let mut r = StatusCode::NO_CONTENT.into_response();
    add(&mut r, "tus-version", "1.0.0")?;
    add(&mut r, "tus-extension", "creation,termination,expiration")?;
    add(&mut r, "tus-max-size", state.config.upload.max_bytes)?;
    Ok(r)
}
async fn create(
    State(state): State<AppState>,
    current: Current,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let length = number(&headers, "upload-length")?;
    if length == 0 {
        return Err(HttpError::Upload(StatusCode::UNPROCESSABLE_ENTITY));
    }
    if u64::try_from(length).map_err(|_| HttpError::BadRequest)? > state.config.upload.max_bytes {
        return Err(HttpError::Upload(StatusCode::PAYLOAD_TOO_LARGE));
    }
    let metadata_header = field(&headers, "upload-metadata")?;
    let metadata = headers::metadata(metadata_header).ok_or(HttpError::BadRequest)?;
    let album: AlbumId = metadata.album.parse().map_err(|_| HttpError::BadRequest)?;
    let filename = headers::filename(&metadata.filename);
    let (temp, file) = state.store.create_upload().await.map_err(internal)?;
    drop(file);
    let db = pool(&state)?;
    let operation = async {
        let mut tx = db.begin().await?;
        let row = uploads::create(
            &mut tx,
            current.0.session.user_id,
            album,
            &filename,
            length,
            &metadata.filetype,
            &temp.to_string(),
            metadata_header,
        )
        .await?;
        tx.commit().await?;
        Ok::<_, yhotos_db::DbError>(row)
    }
    .await;
    let row = match operation {
        Ok(row) => row,
        Err(e) => {
            let _ = state.store.remove_upload(&temp).await;
            return Err(if matches!(e, yhotos_db::DbError::InvalidInput) {
                HttpError::BadRequest
            } else {
                internal(e)
            });
        }
    };
    let mut r = StatusCode::CREATED.into_response();
    add(&mut r, "location", format!("/admin/upload/{}", row.id))?;
    add(&mut r, "upload-expires", expires(row.expires_at))?;
    Ok(r)
}
async fn head(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let mut tx = pool(&state)?.begin().await?;
    let row = uploads::lock(&mut tx, id, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let mut r = StatusCode::OK.into_response();
    if !row.metadata.is_empty() {
        if headers::metadata(&row.metadata).is_none() {
            return Err(internal("invalid stored metadata"));
        }
        add(&mut r, "upload-metadata", &row.metadata)?;
    }

    add(&mut r, "upload-offset", row.received_bytes)?;
    add(&mut r, "upload-length", row.declared_bytes)?;
    add(&mut r, "upload-expires", expires(row.expires_at))?;
    Ok(r)
}
async fn terminate(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let mut tx = pool(&state)?.begin().await?;
    let row = uploads::lock(&mut tx, id, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let temp: TempId = row.tmp_path.parse().map_err(internal)?;
    state.store.remove_upload(&temp).await.map_err(internal)?;
    uploads::remove(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
fn pool(state: &AppState) -> Result<&sqlx::PgPool, HttpError> {
    match &state.auth {
        Some(auth) => Ok(auth.db(crate::auth::Audience(true))?.pool()),
        None => Ok(&state.pool),
    }
}
fn validator(state: &AppState) -> Result<Ingest, HttpError> {
    let mut validator = Ingest::new(
        state.store.clone(),
        pool(state)?.clone(),
        state.config.worker.codec_host_path.clone(),
    );
    validator.codec_timeout = Duration::from_secs(u64::from(state.config.worker.codec_timeout));
    validator.concurrency_per_host = state.config.worker.vips_concurrency;
    validator.limits.max_bytes = state.config.upload.max_bytes;
    Ok(validator)
}
async fn patch(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
    request: Request,
) -> Result<Response, HttpError> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|v| v != "application/offset+octet-stream")
    {
        return Err(HttpError::Upload(StatusCode::UNSUPPORTED_MEDIA_TYPE));
    }
    let offset = number(request.headers(), "upload-offset")?;
    let mut tx = pool(&state)?.begin().await?;
    uploads::patch_deadline(&mut tx, state.config.worker.codec_timeout + 120).await?;
    let row = uploads::lock(&mut tx, id, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    if offset != row.received_bytes {
        return Err(HttpError::Upload(StatusCode::CONFLICT));
    }
    let temp: TempId = row.tmp_path.parse().map_err(internal)?;
    let mut file = state.store.open_upload(&temp).await.map_err(internal)?;
    let cap = u64::from(state.config.upload.chunk_bytes)
        .min(u64::try_from(row.declared_bytes - offset).map_err(internal)?);
    let next = stream::write(
        &mut file,
        request.into_body(),
        u64::try_from(offset).map_err(internal)?,
        cap,
    )
    .await
    .map_err(HttpError::Upload)?;
    drop(file);
    let mut r = StatusCode::NO_CONTENT.into_response();
    add(&mut r, "upload-offset", next)?;
    add(&mut r, "upload-expires", expires(row.expires_at))?;
    if next < u64::try_from(row.declared_bytes).map_err(internal)? {
        uploads::advance(&mut tx, id, i64::try_from(next).map_err(internal)?).await?;
        tx.commit().await?;
        return Ok(r);
    }
    let path = state.store.upload_path_for_host(&temp).map_err(internal)?;
    let Ok(validated) = validator(&state)?.finish(&path).await else {
        state.store.remove_upload(&temp).await.map_err(internal)?;
        uploads::remove(&mut tx, id).await?;
        tx.commit().await?;
        return Err(HttpError::Upload(StatusCode::UNPROCESSABLE_ENTITY));
    };
    let input = state.store.open_upload(&temp).await.map_err(internal)?;
    let put = state
        .store
        .put_original(input, u64::try_from(row.declared_bytes).map_err(internal)?)
        .await
        .map_err(internal)?;
    let hash = put.hash.to_string();
    uploads::lock_hash(&mut tx, &hash).await?;
    let existing = photos::get_by_hash(&mut tx, &hash)
        .await?
        .into_iter()
        .next();
    let (photo, result) = if let Some(photo) = existing {
        (photo, "duplicate")
    } else {
        let photo = photos::create(
            &mut tx,
            &photos::NewPhoto {
                album_id: row.album_id,
                title: row.filename.clone(),
                original_hash: hash,
                original_name: row.filename,
                original_bytes: row.declared_bytes,
                mime: validated.mime.into(),
                kind: validated.kind,
            },
        )
        .await?;
        Queue::enqueue_in(
            &mut tx,
            &Job::Ingest {
                photo_id: photo.id,
                original_hash: put.hash,
                generation: 0,
            },
            0,
            None,
        )
        .await
        .map_err(internal)?;
        (photo, "created")
    };
    uploads::remove(&mut tx, id).await?;
    tx.commit().await?;
    // A crash after commit leaves only an orphan directory; the hourly sweep removes it.
    if state.store.remove_upload(&temp).await.is_err() {
        tracing::warn!(upload_id=%id,"upload cleanup deferred");
    }
    add(&mut r, "upload-result", result)?;
    add(
        &mut r,
        "location",
        format!("/admin/api/photos/{}", photo.short_id),
    )?;
    Ok(r)
}
async fn status(
    State(state): State<AppState>,
    _current: Current,
    Path(id): Path<String>,
) -> Result<Response, HttpError> {
    let short: ShortId = id.parse().map_err(|_| HttpError::NotFound)?;
    let mut conn = pool(&state)?.acquire().await?;
    let status = uploads::status(&mut conn, &short.to_string())
        .await?
        .ok_or(HttpError::NotFound)?;
    Ok(axum::Json(serde_json::json!({"status":status})).into_response())
}
#[derive(Template)]
#[template(path = "default/admin/upload.html")]
struct UploadPage<'a> {
    csrf: &'a str,
    nonce: Nonce,
    script: &'a str,
    chunk: u32,
    albums: Vec<(String, String)>,
}
async fn page(
    State(state): State<AppState>,
    _current: Current,
    Extension(nonce): Extension<Nonce>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let token = CsrfToken::for_page(&headers)?;
    let mut conn = pool(&state)?.acquire().await?;
    let albums = yhotos_db::albums::list_tree(&mut conn).await?;
    let mut choices = Vec::new();
    for album in albums.iter().filter(|a| a.deleted_at.is_none()) {
        let mut title = album.title.clone();
        let mut parent = album.parent_id;
        let mut depth = 0;
        while let Some(id) = parent {
            if depth >= 32 {
                break;
            }
            let Some(a) = albums.iter().find(|a| a.id == id) else {
                break;
            };
            title = format!("{} / {title}", a.title);
            parent = a.parent_id;
            depth += 1;
        }
        choices.push((album.id.to_string(), title));
    }
    choices.sort_by(|a, b| a.1.cmp(&b.1));
    let body = UploadPage {
        csrf: token.form_value(),
        nonce,
        script: crate::static_assets::upload_url()?,
        chunk: state.config.upload.chunk_bytes,
        albums: choices,
    }
    .render()
    .map_err(internal)?;
    Ok(([(header::SET_COOKIE, token.cookie(true)?)], Html(body)).into_response())
}
