use crate::{
    Ingest, JobError,
    host::retry,
    plan::Planned,
    protocol::{Kind, Report},
};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yhotos_core::PhotoId;
use yhotos_jobs::{Job, Queue};
use yhotos_storage::{Area, ContentHash, DerivativeKey, TempDir};

pub(crate) struct Verified {
    dir: TempDir,
    hashes: Vec<ContentHash>,
}
impl Ingest {
    pub(crate) async fn published_outputs_exist(
        &self,
        photo: PhotoId,
        plans: &[Planned],
    ) -> Result<bool, JobError> {
        let rows=sqlx::query!("SELECT variant,format,hash,width,height,bytes FROM derivatives WHERE photo_id=$1 ORDER BY variant,format",photo.0).fetch_all(&self.db).await.map_err(|_|retry("derivative lookup failed"))?;
        if rows.len() != plans.len() {
            return Ok(false);
        }
        for plan in plans {
            let Some(row) = rows
                .iter()
                .find(|r| r.variant == plan.variant && r.format == plan.format.as_str())
            else {
                return Ok(false);
            };
            let Ok(hash) = row.hash.parse::<ContentHash>() else {
                return Ok(false);
            };
            if row.width <= 0 || row.height <= 0 || row.bytes <= 0 {
                return Ok(false);
            }
            let key = DerivativeKey {
                hash: &hash,
                variant: &plan.variant,
                format: plan.format.as_str(),
                hdr: false,
            };
            let Ok(file) = self
                .store
                .open_read(Area::Derivatives, &hash, Some(&key))
                .await
            else {
                return Ok(false);
            };
            let Ok(metadata) = file.metadata().await else {
                return Ok(false);
            };
            if i64::try_from(metadata.len()).ok() != Some(row.bytes) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) async fn verify_outputs(
        &self,
        dir: &Path,
        plans: &[Planned],
        report: &Report,
    ) -> Result<Verified, JobError> {
        let bad = || JobError::Fatal("codec output verification failed".into());
        if report.outputs.len() != plans.len() {
            return Err(bad());
        }
        let verified = self
            .store
            .tmp_dir()
            .map_err(|_| retry("temporary storage unavailable"))?;
        let mut hashes = Vec::new();
        for plan in plans {
            let mut matching = report.outputs.iter().filter(|o| o.name == plan.name);
            let output = matching.next().ok_or_else(bad)?;
            if matching.next().is_some()
                || output.bytes == 0
                || output.bytes > 2 << 30
                || output.width == 0
                || output.height == 0
                || output.width > plan.long_edge
                || output.height > plan.long_edge
                || (plan.square && output.width != output.height)
            {
                return Err(bad());
            }
            // These names come exclusively from the validated plan, never the report.
            let source = dir.join(&plan.name);
            let meta = tokio::fs::symlink_metadata(&source)
                .await
                .map_err(|_| bad())?;
            if !meta.is_file() || meta.len() != output.bytes {
                return Err(bad());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(bad());
                }
            }
            let canonical = tokio::fs::canonicalize(&source).await.map_err(|_| bad())?;
            if !canonical.starts_with(dir) {
                return Err(bad());
            }
            let mut options = tokio::fs::OpenOptions::new();
            options.read(true).custom_flags(nix::libc::O_NOFOLLOW);
            let mut input = options.open(&source).await.map_err(|_| bad())?;
            let mut copy = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o640)
                .open(verified.path().join(&plan.name))
                .await
                .map_err(|_| retry("verified output unavailable"))?;
            // Snapshot and hash in the same streaming pass. The exact verified bytes,
            // rather than a mutable child-produced pathname, are later published.
            let mut digest = blake3::Hasher::new();
            let mut buffer = vec![0; 256 * 1024];
            let mut bytes = 0_u64;
            loop {
                let n = input.read(&mut buffer).await.map_err(|_| bad())?;
                if n == 0 {
                    break;
                }
                bytes += n as u64;
                if bytes > output.bytes {
                    return Err(bad());
                }
                let chunk = buffer.get(..n).ok_or_else(bad)?;
                digest.update(chunk);
                copy.write_all(chunk)
                    .await
                    .map_err(|_| retry("verified output write failed"))?;
            }
            copy.flush()
                .await
                .map_err(|_| retry("verified output write failed"))?;
            drop(copy);
            let hash: ContentHash = digest
                .finalize()
                .to_hex()
                .as_str()
                .parse()
                .map_err(|_| bad())?;
            if bytes != output.bytes || hash.to_string() != output.blake3 {
                return Err(bad());
            }
            hashes.push(hash);
        }
        Ok(Verified {
            dir: verified,
            hashes,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn publish(
        &self,
        photo: PhotoId,
        original: &str,
        marker: &str,
        report: &Report,
        plans: &[Planned],
        verified: &Verified,
        placeholder: &[u8],
        followup_priority: i32,
    ) -> Result<(), JobError> {
        let (taken, taken_local) =
            crate::timestamp::capture_time(&report.metadata, &self.default_timezone)?;
        let mut ids = Vec::new();
        let mut variants = Vec::new();
        let mut formats = Vec::new();
        let mut hashes = Vec::new();
        let mut widths = Vec::new();
        let mut heights = Vec::new();
        let mut sizes = Vec::new();
        for (plan, hash) in plans.iter().zip(&verified.hashes) {
            let output = report
                .outputs
                .iter()
                .find(|o| o.name == plan.name)
                .ok_or_else(|| JobError::Fatal("missing output".into()))?;
            self.store
                .put_derivative(
                    &DerivativeKey {
                        hash,
                        variant: &plan.variant,
                        format: plan.format.as_str(),
                        hdr: false,
                    },
                    &verified.dir.path().join(&plan.name),
                )
                .await
                .map_err(|_| retry("derivative placement failed"))?;
            ids.push(uuid::Uuid::now_v7());
            variants.push(plan.variant.clone());
            formats.push(plan.format.as_str().to_owned());
            hashes.push(hash.to_string());
            widths.push(
                i32::try_from(output.width)
                    .map_err(|_| JobError::Fatal("invalid output width".into()))?,
            );
            heights.push(
                i32::try_from(output.height)
                    .map_err(|_| JobError::Fatal("invalid output height".into()))?,
            );
            sizes.push(
                i64::try_from(output.bytes)
                    .map_err(|_| JobError::Fatal("invalid output size".into()))?,
            );
        }
        let id = &report.identity;
        let m = &report.metadata;
        let (w, h) = if id.orientation >= 5 {
            (id.height, id.width)
        } else {
            (id.width, id.height)
        };
        let width = i32::try_from(w).map_err(|_| JobError::Fatal("invalid image width".into()))?;
        let height =
            i32::try_from(h).map_err(|_| JobError::Fatal("invalid image height".into()))?;
        let colors: Vec<i32> = report
            .dominant_colors
            .iter()
            .map(|[r, g, b]| (i32::from(*r) << 16) | (i32::from(*g) << 8) | i32::from(*b))
            .collect();
        let iso = m.iso.and_then(|n| i32::try_from(n).ok());
        let shutter = m.shutter.map(|n| n.to_string());
        let (lat, lon) = m
            .gps
            .map_or((None, None), |(lat, lon)| (Some(lat), Some(lon)));
        let raw = matches!(id.kind, Kind::Raw(_));
        let mut tx = self
            .db
            .begin()
            .await
            .map_err(|_| retry("ingest transaction failed"))?;
        let changed=sqlx::query!("UPDATE photos SET width=$3,height=$4,orientation=$5,is_hdr=$6,has_gain_map=$6,icc_name=$7,camera_make=$8,camera_model=$9,lens=$10,focal_mm=$11,aperture=$12,shutter=$13,iso=$14,gps_lat=$15,gps_lon=$16,taken_at=coalesce($17,taken_at),dominant_colors=$18,placeholder=$19,kind=CASE WHEN $20 THEN 'raw'::media_kind ELSE kind END,status='ready',failure_reason=NULL,derivative_plan=$21,taken_at_local=coalesce($22,taken_at_local) WHERE id=$1 AND original_hash=$2 AND deleted_at IS NULL", photo.0,original,width,height,i32::from(id.orientation),id.has_gain_map,id.icc_desc,m.make,m.model,m.lens,m.focal_mm,m.aperture,shutter,iso,lat,lon,taken,&colors,placeholder,raw,marker,taken_local).execute(&mut *tx).await.map_err(|_|retry("photo publication failed"))?.rows_affected();
        if changed != 1 {
            return Err(retry("original changed during ingest"));
        }
        // Remove only obsolete plan entries; upsert preserves identities of retained entries.
        sqlx::query!("DELETE FROM derivatives WHERE photo_id=$1 AND (hdr OR (variant,format) NOT IN (SELECT * FROM UNNEST($2::text[],$3::text[])))",photo.0,&variants,&formats).execute(&mut *tx).await.map_err(|_|retry("derivative cleanup failed"))?;
        sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) SELECT id,$1,variant,format,false,hash,width,height,bytes FROM UNNEST($2::uuid[],$3::text[],$4::text[],$5::text[],$6::int[],$7::int[],$8::bigint[]) AS d(id,variant,format,hash,width,height,bytes) ON CONFLICT(photo_id,variant,format,hdr) DO UPDATE SET hash=excluded.hash,width=excluded.width,height=excluded.height,bytes=excluded.bytes",photo.0,&ids,&variants,&formats,&hashes,&widths,&heights,&sizes).execute(&mut *tx).await.map_err(|_|retry("derivative publication failed"))?;
        let mut followups = vec![
            Job::MlEmbed { photo_id: photo },
            Job::MlFaces { photo_id: photo },
            Job::MlOcr { photo_id: photo },
            Job::MlTags { photo_id: photo },
        ];
        if m.gps.is_some() {
            followups.push(Job::GeoReverse { photo_id: photo });
        }
        for job in followups {
            Queue::enqueue_in(&mut tx, &job, followup_priority, None)
                .await
                .map_err(|_| retry("follow up enqueue failed"))?;
        }
        tx.commit()
            .await
            .map_err(|_| retry("ingest commit failed"))?;
        Ok(())
    }
}
