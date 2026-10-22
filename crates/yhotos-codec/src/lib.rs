#![deny(unsafe_op_in_unsafe_fn)]
//! Bounded image identification and color-managed, linear-light derivatives.

mod bmff;
mod headers;
mod metadata;
mod mpf;
mod native;
mod palette;
mod placeholder;
#[cfg(feature = "raw")]
mod raw;
pub mod sandbox;
mod tiff;

pub use metadata::{Metadata, metadata};
pub use palette::dominant_colors;
pub use placeholder::thumbhash;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

/// Rejections contain no file paths, decoder messages, or other process internals.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    /// File could not be read or written.
    #[error("image IO failed")]
    Io,
    /// Bytes do not identify a supported image.
    #[error("unrecognized image header")]
    BadMagic,
    /// Container, filename, or decoder disagrees with the header.
    #[error("inconsistent image container")]
    Mismatch,
    /// A configured byte, dimension, or pixel budget was exceeded.
    #[error("image exceeds limits")]
    Limits,
    /// Header or pixel stream is corrupt or incomplete.
    #[error("image decode failed")]
    Decode,
    /// A requested operation is unavailable or intentionally unsupported.
    #[error("unsupported image operation")]
    Unsupported,
    /// Options are outside their documented ranges.
    #[error("invalid image options")]
    Options,
    /// Encoder could not complete the output.
    #[error("image encode failed")]
    Encode,
}

/// Caps checked on headers before any pixel allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Total pixels, including all pages.
    pub max_pixels: u64,
    /// Input file size in bytes.
    pub max_bytes: u64,
    /// Maximum width or height.
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
impl Limits {
    pub(crate) fn check(&self, w: u32, h: u32, pages: u32) -> Result<(), CodecError> {
        let pixels = u64::from(w)
            .checked_mul(u64::from(h))
            .and_then(|n| n.checked_mul(u64::from(pages)))
            .ok_or(CodecError::Limits)?;
        if w == 0
            || h == 0
            || pages == 0
            || w > self.max_dim
            || h > self.max_dim
            || pixels > self.max_pixels
        {
            return Err(CodecError::Limits);
        }
        Ok(())
    }
}

/// Camera maker, bounded to 256 UTF-8 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawMake(pub String);
/// Recognized video container; identity never decodes video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoContainer {
    /// ISO base media file format.
    Mp4,
    /// `QuickTime` movie container.
    Mov,
}
/// Kind agreed between magic bytes and the selected native loader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// JPEG still image.
    Jpeg,
    /// PNG raster image.
    Png,
    /// TIFF container.
    Tiff,
    /// HEVC-coded HEIF image (heif feature).
    Heif,
    /// AV1-coded HEIF image.
    Avif,
    /// JPEG XL codestream or container.
    Jxl,
    /// WebP image.
    Webp,
    /// Camera RAW with a bounded maker.
    Raw(RawMake),
    /// Header-only video identity.
    Video {
        /// Recognized BMFF container.
        container: VideoContainer,
    },
    /// No supported decoder agreement.
    Unknown,
}
/// Header-only properties. Orientation is in the EXIF range 1..=8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Validated container kind.
    pub kind: Kind,
    /// Stored width before orientation, absent for video without a track header.
    pub width: Option<u32>,
    /// Stored height before orientation, absent for video without a track header.
    pub height: Option<u32>,
    /// EXIF transform, with missing orientation normalized to 1.
    pub orientation: u8,
    /// An embedded ICC profile exists.
    pub has_icc: bool,
    /// Bounded profile description.
    pub icc_desc: Option<String>,
    /// Bits per channel.
    pub bits: u8,
    /// Alpha is preserved on decode and resize.
    pub has_alpha: bool,
    /// Gain-map metadata was detected; HDR encoding remains unsupported.
    pub has_gain_map: bool,
    /// Number of pages or frames in the container.
    pub pages: u32,
}

/// Identify without evaluating the pixel pipeline; enforce limits before decode.
///
/// # Errors
/// Returns a typed rejection for invalid headers, exceeded limits, or failed native operations.
pub fn identify(path: &Path, limits: &Limits) -> Result<Identity, CodecError> {
    let kind = headers::inspect(path, limits)?;
    if matches!(kind, Kind::Video { .. }) {
        let dims = bmff::video_dimensions(
            std::fs::File::open(path).map_err(|_| CodecError::Io)?,
            limits,
        )?;
        return Ok(Identity {
            kind,
            width: dims.map(|d| d.0),
            height: dims.map(|d| d.1),
            orientation: 1,
            has_icc: false,
            icc_desc: None,
            bits: 8,
            has_alpha: false,
            has_gain_map: false,
            pages: 1,
        });
    }
    #[cfg(feature = "raw")]
    if raw::candidate(path, &kind) {
        return raw::identify(path, limits);
    }
    if kind == Kind::Jxl {
        // libvips' JXL header loader scans through the pixel payload to count
        // frames and collect trailing boxes. The bounded libjxl header already
        // supplies decoder agreement and all still-image identity fields.
        return native::identify_jxl(path, limits);
    }
    let has_gain_map = if matches!(kind, Kind::Heif | Kind::Avif) {
        false // The native loader's bounded BMFF preflight also checks gain maps.
    } else {
        headers::gain_map(path)?
    };
    let image = native::Image::load(path, &kind)?;
    let compression = image.string("heif-compression");
    if matches!(kind, Kind::Avif) && compression.as_deref() != Some("av1")
        || matches!(kind, Kind::Heif) && compression.as_deref() != Some("hevc")
    {
        return Err(CodecError::Mismatch);
    }
    let (width, height) = image.size()?;
    let pages = image.int("n-pages").unwrap_or(1);
    let pages = u32::try_from(pages).map_err(|_| CodecError::Limits)?;
    limits.check(width, height, pages)?;
    let profile = image.blob("icc-profile-data");
    let orientation = u8::try_from(image.orientation()).map_err(|_| CodecError::Decode)?;
    if !(1..=8).contains(&orientation) {
        return Err(CodecError::Decode);
    }
    Ok(Identity {
        kind,
        width: Some(width),
        height: Some(height),
        orientation,
        has_icc: profile.is_some() && !image.has("yhotos-nclx-profile"),
        icc_desc: profile.as_deref().and_then(native::profile_description),
        bits: image.bits(),
        has_alpha: image.has_alpha(),
        has_gain_map: has_gain_map
            || image.int("yhotos-gain-map") == Some(1)
            || image.has("gainmap-data")
            || image.has("gainmap"),
        pages,
    })
}

/// Materialized 16-bit linear XYZ (1/256 unit) with separate unassociated alpha.
/// Not `Send`/`Sync`: native image handles stay in the calling codec thread.
pub struct Working {
    image: native::Image,
    profile: Option<Arc<[u8]>>,
    alpha: bool,
}
impl Working {
    /// Dimensions after applying EXIF orientation.
    ///
    /// # Errors
    /// Returns Limits if native dimensions cannot be represented.
    pub fn dimensions(&self) -> Result<(u32, u32), CodecError> {
        self.image.size()
    }
}

/// Decode exactly once, apply orientation, and import the embedded ICC to XYZ.
///
/// # Errors
/// Returns a typed rejection for invalid headers, exceeded limits, or failed native operations.
pub fn decode(path: &Path, id: &Identity, limits: &Limits) -> Result<Working, CodecError> {
    if &identify(path, limits)? != id {
        return Err(CodecError::Mismatch);
    }
    #[cfg(feature = "raw")]
    if matches!(id.kind, Kind::Raw(_)) {
        return raw::decode(path, limits);
    }
    from_image(native::Image::load(path, &id.kind)?)
}

#[allow(clippy::needless_pass_by_value)] // Own the loaded image through materialization.
fn from_image(image: native::Image) -> Result<Working, CodecError> {
    let image = image.autorot()?;
    // RGB-only output codecs cannot carry CMYK/gray device values. Import
    // still uses that source profile; export uses an embedded sRGB profile.
    let profile = image
        .blob("icc-profile-data")
        .filter(|b| native::rgb_profile(b))
        .map(Arc::from);
    let alpha = image.has_alpha();
    let bands = image.bands();
    let color = if alpha {
        image.extract(0, bands - 1)?
    } else {
        image.copy()?
    };
    let xyz = color.import()?.linear(256.0)?.cast(native::USHORT)?;
    let image = if alpha {
        xyz.join(&image.alpha16()?)?
    } else {
        xyz
    };
    Ok(Working {
        image: image.materialize()?,
        profile,
        alpha,
    })
}

/// Target long edge, optionally center cropped to a square; never upscales.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeSpec {
    /// Positive target long edge.
    pub long_edge: u32,
    /// Crop before resampling.
    pub square: bool,
}
/// Resize from the original working image with Lanczos3 and premultiplied alpha.
///
/// # Errors
/// Returns a typed rejection for invalid headers, exceeded limits, or failed native operations.
pub fn resize(w: &Working, spec: ResizeSpec) -> Result<Working, CodecError> {
    if spec.long_edge == 0 || spec.long_edge > 65_535 {
        return Err(CodecError::Options);
    }
    let (width, height) = w.dimensions()?;
    let mut image = w.image.copy()?;
    if spec.square {
        let side = width.min(height);
        image = image.crop((width - side) / 2, (height - side) / 2, side, side)?;
    }
    let (width, height) = image.size()?;
    let scale = (f64::from(spec.long_edge) / f64::from(width.max(height))).min(1.0);
    if scale < 1.0 {
        if w.alpha {
            image = image.premultiply()?;
        }
        image = image.resize(scale)?;
        if w.alpha {
            image = image.unpremultiply()?;
        }
        image = image.cast(native::USHORT)?;
    }
    Ok(Working {
        image,
        profile: w.profile.clone(),
        alpha: w.alpha,
    })
}

/// Encoder options; HDR requests always return Unsupported.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Encode {
    /// AV1 still image, quality 1..=100, speed 0..=9.
    Avif {
        /// Encoder quality 1..=100.
        quality: u8,
        /// AV1 speed 0..=9; higher is faster.
        speed: u8,
        #[serde(default)]
        /// Reserved; true always returns Unsupported.
        hdr: bool,
    },
    /// JPEG XL, distance 0..=25, effort 1..=9.
    Jxl {
        /// Butteraugli distance 0..=25; zero is lossless.
        distance: f32,
        /// Encoder effort 1..=9.
        effort: u8,
        #[serde(default)]
        /// Reserved; true always returns Unsupported.
        hdr: bool,
    },
    /// JPEG fallback, quality 1..=100.
    Jpeg {
        /// Encoder quality 1..=100.
        quality: u8,
        /// Write progressive JPEG scans.
        progressive: bool,
        /// Allow chroma subsampling.
        chroma_subsampling: bool,
        #[serde(default)]
        /// Reserved; true always returns Unsupported.
        hdr: bool,
    },
}
/// Metadata policy; all derivatives strip EXIF, GPS, XMP, and IPTC.
#[derive(Debug, Clone, Copy)]
pub struct EncodeOpts {
    /// Preserve the source color profile, otherwise convert to sRGB.
    pub keep_icc: bool,
    /// Must be true; metadata-bearing derivatives are rejected.
    pub strip_metadata: bool,
}
/// Size and oriented dimensions of a completed output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Encoded {
    /// Exact file length.
    pub bytes: u64,
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
}

/// Encode with only ICC retained. Filename never selects the encoder.
///
/// # Errors
/// Returns a typed rejection for invalid headers, exceeded limits, or failed native operations.
pub fn encode(
    w: &Working,
    enc: Encode,
    opts: EncodeOpts,
    out: &Path,
) -> Result<Encoded, CodecError> {
    if !opts.strip_metadata {
        return Err(CodecError::Options);
    }
    let color = w.image.extract(0, 3)?.linear(1.0 / 256.0)?.xyz()?;
    let color = color.export(if opts.keep_icc {
        w.profile.as_deref()
    } else {
        None
    })?;
    let image = if w.alpha {
        color.join(&w.image.extract(3, 1)?)?
    } else {
        color
    };
    image.save(enc, out)?;
    let (width, height) = w.dimensions()?;
    Ok(Encoded {
        bytes: std::fs::metadata(out).map_err(|_| CodecError::Io)?.len(),
        width,
        height,
    })
}

fn rgba(w: &Working, edge: u32) -> Result<(u32, u32, Vec<u8>), CodecError> {
    let small = resize(
        w,
        ResizeSpec {
            long_edge: edge,
            square: false,
        },
    )?;
    let color = small
        .image
        .extract(0, 3)?
        .linear(1.0 / 256.0)?
        .xyz()?
        .export(None)?
        .linear(1.0 / 257.0)?
        .cast(native::UCHAR)?;
    let image = if small.alpha {
        color.join(
            &small
                .image
                .extract(3, 1)?
                .linear(1.0 / 257.0)?
                .cast(native::UCHAR)?,
        )?
    } else {
        color.join_constant(255.0)?
    };
    let (width, height) = image.size()?;
    Ok((width, height, image.pixels()?))
}

#[cfg(test)]
mod tests;
