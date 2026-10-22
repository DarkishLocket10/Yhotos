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
    Unknown,
}
#[derive(Deserialize)]
pub(crate) struct Identity {
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
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
}
