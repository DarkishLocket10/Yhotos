//! Small deterministic `WebVTT` metadata; native media parsing never occurs here.
use super::{AppState, Area, HttpError, Links, Selector, SessionIdentity, authorize, lookup};
use axum::{
    Extension,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use yhotos_storage::ContentHash;
pub(crate) async fn index_response(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Extension(identity): Extension<SessionIdentity>,
    Extension(links): Extension<Links>,
) -> Result<Response, HttpError> {
    let selector = Selector::parse(&hash, "h264.mp4", Area::Video)?;
    let owners = lookup(&state, &selector).await?;
    let (owner, _) = authorize(&owners, &identity.principal(), &links, false)?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| HttpError::Internal(e.into()))?;
    let (hash, duration) = yhotos_db::derivatives::scrub(&mut conn, owner.photo_id)
        .await
        .map_err(|e| HttpError::Internal(e.into()))?
        .ok_or(HttpError::NotFound)?;
    let hash: ContentHash = hash.parse().map_err(|_| HttpError::NotFound)?;
    let duration = u64::try_from(duration)
        .ok()
        .filter(|n| (1..=1_800_000).contains(n))
        .ok_or(HttpError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        scrub_index(duration, &format!("/i/{hash}/scrub.jpeg")),
    )
        .into_response())
}
/// Fixed 100-cell `WebVTT` index matching the host's 10x10 160-pixel sprite.
/// Callers supply a validated local sprite URL and a bounded duration.
#[must_use]
pub fn scrub_index(duration_ms: u64, sprite_url: &str) -> String {
    use std::fmt::Write;
    let mut vtt = String::from("WEBVTT\n\n");
    let stamp = |ms: u64| {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60,
            ms % 1000
        )
    };
    for n in 0..100 {
        let _ = writeln!(
            vtt,
            "{} --> {}\n{sprite_url}#xywh={},{},160,160\n",
            stamp(n * duration_ms / 100),
            stamp((n + 1) * duration_ms / 100),
            (n % 10) * 160,
            (n / 10) * 160
        );
    }
    vtt
}

#[cfg(test)]
mod tests {
    #[test]
    fn index_covers_every_sprite_cell_and_the_full_duration() {
        let vtt = super::scrub_index(1200, "/i/hash/scrub.jpeg");
        assert!(vtt.starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:00.012"));
        assert!(vtt.contains("00:00:01.188 --> 00:00:01.200"));
        assert_eq!(vtt.matches(" --> ").count(), 100);
        assert!(vtt.ends_with("#xywh=1440,1440,160,160\n\n"));
    }
}
