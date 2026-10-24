//! Private cache access rechecks hidden state on every request, including cache hits.
use super::{AppState, Audience, Current, HttpError, auth, guard};
use axum::{
    body::Body,
    extract::{Path, State},
    http::header,
    response::Response,
};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;
use uuid::Uuid;
use yhotos_db::faces;
use yhotos_storage::{Area, ContentHash, DerivativeKey};
static CROPS: Semaphore = Semaphore::const_new(2);

pub(super) async fn serve(
    State(state): State<AppState>,
    current: Current,
    Path(face): Path<String>,
) -> Result<Response, HttpError> {
    guard(&current)?;
    let id = face
        .strip_suffix(".jpeg")
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or(HttpError::NotFound)?;
    let pool = auth::required(&state)?.db(Audience(true))?.pool();
    let preview = faces::preview(&mut *pool.acquire().await?, id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let mut cached = None;
    if let Some(hash) = preview.cached {
        let hash: ContentHash = hash.parse().map_err(|_| HttpError::NotFound)?;
        let key = DerivativeKey {
            hash: &hash,
            variant: "face",
            format: "jpeg",
            hdr: false,
        };
        cached = state
            .store
            .open_read(Area::Derivatives, &hash, Some(&key))
            .await
            .ok();
    }
    let file = if let Some(file) = cached {
        file
    } else {
        // Normal browser image parallelism should queue briefly, rather than
        // leaving cold previews as failed images when both hosts are occupied.
        let _permit = tokio::time::timeout(Duration::from_secs(2), CROPS.acquire())
            .await
            .map_err(|_| HttpError::RateLimited)?
            .map_err(|_| HttpError::RateLimited)?;
        let bbox: [f32; 4] = preview.bbox.try_into().map_err(|_| HttpError::NotFound)?;
        let source = preview.source.parse().map_err(|_| HttpError::NotFound)?;
        let mut host = yhotos_pipeline::Ingest::new(
            state.store.clone(),
            pool.clone(),
            state.config.worker.codec_host_path.clone(),
        );
        host.codec_timeout = Duration::from_secs(12);
        let crop = host
            .crop_face(&source, bbox)
            .await
            .map_err(|e| HttpError::Internal(anyhow::anyhow!("{e}")))?;
        if !faces::cache_crop(
            &mut *pool.acquire().await?,
            id,
            &bbox,
            &preview.source,
            &crop.hash.to_string(),
            crop.bytes,
        )
        .await?
        {
            return Err(HttpError::NotFound);
        }
        state
            .store
            .open_read(
                Area::Derivatives,
                &crop.hash,
                Some(&DerivativeKey {
                    hash: &crop.hash,
                    variant: "face",
                    format: "jpeg",
                    hdr: false,
                }),
            )
            .await
            .map_err(|_| HttpError::NotFound)?
    };
    let bytes = file
        .metadata()
        .await
        .map_err(|_| HttpError::NotFound)?
        .len();
    if bytes == 0 || bytes > 1 << 20 {
        return Err(HttpError::NotFound);
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::CONTENT_LENGTH, bytes)
        .body(Body::from_stream(ReaderStream::new(file)))
        .map_err(|e| HttpError::Internal(e.into()))
}
