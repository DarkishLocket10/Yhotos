//! Video parsing stays in the confined host; only verified snapshots are published.
use crate::{Ingest, JobError, host::retry};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yhotos_core::PhotoId;
use yhotos_storage::{Area, ContentHash, DerivativeKey, TempDir};
/// Trusted startup settings, validated again by the host at its input boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Canonicalizable ffmpeg executable, never supplied by an upload.
    pub ffmpeg: PathBuf,
    /// Canonicalizable ffprobe executable.
    pub ffprobe: PathBuf,
    /// Maximum input duration in seconds.
    pub max_duration: u32,
    /// Transcode wall deadline in seconds.
    pub timeout: u32,
    /// Threads per encoder process, at most sixteen.
    pub threads: u8,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            ffmpeg: "/usr/bin/ffmpeg".into(),
            ffprobe: "/usr/bin/ffprobe".into(),
            max_duration: 1800,
            timeout: 1800,
            threads: 2,
        }
    }
}
#[derive(Clone, Deserialize)]
pub(crate) struct Info {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    #[serde(rename = "hdr")]
    pub _hdr: bool,
}
#[derive(Deserialize)]
struct Output {
    name: String,
    bytes: u64,
    blake3: String,
    width: u32,
    height: u32,
    av1_level: Option<u8>,
}
#[derive(Deserialize)]
struct Report {
    info: Info,
    outputs: Vec<Output>,
}
pub(crate) struct Rendition {
    pub codec: &'static str,
    pub hash: ContentHash,
    pub bytes: i64,
    pub width: i32,
    pub height: i32,
    pub av1_level: Option<i16>,
}
pub(crate) struct Assets {
    pub info: Info,
    pub renditions: Vec<Rendition>,
    pub scrub: ContentHash,
    pub scrub_bytes: i64,
}
fn bad() -> JobError {
    JobError::Fatal("invalid video output".into())
}
impl Ingest {
    /// Transcodes the current original, fencing publication against concurrent replacements.
    /// Timeouts retry through the ordinary durable job policy.
    /// # Errors
    /// Returns safe fatal validation errors or retryable sandbox/IO/time failures.
    pub async fn transcode_video(&self, photo: PhotoId, attempt: i32) -> Result<(), JobError> {
        self.transcode_video_for(photo, None, attempt).await
    }
    pub(crate) async fn transcode_video_for(
        &self,
        photo: PhotoId,
        expected: Option<(&ContentHash, i64)>,
        attempt: i32,
    ) -> Result<(), JobError> {
        let source=sqlx::query!("SELECT original_hash,generation,kind::text AS \"kind!\",status::text AS \"status!\",derivative_plan FROM photos WHERE id=$1 AND deleted_at IS NULL",photo.0).fetch_optional(&self.db).await.map_err(|_|retry("video lookup failed"))?;
        let Some(source) = source else {
            return Ok(());
        };
        if source.kind != "video" {
            return Ok(());
        }
        let hash: ContentHash = source.original_hash.parse().map_err(|_| bad())?;
        if expected.is_some_and(|(original, generation)| {
            *original != hash || generation != source.generation
        }) {
            return Ok(());
        }
        let (plan, outputs) = self.plan.expand()?;
        let marker = blake3::hash(
            format!(
                "video-v1:{hash}:{plan}:{}",
                serde_json::to_string(&self.video).map_err(|_| bad())?
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        if source.status == "ready"
            && source.derivative_plan.as_deref() == Some(&marker)
            && self.video_exists(photo).await?
        {
            return Ok(());
        }
        let result=async {
            sqlx::query!("UPDATE photos SET status='processing',failure_reason=NULL WHERE id=$1 AND original_hash=$2 AND generation=$3 AND deleted_at IS NULL",photo.0,source.original_hash,source.generation).execute(&self.db).await.map_err(|_|retry("video update failed"))?;
            drop(self.store.open_read(Area::Originals,&hash,None).await.map_err(|_|retry("video original unavailable"))?);
            let dir=self.store.tmp_dir().map_err(|_|retry("video storage unavailable"))?;
            let report:Report=self.request_video_host(&serde_json::json!({"input":self.store.path_original(&hash),"out_dir":dir.path(),"settings":self.video}),attempt).await?;
            if report.info.width==0 || report.info.height==0 || report.info.width>7680 || report.info.height>7680 || report.info.duration_ms==0 || report.info.duration_ms>u64::from(self.video.max_duration)*1000 || report.outputs.len()!=4 {return Err(bad());}
            let snapshot=self.store.tmp_dir().map_err(|_|retry("video storage unavailable"))?;
            let mut hashes=Vec::new();
            for name in ["h264.mp4","av1.mp4","poster.jpeg","scrub.jpeg"] {
                let mut matching=report.outputs.iter().filter(|o|o.name==name); let output=matching.next().ok_or_else(bad)?;
                if matching.next().is_some() || output.width==0 || output.height==0 {return Err(bad());}
                if name=="av1.mp4" && output.av1_level.is_none_or(|level| level>23) {return Err(bad());}
                hashes.push(snapshot_output(dir.path(),&snapshot,name,output).await?);
            }
            let mut renditions=Vec::new();
            for (index,codec,long,short) in [(0,"h264",1080,1080),(1,"av1",3840,2160)] {
                let output=report.outputs.get(index).ok_or_else(bad)?; let hash=*hashes.get(index).ok_or_else(bad)?;
                if !valid_dimensions(output.width,output.height,long,short) {return Err(bad());}
                self.store.put_video(&hash,codec,&snapshot.path().join(format!("{codec}.mp4"))).await.map_err(|_|retry("video placement failed"))?;
                renditions.push(Rendition {codec,hash,bytes:i64::try_from(output.bytes).map_err(|_|bad())?,width:i32::try_from(output.width).map_err(|_|bad())?,height:i32::try_from(output.height).map_err(|_|bad())?,av1_level:output.av1_level.map(i16::from)});
            }
            let scrub=*hashes.get(3).ok_or_else(bad)?; let scrub_output=report.outputs.get(3).ok_or_else(bad)?;
            if (scrub_output.width,scrub_output.height)!=(1600,1600) {return Err(bad());}
            self.store.put_derivative(&DerivativeKey {hash:&scrub,variant:"scrub",format:"jpeg",hdr:false},&snapshot.path().join("scrub.jpeg")).await.map_err(|_|retry("scrub placement failed"))?;
            let assets=Assets {info:report.info,renditions,scrub,scrub_bytes:i64::try_from(scrub_output.bytes).map_err(|_|bad())?};
            // Posters go through exactly the normal image variant, report-validation
            // and immutable-snapshot path, including AVIF/JXL and JPEG fallback.
            let poster_dir=self.store.tmp_dir().map_err(|_|retry("poster storage unavailable"))?;
            let poster=self.run_host(&snapshot.path().join("poster.jpeg"),poster_dir.path(),&outputs,attempt).await?;
            let placeholder=self.validate_report(&poster)?;
            let verified=self.verify_outputs(poster_dir.path(),&outputs,&poster).await?;
            self.publish(photo,&source.original_hash,source.generation,&marker,&poster,&outputs,&verified,&placeholder,-2,Some(&assets)).await
        }.await;
        if let Err(error) = &result {
            let (status, reason) = match error {
                JobError::Fatal(_) => ("failed", Some("video rejected")),
                JobError::Retry { .. } if attempt >= 3 => {
                    ("failed", Some("video processing failed"))
                }
                JobError::Retry { .. } => ("pending", None),
            };
            sqlx::query!("UPDATE photos SET status=$3::text::photo_status,failure_reason=$4 WHERE id=$1 AND original_hash=$2 AND generation=$5 AND status<>'ready'",photo.0,source.original_hash,status,reason,source.generation).execute(&self.db).await.map_err(|_|retry("video failure update failed"))?;
        }
        result
    }
    async fn video_exists(&self, photo: PhotoId) -> Result<bool, JobError> {
        let rows = sqlx::query!(
            "SELECT hash,codec FROM video_renditions WHERE photo_id=$1 AND NOT hls",
            photo.0
        )
        .fetch_all(&self.db)
        .await
        .map_err(|_| retry("video lookup failed"))?;
        if rows.len() != 2 {
            return Ok(false);
        }
        for row in rows {
            let hash: ContentHash = row.hash.parse().map_err(|_| bad())?;
            if !matches!(row.codec.as_str(), "h264" | "av1")
                || !self
                    .store
                    .exists(
                        Area::Video,
                        &hash,
                        Some(&DerivativeKey {
                            hash: &hash,
                            variant: &row.codec,
                            format: "mp4",
                            hdr: false,
                        }),
                    )
                    .await
            {
                return Ok(false);
            }
        }
        let rows = sqlx::query!(
            "SELECT hash,variant,format FROM derivatives WHERE photo_id=$1 AND NOT hdr",
            photo.0
        )
        .fetch_all(&self.db)
        .await
        .map_err(|_| retry("poster lookup failed"))?;
        if rows.len() < 4 {
            return Ok(false);
        }
        for row in rows {
            let hash: ContentHash = row.hash.parse().map_err(|_| bad())?;
            if !self
                .store
                .exists(
                    Area::Derivatives,
                    &hash,
                    Some(&DerivativeKey {
                        hash: &hash,
                        variant: &row.variant,
                        format: &row.format,
                        hdr: false,
                    }),
                )
                .await
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
fn valid_dimensions(width: u32, height: u32, long: u32, short: u32) -> bool {
    width > 0 && height > 0 && width.max(height) <= long && width.min(height) <= short
}

async fn snapshot_output(
    dir: &Path,
    snapshot: &TempDir,
    name: &str,
    report: &Output,
) -> Result<ContentHash, JobError> {
    use std::os::unix::fs::MetadataExt;
    let path = dir.join(name);
    let meta = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| bad())?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.len() != report.bytes
        || report.bytes == 0
        || report.bytes > 20 << 30
        || !tokio::fs::canonicalize(&path)
            .await
            .map_err(|_| bad())?
            .starts_with(dir)
    {
        return Err(bad());
    }
    let mut input = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .await
        .map_err(|_| bad())?;
    let mut out = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o640)
        .open(snapshot.path().join(name))
        .await
        .map_err(|_| retry("snapshot unavailable"))?;
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0_u64;
    let mut buf = vec![0; 256 * 1024];
    loop {
        let n = input.read(&mut buf).await.map_err(|_| bad())?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > report.bytes {
            return Err(bad());
        }
        let chunk = buf.get(..n).ok_or_else(bad)?;
        hash.update(chunk);
        out.write_all(chunk)
            .await
            .map_err(|_| retry("snapshot unavailable"))?;
    }
    out.flush()
        .await
        .map_err(|_| retry("snapshot unavailable"))?;
    let hash: ContentHash = hash
        .finalize()
        .to_hex()
        .as_str()
        .parse()
        .map_err(|_| bad())?;
    if bytes != report.bytes || hash.to_string() != report.blake3 {
        return Err(bad());
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::valid_dimensions;

    #[test]
    fn rendition_bounds_accept_both_four_k_orientations_and_reject_oversize() {
        for (width, height) in [(3840, 2160), (2160, 3840), (1920, 1080)] {
            assert!(valid_dimensions(width, height, 3840, 2160));
        }
        for (width, height) in [(3842, 2160), (2160, 3842), (2162, 2162), (0, 1080)] {
            assert!(!valid_dimensions(width, height, 3840, 2160));
        }
        assert!(valid_dimensions(1080, 608, 1080, 1080));
        assert!(!valid_dimensions(1920, 1080, 1080, 1080));
    }
}
