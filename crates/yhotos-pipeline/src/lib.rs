#![forbid(unsafe_code)]
//! Ingest commits only verified, immutable outputs and atomically queues follow-up work.
mod faces;
mod host;
pub use faces::FaceCrop;
mod plan;
mod protocol;
mod publish;
mod timestamp;
pub mod validate;
pub mod video;
pub use plan::{Format, Variant, VariantPlan};
pub use protocol::Limits;
pub use yhotos_jobs::JobError;

use base64::{Engine, engine::general_purpose::STANDARD};
use host::retry;
use sqlx::PgPool;
use std::{path::PathBuf, time::Duration};
use yhotos_core::PhotoId;
use yhotos_jobs::{Job, JobCtx, JobHandler, JobRow, Queue};
use yhotos_storage::{Area, ContentHash, Store};

/// Parent-side limits and store access. Image parsing happens exclusively in the codec host.
#[derive(Clone)]
pub struct Ingest {
    /// Validated application-owned storage tree.
    pub store: Store,
    /// Worker-role database pool.
    pub db: PgPool,
    /// Trusted executable, configured at startup, never derived from a request.
    pub codec_host: PathBuf,
    /// Configured output sizes and formats.
    pub plan: VariantPlan,
    /// Header/input limits enforced again on the report.
    pub limits: Limits,
    /// Positive libvips thread count per child, normally two.
    pub concurrency_per_host: u8,
    /// Wall-clock deadline covering writes, reads, and child exit, normally five minutes.
    pub codec_timeout: Duration,
    /// Video parser paths, duration and transcode deadline.
    pub video: video::Settings,
    /// Timezone for capture dates lacking an EXIF offset; defaults to UTC.
    /// DST folds choose the earlier instant and gaps shift forward by the gap.
    pub default_timezone: jiff::tz::TimeZone,
    /// Host-only delay, absent in production builds.
    #[cfg(feature = "test-hooks")]
    pub test_delay: Option<Duration>,
    /// Test synchronization after the host exits, before reading any output.
    #[cfg(feature = "test-hooks")]
    pub test_pause: Option<std::sync::Arc<TestPause>>,
}
/// Explicit test synchronization, never selected by environment in production.
#[cfg(feature = "test-hooks")]
pub struct TestPause {
    /// Test receives the output directory after child exit.
    pub exited: tokio::sync::mpsc::Sender<PathBuf>,
    /// Allows verification to resume after intentional corruption.
    pub resume: tokio::sync::Notify,
}
impl Ingest {
    /// Uses the architecture's plan, five-minute host timeout and two libvips threads.
    #[must_use]
    pub fn new(store: Store, db: PgPool, codec_host: PathBuf) -> Self {
        Self {
            store,
            db,
            codec_host,
            plan: VariantPlan::default(),
            limits: Limits::default(),
            concurrency_per_host: 2,
            codec_timeout: Duration::from_secs(300),
            video: video::Settings::default(),
            default_timezone: jiff::tz::TimeZone::UTC,
            #[cfg(feature = "test-hooks")]
            test_delay: None,
            #[cfg(feature = "test-hooks")]
            test_pause: None,
        }
    }
    /// Ingests an existing original; repeating a completed hash/plan is a no-op.
    ///
    /// # Errors
    /// Returns safe retry/fatal outcomes and records failed photo status on fatal errors.
    pub async fn ingest(&self, photo_id: PhotoId) -> Result<(), JobError> {
        self.ingest_attempt(photo_id, 1).await
    }
    /// Uses the durable claim's attempt number to allow only one retry for decode failures.
    ///
    /// # Errors
    /// Returns safe retry/fatal outcomes and records failed photo status on fatal errors.
    pub async fn ingest_attempt(&self, photo_id: PhotoId, attempt: i32) -> Result<(), JobError> {
        self.ingest_with_priority(photo_id, None, attempt, 0).await
    }
    async fn ingest_with_priority(
        &self,
        photo_id: PhotoId,
        expected: Option<(&ContentHash, i64)>,
        attempt: i32,
        priority: i16,
    ) -> Result<(), JobError> {
        let followup_priority = i32::from(priority) - 1;
        let photo = sqlx::query!("SELECT original_hash,original_bytes,generation,kind::text AS \"kind!\",status::text AS \"status!\",derivative_plan FROM photos WHERE id=$1 AND deleted_at IS NULL",photo_id.0).fetch_optional(&self.db).await.map_err(|_|retry("photo lookup failed"))?.ok_or_else(||JobError::Fatal("photo unavailable".into()))?;
        let hash: ContentHash = photo
            .original_hash
            .parse()
            .map_err(|_| JobError::Fatal("invalid original hash".into()))?;
        // A delayed or retried job must never start work on its replacement.
        if expected.is_some_and(|(original, generation)| {
            *original != hash || generation != photo.generation
        }) {
            return Ok(());
        }
        if photo.kind == "video" {
            let mut tx = self
                .db
                .begin()
                .await
                .map_err(|_| retry("video enqueue failed"))?;
            // Serialize video routing against replacement too.
            if sqlx::query!("SELECT id FROM photos WHERE id=$1 AND original_hash=$2 AND generation=$3 AND deleted_at IS NULL FOR UPDATE",photo_id.0,photo.original_hash,photo.generation).fetch_optional(&mut *tx).await.map_err(|_|retry("photo lookup failed"))?.is_none() {
                return Ok(());
            }
            Queue::enqueue_in(
                &mut tx,
                &Job::VideoTranscode {
                    photo_id,
                    original_hash: hash,
                    generation: photo.generation,
                },
                followup_priority,
                None,
            )
            .await
            .map_err(|_| retry("video enqueue failed"))?;
            tx.commit()
                .await
                .map_err(|_| retry("video enqueue failed"))?;
            return Ok(());
        }
        let (plan_hash, outputs) = self.plan.expand()?;
        let marker = blake3::hash(format!("{hash}:{plan_hash}").as_bytes())
            .to_hex()
            .to_string();
        if photo.status == "ready"
            && photo.derivative_plan.as_deref() == Some(&marker)
            && self.published_outputs_exist(photo_id, &outputs).await?
        {
            return Ok(());
        }
        let result = async {
            if self.concurrency_per_host==0 || self.codec_timeout.is_zero() || self.codec_timeout>Duration::from_secs(600) || self.limits.max_pixels==0 || self.limits.max_dim==0 || self.limits.max_bytes==0 || u64::try_from(photo.original_bytes).map_or(true,|n|n>self.limits.max_bytes) { return Err(JobError::Fatal("invalid ingest limits".into())); }
            if sqlx::query!("UPDATE photos SET status='processing',failure_reason=NULL WHERE id=$1 AND original_hash=$2 AND generation=$3 AND deleted_at IS NULL",photo_id.0,photo.original_hash,photo.generation).execute(&self.db).await.map_err(|_|retry("photo update failed"))?.rows_affected() == 0 {
                return Ok(());
            }
            // Validate storage symlinks through the storage API before handing the typed
            // original path to the host, which canonicalizes and checks the regular file.
            let original = self.store.open_read(Area::Originals,&hash,None).await.map_err(|_|retry("original unavailable"))?;
            drop(original);
            let dir = self.store.tmp_dir().map_err(|_|retry("temporary storage unavailable"))?;
            let report = self.run_host(&self.store.path_original(&hash),dir.path(),&outputs,attempt).await?;
            #[cfg(feature="test-hooks")]
            if let Some(pause) = &self.test_pause {
                pause.exited.send(dir.path().to_path_buf()).await.map_err(|_|retry("test synchronization failed"))?;
                pause.resume.notified().await;
            }
            let placeholder = self.validate_report(&report)?;
            let verified = self.verify_outputs(dir.path(),&outputs,&report.outputs).await?;
            self.publish(photo_id,&photo.original_hash,photo.generation,&marker,&report,&outputs,&verified,&placeholder,followup_priority,None).await
        }.await;
        if let Err(error) = &result {
            let (status, reason) = match error {
                JobError::Fatal(reason) => {
                    ("failed", Some(host::failure_reason(reason.as_bytes())))
                }
                JobError::Retry { reason, .. } if attempt >= 3 => {
                    ("failed", Some(host::failure_reason(reason.as_bytes())))
                }
                JobError::Retry { .. } => ("pending", None),
            };
            sqlx::query!("UPDATE photos SET status=$3::text::photo_status,failure_reason=$4 WHERE id=$1 AND original_hash=$2 AND generation=$5 AND status<>'ready'",photo_id.0,photo.original_hash,status,reason,photo.generation).execute(&self.db).await.map_err(|_|retry("photo failure update failed"))?;
        }
        result
    }
    fn validate_report(&self, r: &protocol::Report) -> Result<Vec<u8>, JobError> {
        let id = &r.identity;
        let bad = || JobError::Fatal("invalid codec report".into());
        if !id
            .width
            .zip(id.height)
            .is_some_and(|(w, h)| validate::dimensions(&self.limits, w, h, id.pages))
            || !(1..=8).contains(&id.orientation)
            || !id.kind.accepts_bits(id.bits)
            || r.dominant_colors.is_empty()
            || r.dominant_colors.len() > 16
        {
            return Err(bad());
        }
        if let protocol::Kind::Raw(make) = &id.kind
            && make.len() > 256
        {
            return Err(bad());
        }
        for text in [
            &id.icc_desc,
            &r.metadata.make,
            &r.metadata.model,
            &r.metadata.lens,
            &r.metadata.taken_at,
            &r.metadata.offset_time_original,
        ]
        .into_iter()
        .flatten()
        {
            if text.len() > 256 || text.chars().any(char::is_control) {
                return Err(bad());
            }
        }
        for n in [r.metadata.focal_mm, r.metadata.aperture, r.metadata.shutter]
            .into_iter()
            .flatten()
        {
            if !n.is_finite() || n <= 0.0 {
                return Err(bad());
            }
        }
        if r.metadata
            .iso
            .is_some_and(|n| n == 0 || n > i32::MAX as u32)
            || r.metadata.gps.is_some_and(|(lat, lon)| {
                !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0
            })
        {
            return Err(bad());
        }
        let placeholder = STANDARD.decode(&r.thumbhash).map_err(|_| bad())?;
        if !(5..=128).contains(&placeholder.len()) {
            return Err(bad());
        }
        Ok(placeholder)
    }
}
#[async_trait::async_trait]
impl JobHandler for Ingest {
    async fn handle(&self, row: &JobRow, _: &JobCtx) -> Result<(), JobError> {
        match Job::from_row(row).map_err(|_| JobError::Fatal("invalid job payload".into()))? {
            Job::Ingest {
                photo_id,
                original_hash,
                generation,
            } => {
                let priority = i16::try_from(row.priority)
                    .map_err(|_| JobError::Fatal("invalid ingest priority".into()))?;
                self.ingest_with_priority(
                    photo_id,
                    Some((&original_hash, generation)),
                    row.attempts,
                    priority,
                )
                .await
            }
            Job::Gc => self.sweep_uploads().await,
            Job::VideoTranscode {
                photo_id,
                original_hash,
                generation,
            } => {
                self.transcode_video_for(photo_id, Some((&original_hash, generation)), row.attempts)
                    .await
            }
            _ => Err(JobError::Retry {
                after: Duration::from_secs(3600),
                reason: "not implemented".into(),
            }),
        }
    }
}
