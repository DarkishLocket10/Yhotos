//! Worker-owned protocol types. No decoder is linked into the worker process.
use serde::{Deserialize, Serialize};

/// Limits sent only through the host's JSON protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Header pixel ceiling, including all pages.
    pub max_pixels: u64,
    /// Original byte ceiling.
    pub max_bytes: u64,
    /// Maximum image dimension.
    pub max_dim: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pixels: 250_000_000,
            max_bytes: 2 << 30,
            max_dim: 65_535,
        }
    }
}
#[derive(Deserialize)]
pub(crate) struct Report {
    #[serde(default)]
    pub hdr_skipped_reason: Option<HdrSkippedReason>,
    #[serde(default)]
    pub skipped_outputs: Vec<SkippedOutput>,
    pub identity: Identity,
    pub metadata: Metadata,
    pub thumbhash: String,
    pub dominant_colors: Vec<[u8; 3]>,
    pub outputs: Vec<OutputReport>,
}
#[derive(Deserialize)]
pub(crate) struct OutputReport {
    pub name: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub blake3: String,
}
#[derive(Deserialize)]
pub(crate) enum Kind {
    Jpeg,
    Png,
    Tiff,
    Heif,
    Avif,
    Jxl,
    Webp,
    Raw(String),
    Video { container: VideoContainer },
    Unknown,
}
impl Kind {
    // These are container sample depths, not the decoded working-image depth.
    // TIFF/JXL's 32-bit depths include floating-point samples.
    pub(crate) fn accepts_bits(&self, bits: u8) -> bool {
        match self {
            Self::Png => matches!(bits, 1 | 2 | 4 | 8 | 16),
            Self::Jpeg => matches!(bits, 8 | 12),
            Self::Heif | Self::Avif => matches!(bits, 8 | 10 | 12),
            Self::Tiff | Self::Jxl | Self::Raw(_) => (1..=32).contains(&bits),
            Self::Webp => bits == 8,
            Self::Unknown | Self::Video { .. } => false,
        }
    }
}
#[derive(Deserialize)]
pub(crate) struct Identity {
    pub kind: Kind,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: u8,
    pub bits: u8,
    pub pages: u32,
    pub has_gain_map: bool,
    pub icc_desc: Option<String>,
}
#[derive(Deserialize)]
pub(crate) struct Metadata {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub focal_mm: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter: Option<f64>,
    pub iso: Option<u32>,
    pub gps: Option<(f64, f64)>,
    pub taken_at: Option<String>,
    pub offset_time_original: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::Kind;

    #[test]
    fn report_depths_are_validated_for_each_container() {
        for bits in 0..=u8::MAX {
            for (kind, valid) in [
                (Kind::Png, [1, 2, 4, 8, 16].contains(&bits)),
                (Kind::Jpeg, [8, 12].contains(&bits)),
                (Kind::Heif, [8, 10, 12].contains(&bits)),
                (Kind::Avif, [8, 10, 12].contains(&bits)),
                (Kind::Tiff, bits > 0 && bits <= 32),
                (Kind::Jxl, bits > 0 && bits <= 32),
                (Kind::Raw("fixture".into()), bits > 0 && bits <= 32),
                (Kind::Webp, bits == 8),
                (Kind::Unknown, false),
            ] {
                assert_eq!(kind.accepts_bits(bits), valid, "depth {bits}");
            }
        }
    }
}

#[derive(Deserialize)]
pub(crate) enum VideoContainer {
    Mp4,
    Mov,
}

#[derive(Clone, Copy, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HdrSkippedReason {
    NoGainMap,
    InvalidGainMap,
    UnsupportedGainMap,
    #[serde(rename = "compressed JPEG XL gain map not supported")]
    CompressedJxlGainMap,
    GainMapLimits,
    EncoderUnavailable,
}
#[derive(Deserialize)]
pub(crate) struct SkippedOutput {
    pub name: String,
    pub reason: HdrSkippedReason,
}
