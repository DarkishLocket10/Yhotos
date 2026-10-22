use crate::JobError;
use serde::{Deserialize, Serialize};

/// Encoders supported by the host and derivative serving layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// AVIF, ICC preserved.
    Avif,
    /// JPEG XL, ICC preserved.
    Jxl,
    /// Progressive JPEG fallback.
    Jpeg,
}
impl Format {
    /// Stable storage and database format token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Avif => "avif",
            Self::Jxl => "jxl",
            Self::Jpeg => "jpeg",
        }
    }
    pub(crate) fn encode(self, long_edge: u32) -> serde_json::Value {
        // Small variants retain AVIF Q60/speed6, JXL distance1/effort7 and
        // progressive JPEG Q90/4:4:4. Above 1600px, integer AVIF/JPEG quality
        // and 0.01 JXL distance sweeps select the smallest passing settings:
        // edge  | AVIF Q | JXL distance | JPEG Q
        // 2048  |   86   |    0.68      |   98
        // 2560  |   84   |    0.81      |   98
        // 3200  |   83   |    0.98      |    -
        // Each next smaller setting fails the unchanged 0.98 gate. Full-size
        // local 8x8 SSIM vs independent linear thumbnails (libvips 8.18.7):
        // variant  | AVIF bytes/SSIM   | JXL bytes/SSIM    | JPEG bytes/SSIM
        // thumb    |   7603/0.980006   |  14741/0.990502   |  22472/0.979364
        // thumb2x  |  17616/0.981469   |  38290/0.990469   |  63871/0.977494
        // small    |  15027/0.982941   |  31830/0.990923   |  52478/0.980660
        // small2x  |  37968/0.983699   |  88095/0.992482   | 158388/0.980878
        // medium   |  37968/0.983699   |  88095/0.992482   | 158388/0.980878
        // medium2x | 702806/0.980711   | 667758/0.980060   |1828413/0.981021
        // large    | 469754/0.980388   | 459335/0.980233   |1193745/0.980588
        // xlarge   | 971377/0.981363   | 873372/0.980129   |       -
        // photo.jpg supplies edges <=1600; photo-large.jpg supplies larger
        // edges at their actual sizes. Thumb variants crop first, never upscale.
        // The pipeline test also checks P3 chart deltaE and AVIF/JXL ICC retention.
        match self {
            Self::Avif => {
                let quality = match long_edge {
                    0..=1600 => 60,
                    1601..=2048 => 86,
                    2049..=2560 => 84,
                    _ => 83,
                };
                serde_json::json!({"Avif":{"quality":quality,"speed":6,"hdr":false}})
            }
            Self::Jxl => {
                let distance = match long_edge {
                    0..=1600 => 1.0,
                    1601..=2048 => 0.68,
                    2049..=2560 => 0.81,
                    _ => 0.98,
                };
                serde_json::json!({"Jxl":{"distance":distance,"effort":7,"hdr":false}})
            }
            Self::Jpeg => {
                let quality = if long_edge > 1600 { 98 } else { 90 };
                serde_json::json!({"Jpeg":{"quality":quality,"progressive":true,"chroma_subsampling":false,"hdr":false}})
            }
        }
    }
}
/// A configured resize from the original working image, never an intermediate derivative.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    /// Storage token, 1..=14 lowercase ASCII letters/digits (leaves room for `2x`).
    pub name: String,
    /// Maximum long edge, without upscaling.
    pub long_edge: u32,
    /// Center crop to a square before resizing.
    pub square: bool,
    /// One output per selected encoder.
    pub formats: Vec<Format>,
    /// Also generate twice the long edge.
    pub scale2x: bool,
}
/// Serializable plan whose digest changes when any output or encoder policy changes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantPlan {
    /// Requested outputs; expanded plan is capped at the host's 32-output ceiling.
    pub variants: Vec<Variant>,
}
impl Default for VariantPlan {
    fn default() -> Self {
        Self {
            variants: [
                ("thumb", 320, true, true),
                ("small", 640, false, true),
                ("medium", 1280, false, true),
                ("large", 2048, false, false),
                ("xlarge", 3200, false, false),
            ]
            .into_iter()
            .map(|(name, long_edge, square, scale2x)| Variant {
                name: name.into(),
                long_edge,
                square,
                scale2x,
                formats: if name == "xlarge" {
                    vec![Format::Avif, Format::Jxl]
                } else {
                    vec![Format::Avif, Format::Jxl, Format::Jpeg]
                },
            })
            .collect(),
        }
    }
}
pub(crate) struct Planned {
    pub variant: String,
    pub name: String,
    pub format: Format,
    pub long_edge: u32,
    pub square: bool,
    pub keep_icc: bool,
}
impl VariantPlan {
    /// Checks names, dimensions, duplicate outputs and the host's output-count ceiling.
    ///
    /// # Errors
    /// Returns a safe fatal configuration error for an invalid plan.
    pub fn validate(&self) -> Result<(), JobError> {
        self.expand().map(|_| ())
    }
    pub(crate) fn expand(&self) -> Result<(String, Vec<Planned>), JobError> {
        let mut outputs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for v in &self.variants {
            if v.name.is_empty()
                || v.name.len() > 14
                || !v
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                || v.long_edge == 0
                || v.long_edge > if v.scale2x { 32_767 } else { 65_535 }
                || v.formats.is_empty()
            {
                return Err(JobError::Fatal("invalid variant plan".into()));
            }
            for scale in 1..=if v.scale2x { 2 } else { 1 } {
                for format in &v.formats {
                    let variant = if scale == 2 {
                        format!("{}2x", v.name)
                    } else {
                        v.name.clone()
                    };
                    let name = format!("{variant}.{}", format.as_str());
                    if !seen.insert(name.clone()) {
                        return Err(JobError::Fatal("duplicate variant".into()));
                    }
                    outputs.push(Planned {
                        variant,
                        name,
                        format: *format,
                        long_edge: v.long_edge * scale,
                        square: v.square,
                        keep_icc: *format != Format::Jpeg || v.long_edge >= 1280,
                    });
                }
            }
        }
        if outputs.is_empty() || outputs.len() > 32 {
            return Err(JobError::Fatal("invalid output count".into()));
        }
        // Encoder policy revision 2 invalidates derivatives made with the old
        // large-output settings, even when the configured sizes are unchanged.
        let bytes = serde_json::to_vec(&(2, self))
            .map_err(|_| JobError::Fatal("invalid variant plan".into()))?;
        Ok((blake3::hash(&bytes).to_hex().to_string(), outputs))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn default_plan_has_expected_sizes_encoders_and_stable_version() {
        let mut plan = VariantPlan::default();
        let (hash, outputs) = plan.expand().unwrap();
        assert_eq!(outputs.len(), 23);
        assert_eq!(
            outputs
                .iter()
                .filter(|o| o.variant == "medium2x" && o.long_edge == 2560)
                .count(),
            3
        );
        assert!(
            !outputs
                .iter()
                .any(|o| o.variant == "xlarge" && o.format == Format::Jpeg)
        );
        assert_eq!(plan.expand().unwrap().0, hash);
        plan.variants[0].long_edge += 1;
        assert_ne!(plan.expand().unwrap().0, hash);
    }
    #[test]
    fn rejects_paths_duplicates_and_excess_outputs() {
        let mut plan = VariantPlan::default();
        plan.variants[0].name = "../bad".into();
        assert!(plan.expand().is_err());
        let mut plan = VariantPlan::default();
        plan.variants.push(plan.variants[0].clone());
        assert!(plan.expand().is_err());
    }
}
