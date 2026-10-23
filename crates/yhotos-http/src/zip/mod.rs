//! Authorized, bounded STORE archives; file contents flow directly to the HTTP body.
mod source;
mod transport;
pub mod writer;
use crate::{
    AppState,
    error::HttpError,
    files::streams::GuardedStream,
    middleware::{rate_limit::ClientIp, session::SessionIdentity},
    share::Links,
};
use axum::{
    Extension, Json,
    body::Body,
    extract::{Query, State},
    http::{HeaderValue, header},
    response::Response,
};
use serde::Deserialize;
use std::{
    net::IpAddr,
    sync::{Mutex, OnceLock},
};
use yhotos_core::{AlbumId, ShortId, access::Principal};
use yhotos_storage::{Area, ContentHash, DerivativeKey};
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Options {
    album: Option<AlbumId>,
    #[serde(default)]
    size: Size,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Size {
    #[default]
    Original,
    Large,
    Medium,
}
impl Size {
    fn name(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Large => "large",
            Self::Medium => "medium",
        }
    }
}
pub(crate) async fn album(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Extension(ip): Extension<ClientIp>,
    Query(options): Query<Options>,
) -> Result<Response, HttpError> {
    if options.album.is_none() {
        return Err(HttpError::BadRequest);
    }
    serve(state, identity, links, ip.0, options, Vec::new()).await
}
pub(crate) async fn selection(
    State(state): State<AppState>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
    Extension(ip): Extension<ClientIp>,
    Query(options): Query<Options>,
    Json(ids): Json<Vec<String>>,
) -> Result<Response, HttpError> {
    if options.album.is_some()
        || ids.is_empty()
        || ids.len() > 500
        || ids
            .iter()
            .any(|id| id.parse::<ShortId>().is_err() && id.parse::<uuid::Uuid>().is_err())
    {
        return Err(HttpError::BadRequest);
    }
    serve(state, identity, links, ip.0, options, ids).await
}
fn safe_name(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(80)
        .collect();
    if value.is_empty() {
        "download".into()
    } else {
        value
    }
}
fn extension(mime: &str) -> Result<&'static str, HttpError> {
    match mime {
        "image/jpeg" => Ok("jpeg"),
        "image/png" => Ok("png"),
        "image/avif" => Ok("avif"),
        "image/jxl" => Ok("jxl"),
        "image/heic" => Ok("heic"),
        "image/heif" => Ok("heif"),
        "image/tiff" => Ok("tiff"),
        "image/webp" => Ok("webp"),
        "image/x-raw" => Ok("raw"),
        "video/mp4" => Ok("mp4"),
        "video/quicktime" => Ok("mov"),
        _ => Err(HttpError::NotFound),
    }
}
fn visitor(ip: IpAddr) -> Result<[u8; 32], HttpError> {
    static SALT: OnceLock<Mutex<(i64, [u8; 32])>> = OnceLock::new();
    let day = time::OffsetDateTime::now_utc().unix_timestamp() / 86400;
    let mut salt = SALT
        .get_or_init(|| Mutex::new((-1, [0; 32])))
        .lock()
        .map_err(|_| HttpError::Internal(anyhow::anyhow!("event salt unavailable")))?;
    if salt.0 != day {
        getrandom::fill(&mut salt.1)
            .map_err(|_| HttpError::Internal(anyhow::anyhow!("event entropy unavailable")))?;
        salt.0 = day;
    }
    Ok(*blake3::keyed_hash(&salt.1, ip.to_string().as_bytes()).as_bytes())
}
#[allow(clippy::too_many_lines)] // Authorization and manifest checks precede body construction in one auditable flow.
async fn serve(
    state: AppState,
    identity: SessionIdentity,
    links: Links,
    ip: IpAddr,
    options: Options,
    ids: Vec<String>,
) -> Result<Response, HttpError> {
    let permit = state.zip_streams().acquire(ip)?;
    // A link bearer cannot regain public download permission when its link denies it.
    let downloadable = links
        .0
        .iter()
        .filter(|l| l.caps.view && l.caps.download)
        .map(|l| l.target)
        .collect::<Vec<_>>();
    if !links.0.is_empty()
        && downloadable.is_empty()
        && !matches!(identity.principal(), Principal::User { .. })
    {
        return Err(HttpError::NotFound);
    }
    let bindings =
        yhotos_db::access_sql::visible_photos(&identity.principal()).with_links(downloadable);
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let rows = yhotos_db::zip::manifest(
        &mut conn,
        &bindings,
        options.album.map(|a| a.0),
        &ids,
        options.size.name(),
    )
    .await
    .map_err(|e| HttpError::Internal(e.into()))?;
    if rows.is_empty() {
        return Err(HttpError::NotFound);
    }
    if rows.len() > 100_000 {
        return Err(HttpError::BadRequest);
    }
    let filename = if let Some(album) = options.album {
        let album = yhotos_db::albums::get_by_id(&mut conn, album)
            .await
            .map_err(|e| HttpError::Internal(e.into()))?
            .ok_or(HttpError::NotFound)?;
        safe_name(album.slug.as_str())
    } else {
        "selection".into()
    };
    let mut files = Vec::with_capacity(rows.len());
    let mut total = 98_u64;
    for (index, row) in rows.into_iter().enumerate() {
        let hash: ContentHash = row.hash.parse().map_err(|_| HttpError::NotFound)?;
        let bytes = u64::try_from(row.bytes).map_err(|_| HttpError::NotFound)?;
        let name = format!(
            "{:03}-{}.{}",
            index + 1,
            safe_name(if row.title.is_empty() {
                &row.short_id
            } else {
                &row.title
            }),
            extension(&row.mime)?
        );
        // Worst-case local ZIP64 + descriptor + ZIP64 central entry.
        total = total
            .checked_add(bytes)
            .and_then(|n| n.checked_add(30 + 20 + 24 + 46 + 28 + 2 * name.len() as u64))
            .ok_or(HttpError::BadRequest)?;
        if total > state.config.zip.max_bytes {
            return Err(HttpError::BadRequest);
        }
        files.push((name, hash, bytes));
    }
    yhotos_db::zip::record(
        &mut conn,
        options.album.map(|a| a.0),
        &visitor(ip)?,
        &serde_json::json!({"size":options.size.name(),"count":files.len()}),
    )
    .await
    .map_err(|e| HttpError::Internal(e.into()))?;
    drop(conn);
    let (output, reader, finished) = transport::channel();
    let store = state.store.clone();
    let cap = state.config.zip.max_bytes;
    let lifetime = state.config.transfer_deadline;
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(async move {
            let stream = async {
                let mut zip = writer::ZipWriter::new(output, cap);
                for (name, hash, size) in files {
                    let key = DerivativeKey {
                        hash: &hash,
                        variant: options.size.name(),
                        format: "jpeg",
                        hdr: false,
                    };
                    let original = matches!(options.size, Size::Original);
                    let file = store
                        .open_read(
                            if original {
                                Area::Originals
                            } else {
                                Area::Derivatives
                            },
                            &hash,
                            (!original).then_some(&key),
                        )
                        .await
                        .map_err(|_| std::io::Error::other("archive file unavailable"))?;
                    zip.entry(name, size, source::File(Some(file.into_std().await)))
                        .await?;
                }
                zip.finish().await
            };
            let complete = matches!(tokio::time::timeout(lifetime, stream).await, Ok(Ok(())));
            if !complete {
                tracing::warn!("archive stream incomplete");
            }
            let _ = finished.send(complete);
        });
    });
    let body = Body::from_stream(GuardedStream::new(reader, permit, lifetime));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}.zip\""))
            .map_err(|e| HttpError::Internal(e.into()))?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests;
