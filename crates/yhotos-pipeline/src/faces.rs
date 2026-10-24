//! Administrative preview generation; this module never links an image decoder.
use crate::{
    Ingest, JobError,
    host::retry,
    plan::{Format, Planned},
    protocol::OutputReport,
};
use yhotos_storage::{Area, ContentHash, DerivativeKey};

#[derive(serde::Deserialize)]
struct Report {
    outputs: Vec<OutputReport>,
}

/// A verified, content-addressed JPEG in the private `face` storage variant.
pub struct FaceCrop {
    /// Independently rehashed bytes, never a host-supplied digest alone.
    pub hash: ContentHash,
    /// Bounded output length; dimensions are always 256 by 256.
    pub bytes: u64,
}
impl Ingest {
    /// Crops only a validated medium JPEG in a bounded sandboxed host process.
    /// Callers must authorize the face before invoking this administrative operation.
    ///
    /// # Errors
    /// Returns sanitized errors for invalid boxes, missing inputs or host failure.
    pub async fn crop_face(
        &self,
        source: &ContentHash,
        bbox: [f32; 4],
    ) -> Result<FaceCrop, JobError> {
        let [x1, y1, x2, y2] = bbox;
        if bbox
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            || x2 - x1 < 0.01
            || y2 - y1 < 0.01
        {
            return Err(JobError::Fatal("invalid face box".into()));
        }
        let key = DerivativeKey {
            hash: source,
            variant: "medium",
            format: "jpeg",
            hdr: false,
        };
        drop(
            self.store
                .open_read(Area::Derivatives, source, Some(&key))
                .await
                .map_err(|_| retry("medium derivative unavailable"))?,
        );
        let input = self
            .store
            .path_derivative(&key)
            .map_err(|_| retry("medium derivative unavailable"))?;
        let dir = self
            .store
            .tmp_dir()
            .map_err(|_| retry("temporary storage unavailable"))?;
        let plan = Planned {
            variant: "face".into(),
            name: "face.jpeg".into(),
            format: Format::Jpeg,
            hdr: false,
            long_edge: 256,
            square: true,
            keep_icc: false,
        };
        let request = serde_json::json!({"input":input,"out_dir":dir.path(),"crop":bbox,
          "limits":{"max_pixels":4_000_000,"max_bytes":16_777_216,"max_dim":4096,"wall_seconds":10,"cpu_seconds":8,"max_file_size":1_048_576},
          "outputs":[{"name":plan.name,"resize":{"long_edge":256,"square":false},"encode":plan.format.encode(256, false),"keep_icc":false}]});
        let report: Report = self.request_host(&request, 1).await?;
        let output = report
            .outputs
            .first()
            .ok_or_else(|| retry("invalid crop report"))?;
        if report.outputs.len() != 1
            || output.width != 256
            || output.height != 256
            || output.bytes > 1 << 20
        {
            return Err(JobError::Fatal("invalid crop report".into()));
        }
        let verified = self
            .verify_outputs(dir.path(), &[plan], &report.outputs)
            .await?;
        let hash = *verified
            .hashes
            .first()
            .ok_or_else(|| retry("invalid crop digest"))?;
        self.store
            .put_derivative(
                &DerivativeKey {
                    hash: &hash,
                    variant: "face",
                    format: "jpeg",
                    hdr: false,
                },
                &verified.dir.path().join("face.jpeg"),
            )
            .await
            .map_err(|_| retry("crop publication failed"))?;
        Ok(FaceCrop {
            hash,
            bytes: output.bytes,
        })
    }
}
