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
    pub(crate) fn encode(self, long_edge: u32, hdr: bool) -> serde_json::Value {
        // Large outputs include headroom for the Linux production AV1 build,
        // which scored 0.979201 at Q84/2560px and 0.978236 at Q83/3200px.
        // Q90 raises both failing settings by at least six quality points.
        // All large encoders target SSIM >=0.982 on macOS; smaller variants
        // retain settings already above their 0.972 target. See the real-host
        // fidelity test for per-platform scores, bytes, chart deltaE and ICC.
        match self {
            Self::Avif => {
                let quality = match long_edge {
                    0..=1600 => 60,
                    _ => 90,
                };
                serde_json::json!({"Avif":{"quality":quality,"speed":6,"hdr":hdr}})
            }
            Self::Jxl => {
                let distance = match long_edge {
                    0..=1600 => 1.0,
                    1601..=2048 => 0.55,
                    2049..=2560 => 0.65,
                    _ => 0.78,
                };
                serde_json::json!({"Jxl":{"distance":distance,"effort":7,"hdr":hdr}})
            }
            Self::Jpeg => {
                let quality = if long_edge > 1600 { 99 } else { 90 };
                serde_json::json!({"Jpeg":{"quality":quality,"progressive":true,"chroma_subsampling":false,"hdr":hdr}})
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
    /// Add HDR outputs at this variant when the source has a valid gain map.
    #[serde(default)]
    pub hdr: bool,
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
                hdr: matches!(name, "medium" | "large" | "xlarge"),
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
#[derive(Clone)]
pub(crate) struct Planned {
    pub hdr: bool,
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
                        hdr: false,
                        variant,
                        name,
                        format: *format,
                        long_edge: v.long_edge * scale,
                        square: v.square,
                        // The sRGB conversion applies to JPEG outputs below medium by
                        // their actual (scaled) size, so small2x keeps the source profile.
                        // A paired HDR base must retain identical device color even when
                        // a configured HDR edge is smaller than the defaults.
                        keep_icc: *format != Format::Jpeg
                            || v.long_edge * scale >= 1280
                            || v.hdr && scale == 1,
                    });
                }
            }
            if v.hdr {
                let mut formats = v.formats.clone();
                if !formats.contains(&Format::Jpeg) {
                    formats.push(Format::Jpeg);
                }
                for format in formats {
                    let name = format!("{}.{}.hdr", v.name, format.as_str());
                    if !seen.insert(name.clone()) {
                        return Err(JobError::Fatal("duplicate variant".into()));
                    }
                    outputs.push(Planned {
                        hdr: true,
                        variant: v.name.clone(),
                        name,
                        format,
                        long_edge: v.long_edge,
                        square: v.square,
                        keep_icc: true,
                    });
                }
            }
        }
        if outputs.is_empty() || outputs.len() > 32 {
            return Err(JobError::Fatal("invalid output count".into()));
        }
        // Encoder policy revision 5 adds HDR outputs and invalidates the old
        // SDR-only plan. Revision 4 invalidates derivatives made with the old
        // large-output settings or the old small2x JPEG profile conversion, even
        // when the configured sizes are unchanged.
        let bytes = serde_json::to_vec(&(5, self))
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
        assert_eq!(outputs.len(), 32);
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
                .any(|o| o.variant == "xlarge" && o.format == Format::Jpeg && !o.hdr)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod icc_policy_tests {
    use super::*;

    #[test]
    fn jpeg_keeps_source_profile_from_medium_size_by_actual_scaled_edge() {
        let (_, outputs) = VariantPlan::default().expand().unwrap();
        let jpeg = |variant: &str| {
            outputs
                .iter()
                .find(|o| o.variant == variant && o.format == Format::Jpeg)
                .unwrap()
        };
        // small2x is 1280 px, the same size as medium, so it must not be converted to sRGB.
        assert_eq!(jpeg("small2x").long_edge, 1280);
        assert!(jpeg("small2x").keep_icc);
        assert!(jpeg("medium").keep_icc);
        // Below medium, JPEG fallbacks convert to sRGB.
        assert!(!jpeg("small").keep_icc);
        assert!(!jpeg("thumb").keep_icc);
        // AVIF and JXL always keep the profile.
        assert!(
            outputs
                .iter()
                .filter(|o| o.format != Format::Jpeg)
                .all(|o| o.keep_icc)
        );
    }
    #[test]
    fn custom_hdr_edges_keep_the_same_profile_in_both_jpeg_renditions() {
        let mut plan = VariantPlan::default();
        let medium = plan
            .variants
            .iter_mut()
            .find(|v| v.name == "medium")
            .unwrap();
        medium.long_edge = 1000;
        let (_, outputs) = plan.expand().unwrap();
        let paired = outputs
            .iter()
            .filter(|o| o.variant == "medium" && o.format == Format::Jpeg)
            .collect::<Vec<_>>();
        assert_eq!(paired.len(), 2);
        assert!(paired.iter().any(|o| o.hdr));
        assert!(paired.iter().any(|o| !o.hdr));
        assert!(paired.iter().all(|o| o.long_edge == 1000 && o.keep_icc));
    }
}
