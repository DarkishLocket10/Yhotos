//! Authorized immutable file serving. Metadata (including misses) expires after
//! 60 seconds; admin visibility/grant changes take effect at origin within that
//! TTL. Edge-cached public files additionally need the architecture's edge purge.
//! Axum/Hyper expose no safe file/sendfile body; reads use bounded 256 KiB chunks.

pub mod range;
mod streams;

use crate::{
    AppState,
    error::HttpError,
    middleware::{rate_limit::ClientIp, request_id::REQUEST_ID, session::SessionIdentity},
};
use axum::{
    Extension,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
};
use moka::future::Cache;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use yhotos_core::access::{AlbumAccess, PhotoAccess, Principal, can_view_photo};
use yhotos_db::{
    derivatives,
    photos::{self, OwnerRow},
};
use yhotos_storage::{Area, ContentHash, DerivativeKey};

const BUFFER: usize = 256 * 1024;

/// Bounded metadata caching by validated lookup, with no principal-specific responses.
#[derive(Clone)]
pub struct FileCache {
    rows: Cache<LookupKey, Arc<Vec<OwnerRow>>>,
    streams: Arc<streams::Streams>,
}
impl Default for FileCache {
    fn default() -> Self {
        Self::new(4, 64)
    }
}
impl FileCache {
    /// Caps original and video streams per IP and globally; zero fails closed.
    #[must_use]
    pub fn new(per_ip_stream_cap: usize, global_stream_cap: u16) -> Self {
        Self {
            rows: Cache::builder()
                .max_capacity(50_000)
                .time_to_live(Duration::from_secs(60))
                .build(),
            streams: streams::Streams::new(per_ip_stream_cap, global_stream_cap),
        }
    }
    /// Invalidates origin metadata after an admin mutation, or for cold-path measurement.
    /// In-flight lookups and already edge-cached responses are not revoked by this call.
    pub fn invalidate_all(&self) {
        self.rows.invalidate_all();
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum LookupKey {
    Original(ContentHash),
    Derivative {
        hash: ContentHash,
        variant: String,
        format: String,
        hdr: bool,
    },
    Video(ContentHash),
}

struct Selector {
    hash: ContentHash,
    area: Area,
    variant: String,
    format: String,
    hdr: bool,
}
impl Selector {
    fn parse(hash: &str, name: &str, area: Area) -> Result<Self, HttpError> {
        let hash = hash.parse().map_err(|_| HttpError::NotFound)?;
        let (variant, format, hdr) = match area {
            Area::Originals => {
                // The decoded filename is display-only and never part of a lookup.
                if name.len() > 255 {
                    return Err(HttpError::NotFound);
                }
                ("", "", false)
            }
            Area::Derivatives => {
                let (name, hdr) = name
                    .strip_suffix(".hdr")
                    .map_or((name, false), |s| (s, true));
                let (variant, format) = name.split_once('.').ok_or(HttpError::NotFound)?;
                if !matches!(format, "avif" | "jxl" | "jpeg") {
                    return Err(HttpError::NotFound);
                }
                (variant, format, hdr)
            }
            Area::Video => {
                let codec = name.strip_suffix(".mp4").ok_or(HttpError::NotFound)?;
                if !matches!(codec, "h264" | "av1") {
                    return Err(HttpError::NotFound);
                }
                (codec, "mp4", false)
            }
        };
        if area != Area::Originals
            && (variant.is_empty()
                || variant.len() > 16
                || !variant
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
        {
            return Err(HttpError::NotFound);
        }
        Ok(Self {
            hash,
            area,
            variant: variant.into(),
            format: format.into(),
            hdr,
        })
    }

    fn lookup_key(&self) -> LookupKey {
        match self.area {
            Area::Originals => LookupKey::Original(self.hash),
            Area::Derivatives => LookupKey::Derivative {
                hash: self.hash,
                variant: self.variant.clone(),
                format: self.format.clone(),
                hdr: self.hdr,
            },
            Area::Video => LookupKey::Video(self.hash),
        }
    }
}

pub(crate) async fn derivative(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, axum::extract::rejection::PathRejection>,
    Extension(identity): Extension<SessionIdentity>,
    request: Request,
) -> Result<Response, HttpError> {
    let Path((hash, name)) = path.map_err(|_| HttpError::NotFound)?;
    serve(
        &state,
        &identity.principal(),
        Selector::parse(&hash, &name, Area::Derivatives)?,
        request,
    )
    .await
}
pub(crate) async fn original(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, axum::extract::rejection::PathRejection>,
    Extension(identity): Extension<SessionIdentity>,
    request: Request,
) -> Result<Response, HttpError> {
    let Path((hash, name)) = path.map_err(|_| HttpError::NotFound)?;
    serve(
        &state,
        &identity.principal(),
        Selector::parse(&hash, &name, Area::Originals)?,
        request,
    )
    .await
}
pub(crate) async fn video(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, axum::extract::rejection::PathRejection>,
    Extension(identity): Extension<SessionIdentity>,
    request: Request,
) -> Result<Response, HttpError> {
    let Path((hash, name)) = path.map_err(|_| HttpError::NotFound)?;
    serve(
        &state,
        &identity.principal(),
        Selector::parse(&hash, &name, Area::Video)?,
        request,
    )
    .await
}

fn may_view(principal: &Principal<'_>, owner: &OwnerRow) -> bool {
    let Some((leaf, parents)) = owner.albums.split_first() else {
        return false;
    };
    let ancestors: Vec<_> = parents
        .iter()
        .map(|album| AlbumAccess {
            id: album.id,
            visibility: album.visibility,
            grants: &album.grants,
            ancestors: &[],
        })
        .collect();
    let album = AlbumAccess {
        id: leaf.id,
        visibility: leaf.visibility,
        grants: &leaf.grants,
        ancestors: &ancestors,
    };
    // Hashes locate metadata only. This check runs before any filesystem access,
    // conditional response or range evaluation, on cold AND cached requests.
    can_view_photo(
        principal,
        &PhotoAccess {
            id: owner.photo_id,
            album: &album,
            visibility: owner.photo_visibility,
            grants: &owner.photo_grants,
        },
    )
}

async fn serve(
    state: &AppState,
    principal: &Principal<'_>,
    selector: Selector,
    request: Request,
) -> Result<Response, HttpError> {
    let owners = lookup(state, &selector).await?;
    // Prefer an anonymously visible owner so shared public content has stable
    // cache policy even when another owner is restricted or the viewer is admin.
    let public_owner = owners
        .iter()
        .find(|row| may_view(&Principal::Anonymous, row));
    let owner = public_owner
        .or_else(|| owners.iter().find(|row| may_view(principal, row)))
        .ok_or(HttpError::NotFound)?;
    let public = public_owner.is_some();
    let key = DerivativeKey {
        hash: &selector.hash,
        variant: &selector.variant,
        format: &selector.format,
        hdr: selector.hdr,
    };
    let mut file = state
        .store
        .open_read(
            selector.area,
            &selector.hash,
            (selector.area != Area::Originals).then_some(&key),
        )
        .await
        .map_err(|_| {
            let request_id = REQUEST_ID
                .try_with(ToString::to_string)
                .unwrap_or_else(|_| "unavailable".into());
            tracing::error!(%request_id, photo_id = %owner.photo_id, "stored file unavailable");
            HttpError::NotFound
        })?;
    let etag = format!("\"{}\"", selector.hash);
    let mut response = file_response(owner, &selector, public, &request, &etag)?;
    if not_modified(request.headers(), &etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return Ok(response);
    }
    let Some(range) = apply_range(&request, owner.bytes, &etag, &mut response)? else {
        return Ok(response);
    };
    if request.method() == Method::HEAD {
        return Ok(response);
    }
    let permit = if matches!(selector.area, Area::Originals | Area::Video) {
        let ip = request
            .extensions()
            .get::<ClientIp>()
            .ok_or(HttpError::BadRequest)?
            .0;
        Some(state.files.streams.acquire(ip)?)
    } else {
        None
    };
    if range.start != 0 {
        file.seek(std::io::SeekFrom::Start(range.start))
            .await
            .map_err(|error| HttpError::Internal(error.into()))?;
    }
    let stream = ReaderStream::with_capacity(file.take(range.len), BUFFER);
    *response.body_mut() = match permit {
        Some(permit) => Body::from_stream(streams::GuardedStream::new(
            stream,
            permit,
            state.config.transfer_deadline,
        )),
        None => Body::from_stream(stream),
    };
    Ok(response)
}
async fn lookup(state: &AppState, selector: &Selector) -> Result<Arc<Vec<OwnerRow>>, HttpError> {
    state
        .files
        .rows
        .try_get_with(selector.lookup_key(), async {
            let mut conn = state
                .pool
                .acquire()
                .await
                .map_err(yhotos_db::DbError::from)?;
            let hash = selector.hash.to_string();
            let rows = match selector.area {
                Area::Originals => photos::owner_of_hash(&mut conn, &hash).await?,
                Area::Derivatives => {
                    derivatives::lookup(
                        &mut conn,
                        &hash,
                        &selector.variant,
                        &selector.format,
                        selector.hdr,
                    )
                    .await?
                }
                Area::Video => derivatives::lookup_video(&mut conn, &hash).await?,
            };
            Ok::<_, yhotos_db::DbError>(Arc::new(rows))
        })
        .await
        .map_err(|error| HttpError::Internal(anyhow::anyhow!("file lookup failed: {error}")))
}
fn file_response(
    owner: &OwnerRow,
    selector: &Selector,
    public: bool,
    request: &Request,
    etag: &str,
) -> Result<Response, HttpError> {
    let mut response = Response::new(Body::empty());
    let headers = response.headers_mut();
    insert(headers, header::CONTENT_TYPE, mime_header(&owner.mime)?);
    set(headers, header::CONTENT_LENGTH, owner.bytes)?;
    set(headers, header::ETAG, etag)?;
    insert(
        headers,
        header::ACCEPT_RANGES,
        HeaderValue::from_static("bytes"),
    );
    insert(
        headers,
        header::CACHE_CONTROL,
        HeaderValue::from_static(if public {
            "public, max-age=31536000, immutable"
        } else {
            "private, no-store"
        }),
    );
    insert(
        headers,
        "cross-origin-resource-policy",
        HeaderValue::from_static(if public {
            "cross-origin"
        } else {
            "same-origin"
        }),
    );
    if !public {
        insert(headers, header::VARY, HeaderValue::from_static("Cookie"));
    }
    if selector.area == Area::Originals {
        let download = request
            .uri()
            .query()
            .is_some_and(|query| query.split('&').any(|part| part == "download=1"));
        let disposition = if download { "attachment" } else { "inline" };
        set(
            headers,
            header::CONTENT_DISPOSITION,
            format!(
                "{disposition}; filename=\"{}\"",
                sanitize_filename(&owner.original_name)
            ),
        )?;
    }
    Ok(response)
}
fn apply_range(
    request: &Request,
    total: u64,
    etag: &str,
    response: &mut Response,
) -> Result<Option<range::ByteRange>, HttpError> {
    let mut start = 0;
    let mut length = total;
    // HTTP defines Range for GET only; HEAD returns the full representation headers.
    if request.method() == Method::GET
        && request
            .headers()
            .get(header::IF_RANGE)
            .is_none_or(|value| value.as_bytes() == etag.as_bytes())
    {
        let values = request.headers().get_all(header::RANGE);
        if let Some(value) = values.iter().next() {
            let parsed = if values.iter().count() == 1 {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| range::parse(value, total).ok())
            } else {
                None
            };
            let Some(range) = parsed else {
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                // An error for a particular Range must never replace a public
                // full representation in an intermediary's URI cache.
                if response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .is_none_or(|value| value.as_bytes() != b"private, no-store")
                {
                    insert(
                        response.headers_mut(),
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("no-store"),
                    );
                }
                set(
                    response.headers_mut(),
                    header::CONTENT_RANGE,
                    format!("bytes */{total}"),
                )?;
                set(response.headers_mut(), header::CONTENT_LENGTH, 0)?;
                return Ok(None);
            };
            start = range.start;
            length = range.len;
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            set(
                response.headers_mut(),
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, start + length - 1, total),
            )?;
            set(response.headers_mut(), header::CONTENT_LENGTH, length)?;
        }
    }
    Ok(Some(range::ByteRange { start, len: length }))
}
fn insert(
    headers: &mut HeaderMap,
    key: impl axum::http::header::IntoHeaderName,
    value: HeaderValue,
) {
    headers.insert(key, value);
}
fn set(
    headers: &mut HeaderMap,
    key: impl axum::http::header::IntoHeaderName,
    value: impl std::fmt::Display,
) -> Result<(), HttpError> {
    let value = HeaderValue::from_str(&value.to_string())
        .map_err(|error| HttpError::Internal(error.into()))?;
    insert(headers, key, value);
    Ok(())
}
fn mime_header(mime: &str) -> Result<HeaderValue, HttpError> {
    // Validate untrusted database text without deriving a type from the path/bytes.
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&c))
    };
    let valid = mime.len() <= 127
        && mime
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype));
    if !valid {
        return Err(HttpError::Internal(anyhow::anyhow!(
            "invalid stored media type"
        )));
    }
    HeaderValue::from_str(mime).map_err(|error| HttpError::Internal(error.into()))
}
fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
}
fn sanitize_filename(name: &str) -> String {
    let safe: String = name
        .chars()
        .take(200)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || matches!(safe.as_str(), "." | "..") {
        "download".into()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests;
