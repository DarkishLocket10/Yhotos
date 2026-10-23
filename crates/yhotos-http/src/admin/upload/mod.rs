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
use yhotos_jobs::{Job, JobError, Queue};
use yhotos_pipeline::Ingest;
use yhotos_storage::TempId;

/// Mounts only on the authenticated admin listener.
pub(crate) fn route_table() -> Vec<(&'static str, axum::routing::MethodRouter<AppState>)> {
    vec![
        ("/admin/upload", get(page).post(create).options(options)),
        (
            "/admin/upload/{id}",
            axum::routing::head(head).patch(patch).delete(terminate),
        ),
        ("/admin/upload/{id}/finish", axum::routing::post(finish)),
        ("/admin/api/photos/{id}/status", get(status)),
    ]
}
/// Mounts only on the authenticated admin listener.
pub fn routes() -> Router<AppState> {
    route_table()
        .into_iter()
        .fold(Router::new(), |router, (path, methods)| {
            router.route(path, methods)
        })
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
    super::guard(&current)?;
    let replace = headers
        .get("upload-replace-photo")
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|s| s.parse::<Uuid>().ok())
                .ok_or(HttpError::BadRequest)
        })
        .transpose()?;
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
        if let Some(photo) = replace {
            yhotos_db::library::bind_replacement(&mut tx, row.id, photo).await?;
        }
        yhotos_db::audit::write_target(
            &mut tx,
            Some(current.0.session.user_id),
            "upload.create",
            "upload",
            Some(row.id),
            &serde_json::json!({"album_id":album,"replace_photo_id":replace}),
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
    let Some(row) = uploads::lock(&mut tx, id, current.0.session.user_id).await? else {
        // A completed upload is already a published photo; terminating it is a
        // conflict, not a missing upload, so the client never reports "cancelled".
        return Err(
            if uploads::was_completed(&mut tx, id, current.0.session.user_id).await? {
                HttpError::Upload(StatusCode::CONFLICT)
            } else {
                HttpError::NotFound
            },
        );
    };
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
    let row = uploads::lock_for_patch(&mut tx, id, current.0.session.user_id)
        .await
        .map_err(|error| match error {
            yhotos_db::DbError::Sqlx(sqlx::Error::Database(ref database))
                if database.code().as_deref() == Some("55P03") =>
            {
                HttpError::Upload(StatusCode::CONFLICT)
            }
            _ => HttpError::from(error),
        })?;
    let Some(mut row) = row else {
        // Completion inserts the owner marker and deletes the upload atomically.
        // A fresh statement snapshot also observes a winner that just committed.
        return Err(
            if uploads::was_completed(&mut tx, id, current.0.session.user_id).await? {
                HttpError::Upload(StatusCode::CONFLICT)
            } else {
                HttpError::NotFound
            },
        );
    };
    if offset != row.received_bytes {
        return Err(HttpError::Upload(StatusCode::CONFLICT));
    }
    let temp: TempId = row.tmp_path.parse().map_err(internal)?;
    let mut file = state
        .store
        .open_upload(&temp)
        .await
        .map_err(|_| HttpError::UploadUnavailable)?;
    let cap = u64::from(state.config.upload.chunk_bytes)
        .min(u64::try_from(row.declared_bytes - offset).map_err(internal)?);
    let next = stream::write(
        &mut file,
        request.into_body(),
        u64::try_from(offset).map_err(internal)?,
        cap,
    )
    .await
    .map_err(|status| {
        if status.is_server_error() {
            HttpError::UploadUnavailable
        } else {
            HttpError::Upload(status)
        }
    })?;
    drop(file);
    let previous_offset = row.received_bytes;
    row.received_bytes = i64::try_from(next).map_err(internal)?;
    uploads::advance(&mut tx, id, row.received_bytes).await?;
    if next < u64::try_from(row.declared_bytes).map_err(internal)? {
        tx.commit().await?;
        return progress(&row);
    }
    complete(&state, tx, row, previous_offset).await
}
fn progress(row: &uploads::Upload) -> Result<Response, HttpError> {
    let mut r = StatusCode::NO_CONTENT.into_response();
    add(&mut r, "upload-offset", row.received_bytes)?;
    add(&mut r, "upload-expires", expires(row.expires_at))?;
    Ok(r)
}
async fn finish(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<Uuid>,
) -> Result<Response, HttpError> {
    let mut tx = pool(&state)?.begin().await?;
    uploads::patch_deadline(&mut tx, state.config.worker.codec_timeout + 120).await?;
    let row = uploads::lock_for_write(&mut tx, id, current.0.session.user_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    if row.received_bytes != row.declared_bytes {
        return Err(HttpError::Upload(StatusCode::CONFLICT));
    }
    let previous_offset = row.received_bytes;
    complete(&state, tx, row, previous_offset).await
}
async fn prepare_original(
    state: &AppState,
    row: &uploads::Upload,
    temp: &TempId,
) -> Result<(yhotos_pipeline::validate::Validated, yhotos_storage::Put), HttpError> {
    let path = state
        .store
        .upload_path_for_host(temp)
        .map_err(|_| HttpError::UploadUnavailable)?;
    let validated = validator(state)?
        .finish(&path)
        .await
        .map_err(|error| match error {
            JobError::Fatal(_) => HttpError::Upload(StatusCode::UNPROCESSABLE_ENTITY),
            JobError::Retry { .. } => HttpError::UploadUnavailable,
        })?;
    let input = state
        .store
        .open_upload(temp)
        .await
        .map_err(|_| HttpError::UploadUnavailable)?;
    let put = state
        .store
        .put_original(input, u64::try_from(row.declared_bytes).map_err(internal)?)
        .await
        .map_err(|_| HttpError::UploadUnavailable)?;
    Ok((validated, put))
}
#[allow(clippy::too_many_lines)] // One upload transaction owns validation, replacement, and durable attribution.
async fn complete(
    state: &AppState,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    row: uploads::Upload,
    previous_offset: i64,
) -> Result<Response, HttpError> {
    // Both entry points coordinate only on the creator's upload during IO and validation.
    let id = row.id;
    let temp: TempId = row.tmp_path.parse().map_err(internal)?;
    let prepared = prepare_original(state, &row, &temp).await;
    let (validated, put) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if matches!(error, HttpError::Upload(StatusCode::UNPROCESSABLE_ENTITY)) {
                state
                    .store
                    .remove_upload(&temp)
                    .await
                    .map_err(|_| HttpError::UploadUnavailable)?;
                uploads::remove(&mut tx, id).await?;
            }
            // Retain the fsynced final offset on temporary failures. HEAD then lets a
            // client retry with an empty final PATCH or POST to the finish endpoint.
            tx.commit().await?;
            return Err(error);
        }
    };
    let mut r = progress(&row)?;
    let hash = put.hash.to_string();
    // Serialize identical publications before taking library coordination, so
    // competing uploads deduplicate normally rather than conflict on the tree.
    uploads::lock_hash(&mut tx, &hash).await?;
    // No tree or album lock is held while the request body streams or the host validates.
    // Revalidate under short-lived library coordination before changing any photo rows.
    let unavailable = match uploads::lock_for_completion(&mut tx, row.album_id).await {
        Ok(true) => None,
        Ok(false) => Some(HttpError::Upload(StatusCode::CONFLICT)),
        Err(yhotos_db::DbError::InvalidInput) => Some(HttpError::NotFound),
        Err(error) => return Err(HttpError::from(error)),
    };
    if let Some(error) = unavailable {
        // Validation may already have published bytes. Keep their GC identity even
        // when a concurrent purge cancels this upload before it can be retried.
        yhotos_db::library::abandoned_original(&mut tx, &hash).await?;
        if matches!(error, HttpError::NotFound) {
            // An unavailable destination rejects this PATCH's progress, while a
            // busy tree retains it for an empty final PATCH retry.
            uploads::advance(&mut tx, id, previous_offset).await?;
        }
        tx.commit().await?;
        return Err(error);
    }
    let replacement = yhotos_db::library::replacement_target(&mut tx, id).await?;
    let existing = photos::get_by_hash(&mut tx, &hash)
        .await?
        .into_iter()
        .next();
    let mut replaced_change = None;
    let (photo, result) = if let Some(target) = replacement {
        let original = photos::NewPhoto {
            album_id: row.album_id,
            title: row.filename.clone(),
            original_hash: hash,
            original_name: row.filename,
            original_bytes: row.declared_bytes,
            mime: validated.mime.into(),
            kind: validated.kind,
        };
        replaced_change = Some(
            match yhotos_db::library::replace(&mut tx, row.user_id, target, &original).await {
                Ok(change) => change,
                Err(yhotos_db::DbError::InvalidInput) => {
                    yhotos_db::library::abandoned_original(&mut tx, &original.original_hash)
                        .await?;
                    uploads::advance(&mut tx, id, previous_offset).await?;
                    tx.commit().await?;
                    return Err(HttpError::NotFound);
                }
                Err(error) => return Err(HttpError::from(error)),
            },
        );
        (
            photos::get_by_id(&mut tx, yhotos_core::PhotoId(target))
                .await?
                .ok_or(HttpError::NotFound)?,
            "replaced",
        )
    } else if let Some(photo) = existing {
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
    uploads::record_completion(&mut tx, id, row.user_id).await?;
    uploads::remove(&mut tx, id).await?;
    // Durable attribution for every publication, in the same transaction.
    yhotos_db::audit::write_target(
        &mut tx,
        Some(row.user_id),
        "upload.complete",
        "photo",
        Some(photo.id.0),
        &serde_json::json!({"outcome": result, "upload_id": id, "album_id": row.album_id.0, "bytes": row.declared_bytes}),
    )
    .await?;
    tx.commit().await?;
    if replaced_change.is_some() {
        super::cache::changed(state);
    } else {
        state.files.invalidate_all();
        if let Some(auth) = &state.auth {
            auth.invalidate_all();
        }
    }
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
    current: Current,
    Path(id): Path<String>,
) -> Result<Response, HttpError> {
    super::guard(&current)?;
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
    current: Current,
    Extension(nonce): Extension<Nonce>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    super::guard(&current)?;
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
