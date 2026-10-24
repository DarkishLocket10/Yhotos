#![forbid(unsafe_code)]
//! Typed, durable jobs with fenced claims, bounded retries and cooperative shutdown.

mod worker;
pub use worker::{QueueConfig, retry_backoff};
pub use yhotos_db::jobs::JobRow;

use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use std::time::Duration;
use time::{Date, OffsetDateTime};
use yhotos_core::{JobId, PhotoId};
use yhotos_storage::ContentHash;

macro_rules! kinds {
    ($($name:ident => $wire:literal),+ $(,)?) => {
        /// Closed set of supported queue kinds; database strings are checked at the boundary.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        pub enum JobKind { $(#[doc = concat!("The `", $wire, "` queue.")] #[serde(rename = $wire)] $name),+ }
        impl JobKind {
            /// Stable database and metric label.
            #[must_use]
            pub const fn as_str(self) -> &'static str { match self { $(Self::$name => $wire),+ } }
            /// Maximum wall time of a single claim.
            #[must_use]
            pub const fn timeout(self) -> Duration {
                Duration::from_secs(match self { Self::Ingest => 600, Self::Zip => 1800, Self::Gc => 3600, _ => 300 })
            }
            /// Maximum attempts; placeholders explicitly defer without consuming attempts.
            #[must_use]
            pub const fn max_attempts(self) -> i32 { if matches!(self, Self::Ingest) { 3 } else { 5 } }
        }
    }
}
kinds! {
    Ingest => "ingest", MlEmbed => "ml.embed", MlFaces => "ml.faces", MlOcr => "ml.ocr",
    MlTags => "ml.tags", MlCluster => "ml.cluster", GeoReverse => "geo.reverse", VideoTranscode => "video.transcode",
    Zip => "zip", Gc => "gc", Rollup => "rollup", LegacyRedirectRebuild => "legacy_redirect_rebuild",
}

/// Validated job envelope. Payloads contain identifiers, never file paths or secrets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", deny_unknown_fields)]
pub enum Job {
    /// Generate derivatives and metadata.
    #[serde(rename = "ingest")]
    Ingest {
        /// Existing photo identifier.
        photo_id: PhotoId,
        /// Immutable original this job may process; also part of the dedupe key.
        #[serde(with = "original_hash")]
        original_hash: ContentHash,
        /// Nonnegative replacement generation; identical bytes still get a distinct job.
        #[serde(deserialize_with = "deserialize_generation")]
        generation: i64,
    },
    /// Generate semantic embedding.
    #[serde(rename = "ml.embed")]
    MlEmbed {
        /// Existing photo identifier.
        photo_id: PhotoId,
    },
    /// Detect faces.
    #[serde(rename = "ml.faces")]
    MlFaces {
        /// Existing photo identifier.
        photo_id: PhotoId,
    },
    /// Extract text.
    #[serde(rename = "ml.ocr")]
    MlOcr {
        /// Existing photo identifier.
        photo_id: PhotoId,
    },
    /// Generate tags.
    #[serde(rename = "ml.tags")]
    MlTags {
        /// Existing photo identifier.
        photo_id: PhotoId,
    },
    /// Rebuild private people clusters without merging named identities.
    #[serde(rename = "ml.cluster")]
    MlCluster,
    /// Resolve coordinates offline.
    #[serde(rename = "geo.reverse")]
    GeoReverse {
        /// Existing photo identifier.
        photo_id: PhotoId,
    },
    /// Generate video renditions.
    #[serde(rename = "video.transcode")]
    VideoTranscode {
        /// Existing photo identifier.
        photo_id: PhotoId,
    },
    /// Build an archive.
    #[serde(rename = "zip")]
    Zip {
        /// Archive request identifier.
        request_id: uuid::Uuid,
    },
    /// Remove unreferenced files.
    #[serde(rename = "gc")]
    Gc,
    /// Aggregate one calendar day.
    #[serde(rename = "rollup")]
    Rollup {
        /// UTC calendar day.
        day: Date,
    },
    /// Refresh legacy redirects.
    #[serde(rename = "legacy_redirect_rebuild")]
    LegacyRedirectRebuild,
}
impl Job {
    /// Returns the typed kind without interpreting payload strings.
    #[must_use]
    pub const fn kind(&self) -> JobKind {
        match self {
            Self::Ingest { .. } => JobKind::Ingest,
            Self::MlEmbed { .. } => JobKind::MlEmbed,
            Self::MlFaces { .. } => JobKind::MlFaces,
            Self::MlOcr { .. } => JobKind::MlOcr,
            Self::MlTags { .. } => JobKind::MlTags,
            Self::MlCluster => JobKind::MlCluster,
            Self::GeoReverse { .. } => JobKind::GeoReverse,
            Self::VideoTranscode { .. } => JobKind::VideoTranscode,
            Self::Zip { .. } => JobKind::Zip,
            Self::Gc => JobKind::Gc,
            Self::Rollup { .. } => JobKind::Rollup,
            Self::LegacyRedirectRebuild => JobKind::LegacyRedirectRebuild,
        }
    }
    /// Rejects unknown kinds and malformed identifiers read from the database.
    ///
    /// # Errors
    /// Returns `InvalidJob` for an invalid envelope.
    pub fn from_row(row: &JobRow) -> Result<Self, JobsError> {
        serde_json::from_value(serde_json::json!({"kind": row.kind, "payload": row.payload}))
            .map_err(|_| JobsError::InvalidJob)
    }
}

/// Queue failures contain no SQL, payloads, connection strings or paths.
#[derive(Debug, thiserror::Error)]
pub enum JobsError {
    /// A database operation failed.
    #[error("queue database operation failed")]
    Database(#[from] sqlx::Error),
    /// The checked repository rejected an operation.
    #[error("queue repository operation failed")]
    Repository(#[from] yhotos_db::DbError),
    /// An envelope or queue option was invalid.
    #[error("invalid job or worker configuration")]
    InvalidJob,
    /// A task panicked or was unexpectedly cancelled.
    #[error("worker task failed")]
    Task,
    /// Database unavailability prevented releasing claims within the shutdown budget.
    #[error("worker shutdown deadline exceeded")]
    ShutdownTimeout,
}
/// Handler outcomes determine durable retry or terminal failure.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Retry no sooner than this delay, also respecting exponential backoff.
    #[error("retry: {reason}")]
    Retry {
        /// Minimum delay.
        after: Duration,
        /// Safe diagnostic, never a secret or path.
        reason: String,
    },
    /// Finish with a durable error immediately.
    #[error("{0}")]
    Fatal(String),
}
/// Per-attempt context. Cancellation drops in-flight work before its claim is released.
#[derive(Clone)]
pub struct JobCtx {
    /// Typed enqueue client for follow-up jobs.
    pub queue: Queue,
    /// Cancelled when the shutdown grace period expires.
    pub shutdown: tokio_util::sync::CancellationToken,
}
/// Handlers must drop child processes and temporary resources when cancelled.
#[async_trait::async_trait]
pub trait JobHandler: Send + Sync + 'static {
    /// Returns only after durable effects are committed.
    async fn handle(&self, job: &JobRow, ctx: &JobCtx) -> Result<(), JobError>;
}
/// A cloneable typed queue; all completion and release operations are claim-fenced.
#[derive(Clone)]
pub struct Queue {
    pool: PgPool,
    config: QueueConfig,
}
impl Queue {
    /// Uses CPU-scaled concurrency and the production polling/shutdown intervals.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            config: QueueConfig::default(),
        }
    }
    /// Installs bounded worker settings.
    ///
    /// # Errors
    /// Rejects zero concurrency or intervals, and shutdown periods above 30 seconds.
    pub fn with_config(pool: PgPool, config: QueueConfig) -> Result<Self, JobsError> {
        if config.concurrency == 0
            || config.poll_interval.is_zero()
            || config.shutdown_grace > Duration::from_secs(30)
        {
            return Err(JobsError::InvalidJob);
        }
        Ok(Self { pool, config })
    }
    /// Atomically deduplicates unfinished identical envelopes and notifies on commit.
    ///
    /// # Errors
    /// Returns database or envelope failures without partial insertion.
    pub async fn enqueue(
        &self,
        job: Job,
        priority: i16,
        run_after: Option<OffsetDateTime>,
    ) -> Result<JobId, JobsError> {
        let mut tx = self.pool.begin().await?;
        let id = Self::enqueue_in(&mut tx, &job, i32::from(priority), run_after).await?;
        tx.commit().await?;
        Ok(id)
    }
    /// Enqueues inside the caller's transaction, so follow-ups cannot precede photo publication.
    /// All typed enqueue writers serialize on the same envelope's advisory lock.
    ///
    /// # Errors
    /// Returns database or serialization failures; the caller must roll back on error.
    pub async fn enqueue_in(
        conn: &mut PgConnection,
        job: &Job,
        priority: i32,
        run_after: Option<OffsetDateTime>,
    ) -> Result<JobId, JobsError> {
        if job.kind() == JobKind::Ingest && i16::try_from(priority).is_err()
            || matches!(job, Job::Ingest { generation, .. } if *generation < 0)
        {
            return Err(JobsError::InvalidJob);
        }
        let value = serde_json::to_value(job).map_err(|_| JobsError::InvalidJob)?;
        let payload = value
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let kind = job.kind().as_str();
        Ok(yhotos_db::jobs::enqueue_unique(
            conn,
            kind,
            &payload,
            priority,
            run_after,
            job.kind().max_attempts(),
        )
        .await?)
    }
}

fn deserialize_generation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i64, D::Error> {
    use serde::de::Error;
    let generation = i64::deserialize(deserializer)?;
    if generation < 0 {
        return Err(D::Error::custom("invalid ingest generation"));
    }
    Ok(generation)
}

// Keep the wire digest a canonical string while validating database bytes into
// the storage layer's typed hash before a handler can use them.
mod original_hash {
    use super::ContentHash;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(
        hash: &ContentHash,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(hash)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ContentHash, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_| D::Error::custom("invalid original hash"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn every_payload_round_trips_and_unknown_input_is_rejected() {
        let photo_id = PhotoId::new();
        let jobs = vec![
            Job::Ingest {
                photo_id,
                original_hash: "a".repeat(64).parse().unwrap(),
                generation: 0,
            },
            Job::MlEmbed { photo_id },
            Job::MlFaces { photo_id },
            Job::MlOcr { photo_id },
            Job::MlTags { photo_id },
            Job::GeoReverse { photo_id },
            Job::VideoTranscode { photo_id },
            Job::Zip {
                request_id: uuid::Uuid::now_v7(),
            },
            Job::Gc,
            Job::Rollup {
                day: Date::from_ordinal_date(2026, 1).unwrap(),
            },
            Job::LegacyRedirectRebuild,
        ];
        for job in jobs {
            let value = serde_json::to_value(&job).unwrap();
            assert_eq!(serde_json::from_value::<Job>(value).unwrap(), job);
        }
        assert!(
            serde_json::from_value::<Job>(
                serde_json::json!({"kind":"shell","payload":{"photo_id":photo_id}})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Job>(
                serde_json::json!({"kind":"ingest","payload":{"photo_id":"../../private"}})
            )
            .is_err()
        );
        for hash in ["../private".into(), "A".repeat(64), "a".repeat(63)] {
            assert!(
                serde_json::from_value::<Job>(serde_json::json!({
                    "kind":"ingest", "payload":{"photo_id": photo_id, "original_hash": hash, "generation": 0}
                }))
                .is_err()
            );
        }
        for generation in [
            serde_json::json!(-1),
            serde_json::json!(0.5),
            serde_json::json!(u64::MAX),
            serde_json::Value::Null,
        ] {
            assert!(serde_json::from_value::<Job>(serde_json::json!({
                "kind":"ingest", "payload":{"photo_id": photo_id, "original_hash": "a".repeat(64), "generation": generation}
            })).is_err());
        }
        assert_eq!(JobKind::Ingest.timeout(), Duration::from_mins(10));
        assert_eq!(JobKind::Gc.timeout(), Duration::from_hours(1));
        assert_eq!(JobKind::Zip.timeout(), Duration::from_mins(30));
    }
}
