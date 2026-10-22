//! Upload preflight: bounded magic sniffing in the parent, identity parsing in the host only.
use crate::{
    Ingest, JobError,
    host::retry,
    protocol::{Kind, VideoContainer},
};
use std::{
    path::Path,
    time::{Duration, SystemTime},
};
use tokio::io::AsyncReadExt;
use yhotos_storage::TempId;

/// Agreed upload identity, with nullable dimensions for video without a track header.
pub struct Validated {
    /// Authoritative serving MIME from magic and host agreement.
    pub mime: &'static str,
    /// Database media vocabulary.
    pub kind: yhotos_db::photos::MediaKind,
}
#[derive(serde::Deserialize)]
struct IdentityReport {
    identity: crate::protocol::Identity,
}
impl Ingest {
    /// Validates a trusted storage path without decoding in this process.
    /// # Errors
    /// Invalid content is fatal; storage failures, host unavailability and timeouts are retryable.
    pub async fn finish(&self, path: &Path) -> Result<Validated, JobError> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|_| retry("upload unavailable"))?;
        let len = file
            .metadata()
            .await
            .map_err(|_| retry("upload unavailable"))?
            .len();
        if len == 0 || len > self.limits.max_bytes {
            return Err(rejected());
        }
        let mut head = vec![0; usize::try_from(len.min(4096)).map_err(|_| rejected())?];
        file.read_exact(&mut head)
            .await
            .map_err(|_| retry("upload unavailable"))?;
        let magic = sniff(&head).ok_or_else(rejected)?;
        let dir = self
            .store
            .tmp_dir()
            .map_err(|_| retry("temporary storage unavailable"))?;
        let report: IdentityReport = self
            .request_host(
                &serde_json::json!({"input":path,"out_dir":dir.path(),"limits":self.limits,"outputs":[],"want":["identity"]}),
                1,
            )
            .await?;
        let id = report.identity;
        let (mime, kind) = match &id.kind {
            Kind::Jpeg => ("image/jpeg", 0),
            Kind::Png => ("image/png", 0),
            Kind::Tiff => ("image/tiff", 0),
            Kind::Heif => ("image/heic", 0),
            Kind::Avif => ("image/avif", 0),
            Kind::Jxl => ("image/jxl", 0),
            Kind::Webp => ("image/webp", 0),
            Kind::Raw(make) if make.len() <= 256 => ("image/x-raw", 1),
            Kind::Video {
                container: VideoContainer::Mp4,
            } => ("video/mp4", 2),
            Kind::Video {
                container: VideoContainer::Mov,
            } => ("video/quicktime", 2),
            _ => return Err(rejected()),
        };
        if !(magic == mime
            || magic == "image/tiff" && kind == 1
            || magic == "image/x-raw" && kind == 1)
            || !(1..=8).contains(&id.orientation)
        {
            return Err(rejected());
        }
        match (kind == 2, id.width, id.height) {
            (true, None, None) if id.pages == 1 => {}
            (_, Some(width), Some(height)) if dimensions(&self.limits, width, height, id.pages) => {
            }
            _ => return Err(rejected()),
        }
        if kind != 2 && !id.kind.accepts_bits(id.bits) {
            return Err(rejected());
        }
        Ok(Validated {
            mime,
            kind: match kind {
                1 => yhotos_db::photos::MediaKind::Raw,
                2 => yhotos_db::photos::MediaKind::Video,
                _ => yhotos_db::photos::MediaKind::Image,
            },
        })
    }
    /// Sweeps expired upload rows under locks and abandoned durable directories after 24 hours.
    /// # Errors
    /// Returns retryable failures without leaking storage paths.
    pub async fn sweep_uploads(&self) -> Result<(), JobError> {
        loop {
            let mut tx = self
                .db
                .begin()
                .await
                .map_err(|_| retry("upload sweep failed"))?;
            yhotos_db::uploads::remove_expired_completions(&mut tx)
                .await
                .map_err(|_| retry("upload sweep failed"))?;
            let rows = yhotos_db::uploads::expired(&mut tx)
                .await
                .map_err(|_| retry("upload sweep failed"))?;
            let count = rows.len();
            for row in rows {
                let temp: TempId = row
                    .tmp_path
                    .parse()
                    .map_err(|_| retry("invalid upload identifier"))?;
                self.store
                    .remove_upload(&temp)
                    .await
                    .map_err(|_| retry("upload cleanup failed"))?;
                yhotos_db::uploads::remove(&mut tx, row.id)
                    .await
                    .map_err(|_| retry("upload sweep failed"))?;
            }
            tx.commit()
                .await
                .map_err(|_| retry("upload sweep failed"))?;
            if count < 100 {
                break;
            }
        }
        for (id, created) in self
            .store
            .list_uploads()
            .await
            .map_err(|_| retry("upload cleanup failed"))?
        {
            if SystemTime::now()
                .duration_since(created)
                .unwrap_or_default()
                < Duration::from_hours(24)
            {
                continue;
            }
            let mut conn = self
                .db
                .acquire()
                .await
                .map_err(|_| retry("upload sweep failed"))?;
            if !yhotos_db::uploads::references(&mut conn, &id.to_string())
                .await
                .map_err(|_| retry("upload sweep failed"))?
            {
                self.store
                    .remove_upload(&id)
                    .await
                    .map_err(|_| retry("upload cleanup failed"))?;
            }
        }
        Ok(())
    }
}
fn rejected() -> JobError {
    JobError::Fatal("upload rejected".into())
}
fn sniff(h: &[u8]) -> Option<&'static str> {
    if h.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if h.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if [b"II*\0", b"MM\0*", b"II+\0", b"MM\0+"]
        .iter()
        .any(|m| h.starts_with(*m))
    {
        Some("image/tiff")
    } else if h.starts_with(b"\xff\x0a") || h.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        Some("image/jxl")
    } else if h.starts_with(b"RIFF") && h.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if h.starts_with(b"IIU\0")
        || h.starts_with(b"IIRO")
        || h.starts_with(b"MMOR")
        || h.starts_with(b"FUJIFILMCCD-RAW")
    {
        Some("image/x-raw")
    } else if h.get(4..8) == Some(b"ftyp") {
        let n = usize::try_from(u32::from_be_bytes(h.get(..4)?.try_into().ok()?)).ok()?;
        if !(16..=4096).contains(&n) || !n.is_multiple_of(4) {
            return None;
        }
        let brands: Vec<_> = h
            .get(8..n)?
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter_map(|(i, b)| (i != 1).then_some(b))
            .collect();
        if brands.contains(&b"crx ") {
            Some("image/x-raw")
        } else if brands.iter().any(|b| matches!(*b, b"avif" | b"avis")) {
            Some("image/avif")
        } else if brands.iter().any(|b| {
            matches!(
                *b,
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
            )
        }) {
            Some("image/heic")
        } else if brands.contains(&b"qt  ") {
            Some("video/quicktime")
        } else if brands.iter().any(|b| {
            matches!(
                *b,
                b"isom"
                    | b"iso2"
                    | b"iso3"
                    | b"iso4"
                    | b"iso5"
                    | b"iso6"
                    | b"mp41"
                    | b"mp42"
                    | b"avc1"
                    | b"M4V "
            )
        }) {
            Some("video/mp4")
        } else {
            None
        }
    } else {
        None
    }
}

pub(crate) fn dimensions(limits: &crate::Limits, width: u32, height: u32, pages: u32) -> bool {
    width > 0
        && height > 0
        && pages > 0
        && width <= limits.max_dim
        && height <= limits.max_dim
        && u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| n.checked_mul(u64::from(pages)))
            .is_some_and(|n| n <= limits.max_pixels)
}
