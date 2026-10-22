//! Gain-map pixels never enter the ICC pipeline. Metadata is normalized to ISO log2 units.
#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // Fixed three-channel metadata and checked native descriptors.
use crate::{
    CodecError, Identity, Kind, Limits,
    native::{self, ffi},
};
use std::{fs::File, io::Read, path::Path, ptr::NonNull};

/// Fixed, path-free reasons for retaining only the SDR rendition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrSkippedReason {
    /// The source has no gain map, so only SDR outputs were requested.
    NoGainMap,
    /// The auxiliary metadata is missing, inconsistent, or outside supported ranges.
    InvalidGainMap,
    /// This gain-map representation cannot be interpreted by the installed codecs.
    UnsupportedGainMap,
    /// A gain-map input or allocation exceeds the codec's bounds.
    GainMapLimits,
    /// HDR compression was unavailable or could not complete.
    EncoderUnavailable,
}
impl From<CodecError> for HdrSkippedReason {
    fn from(error: CodecError) -> Self {
        match error {
            CodecError::Unsupported => Self::UnsupportedGainMap,
            CodecError::Limits => Self::GainMapLimits,
            _ => Self::InvalidGainMap,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Metadata {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub gamma: [f32; 3],
    pub base_offset: [f32; 3],
    pub alternate_offset: [f32; 3],
    pub base_headroom: f32,
    pub alternate_headroom: f32,
    pub use_base: bool,
}
impl Metadata {
    pub(crate) fn validate(&self) -> Result<(), CodecError> {
        // Finite, bounded coefficients keep native log/pow operations and HDR pixel values finite.
        if !self.base_headroom.is_finite()
            || !self.alternate_headroom.is_finite()
            || !(0.0..=16.0).contains(&self.base_headroom)
            || !(self.base_headroom..=16.0).contains(&self.alternate_headroom)
            || self.base_headroom >= self.alternate_headroom
        {
            return Err(CodecError::Unsupported);
        }
        for c in 0..3 {
            if !self.min[c].is_finite()
                || !self.max[c].is_finite()
                || !(-16.0..=16.0).contains(&self.min[c])
                || !(self.min[c]..=16.0).contains(&self.max[c])
                || !self.gamma[c].is_finite()
                || !(0.01..=100.0).contains(&self.gamma[c])
                || !self.base_offset[c].is_finite()
                || !(0.0..=1.0).contains(&self.base_offset[c])
                || !self.alternate_offset[c].is_finite()
                || !(0.0..=1.0).contains(&self.alternate_offset[c])
            {
                return Err(CodecError::Decode);
            }
        }
        Ok(())
    }
    fn native(&self) -> ffi::uhdr_gainmap_metadata_t {
        ffi::uhdr_gainmap_metadata_t {
            min_content_boost: self.min.map(f32::exp2),
            max_content_boost: self.max.map(f32::exp2),
            gamma: self.gamma,
            offset_sdr: self.base_offset,
            offset_hdr: self.alternate_offset,
            hdr_capacity_min: self.base_headroom.exp2(),
            hdr_capacity_max: self.alternate_headroom.exp2(),
            use_base_cg: i32::from(self.use_base),
        }
    }
}

pub(crate) struct GainMap {
    pub image: native::Image,
    pub metadata: Metadata,
}
impl GainMap {
    pub(crate) fn resized(&self, spec: crate::ResizeSpec, scale: f64) -> Result<Self, CodecError> {
        let (w, h) = self.image.size()?;
        let image = if spec.square {
            let side = w.min(h);
            self.image
                .crop((w - side) / 2, (h - side) / 2, side, side)?
        } else {
            self.image.copy()?
        };
        let image = if scale < 1.0 {
            image.resize(scale)?.cast(native::UCHAR)?
        } else {
            image
        };
        Ok(Self {
            image,
            metadata: self.metadata.clone(),
        })
    }
}

const MAX_COMPRESSED: u64 = 256 << 20;
pub(crate) fn read(path: &Path, limits: &Limits) -> Result<Vec<u8>, CodecError> {
    let mut file = File::open(path).map_err(|_| CodecError::Io)?;
    let size = file.metadata().map_err(|_| CodecError::Io)?.len();
    if size > limits.max_bytes.min(MAX_COMPRESSED) {
        return Err(CodecError::Limits);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(size + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CodecError::Io)?;
    if bytes.len() as u64 != size {
        return Err(CodecError::Decode);
    }
    Ok(bytes)
}

struct Decoder(NonNull<ffi::uhdr_codec_private_t>);
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: this RAII owner releases its unique live decoder exactly once.
        unsafe {
            ffi::uhdr_release_decoder(self.0.as_ptr());
        }
    }
}
#[allow(clippy::large_types_passed_by_value)] // The C API returns this fixed descriptor by value.
fn status(info: ffi::uhdr_error_info_t) -> Result<(), CodecError> {
    if info.error_code == ffi::uhdr_codec_err_UHDR_CODEC_OK {
        Ok(())
    } else {
        Err(CodecError::Decode)
    }
}
fn compressed(bytes: &[u8]) -> ffi::uhdr_compressed_image_t {
    ffi::uhdr_compressed_image_t {
        data: bytes.as_ptr().cast_mut().cast(),
        data_sz: bytes.len(),
        capacity: bytes.len(),
        cg: ffi::uhdr_color_gamut_UHDR_CG_UNSPECIFIED,
        ct: ffi::uhdr_color_transfer_UHDR_CT_UNSPECIFIED,
        range: ffi::uhdr_color_range_UHDR_CR_UNSPECIFIED,
    }
}
impl Decoder {
    fn probe(bytes: &[u8], limits: &Limits) -> Result<Self, CodecError> {
        // SAFETY: no inputs to constructor; set_image copies the exact live slice. Probe parses
        // only headers. Dimensions of both renditions are checked before requesting any pixels.
        unsafe {
            let decoder = Self(NonNull::new(ffi::uhdr_create_decoder()).ok_or(CodecError::Decode)?);
            status(ffi::uhdr_dec_set_image(
                decoder.0.as_ptr(),
                &mut compressed(bytes),
            ))?;
            status(ffi::uhdr_dec_probe(decoder.0.as_ptr()))?;
            for (w, h) in [
                (
                    ffi::uhdr_dec_get_image_width(decoder.0.as_ptr()),
                    ffi::uhdr_dec_get_image_height(decoder.0.as_ptr()),
                ),
                (
                    ffi::uhdr_dec_get_gainmap_width(decoder.0.as_ptr()),
                    ffi::uhdr_dec_get_gainmap_height(decoder.0.as_ptr()),
                ),
            ] {
                limits.check(
                    u32::try_from(w).map_err(|_| CodecError::Limits)?,
                    u32::try_from(h).map_err(|_| CodecError::Limits)?,
                    1,
                )?;
            }
            Ok(decoder)
        }
    }
    fn metadata(&self) -> Result<Metadata, CodecError> {
        // SAFETY: probe succeeded; metadata is decoder-owned and copied before it is released.
        let metadata = unsafe { ffi::uhdr_dec_get_gainmap_metadata(self.0.as_ptr()).as_ref() }
            .ok_or(CodecError::Decode)?;
        let metadata = Metadata {
            min: metadata.min_content_boost.map(f32::log2),
            max: metadata.max_content_boost.map(f32::log2),
            gamma: metadata.gamma,
            base_offset: metadata.offset_sdr,
            alternate_offset: metadata.offset_hdr,
            base_headroom: metadata.hdr_capacity_min.log2(),
            alternate_headroom: metadata.hdr_capacity_max.log2(),
            use_base: metadata.use_base_cg == 1,
        };
        metadata.validate()?;
        Ok(metadata)
    }
    fn gain(&self) -> Result<&[u8], CodecError> {
        // SAFETY: probe succeeded; descriptor and buffer belong to this live decoder. Its API
        // guarantees capacity bytes; cap length before forming a slice. Borrow cannot escape self.
        unsafe { block(ffi::uhdr_dec_get_gainmap_image(self.0.as_ptr())) }
    }
}
unsafe fn block<'a>(p: *const ffi::uhdr_mem_block_t) -> Result<&'a [u8], CodecError> {
    // SAFETY: callers keep the owning native encoder/decoder alive for the returned borrow.
    let b = unsafe { p.as_ref() }.ok_or(CodecError::Decode)?;
    if b.data.is_null()
        || b.data_sz == 0
        || b.data_sz > b.capacity
        || b.data_sz as u64 > MAX_COMPRESSED
    {
        return Err(CodecError::Limits);
    }
    // SAFETY: native descriptor guarantees capacity, above checks bound the non-null slice.
    Ok(unsafe { std::slice::from_raw_parts(b.data.cast(), b.data_sz) })
}

pub(crate) fn extract(path: &Path, id: &Identity, limits: &Limits) -> Result<GainMap, CodecError> {
    let bytes = read(path, limits)?;
    let (image, metadata) = match id.kind {
        Kind::Jpeg => {
            let gain = crate::hdr_jpeg::extract(&bytes)?;
            (
                native::Image::gain_jpeg(gain.image, limits, id.orientation)?,
                gain.metadata,
            )
        }
        Kind::Heif | Kind::Avif => {
            let gain = crate::gain_container::heif(&bytes, limits)?;
            let mut image =
                native::Image::gain_heif(path, gain.id, limits)?.oriented(id.orientation)?;
            if gain.apple {
                let headroom = gain.metadata.alternate_headroom.exp2();
                let pixels = image
                    .pixels()?
                    .into_iter()
                    .map(|v| {
                        let encoded = f32::from(v) / 255.0;
                        let linear = if encoded < 0.081 {
                            encoded / 4.5
                        } else {
                            ((encoded + 0.099) / 1.099).powf(1.0 / 0.45)
                        };
                        ((1.0 + (headroom - 1.0) * linear).log2()
                            / gain.metadata.alternate_headroom
                            * 255.0)
                            .round()
                            .clamp(0.0, 255.0) as u8
                    })
                    .collect::<Vec<_>>();
                let (w, h) = image.size()?;
                image = native::Image::samples(&pixels, w, h, 3)?;
            }
            (image, gain.metadata)
        }
        Kind::Jxl => {
            let gain = crate::gain_container::jxl(&bytes, limits)?;
            (gain.image, gain.metadata)
        }
        _ => return Err(CodecError::Unsupported),
    };
    if !metadata.use_base {
        return Err(CodecError::Unsupported);
    }
    let (w, h) = image.size()?;
    let (bw, bh) = id.width.zip(id.height).ok_or(CodecError::Decode)?;
    let (bw, bh) = if id.orientation >= 5 {
        (bh, bw)
    } else {
        (bw, bh)
    };
    if w > bw
        || h > bh
        || (f64::from(w) / f64::from(h) - f64::from(bw) / f64::from(bh)).abs() > 0.02
    {
        return Err(CodecError::Decode);
    }
    Ok(GainMap { image, metadata })
}

struct Encoder(NonNull<ffi::uhdr_codec_private_t>);
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: unique ownership from create_encoder, released exactly once.
        unsafe {
            ffi::uhdr_release_encoder(self.0.as_ptr());
        }
    }
}
pub(crate) fn mux(base: &[u8], gain: &[u8], metadata: &Metadata) -> Result<Vec<u8>, CodecError> {
    metadata.validate()?;
    if base.len() as u64 > MAX_COMPRESSED || gain.len() as u64 > MAX_COMPRESSED {
        return Err(CodecError::Limits);
    }
    // SAFETY: live compressed slices and metadata descriptors outlive encode; set APIs copy
    // their inputs. The encoder owns the returned stream until copied below, then RAII frees it.
    unsafe {
        let encoder = Encoder(NonNull::new(ffi::uhdr_create_encoder()).ok_or(CodecError::Encode)?);
        status(ffi::uhdr_enc_set_compressed_image(
            encoder.0.as_ptr(),
            &mut compressed(base),
            ffi::uhdr_img_label_UHDR_BASE_IMG,
        ))?;
        status(ffi::uhdr_enc_set_gainmap_image(
            encoder.0.as_ptr(),
            &mut compressed(gain),
            &mut metadata.native(),
        ))?;
        status(ffi::uhdr_encode(encoder.0.as_ptr()))?;
        let stream = ffi::uhdr_get_encoded_stream(encoder.0.as_ptr())
            .as_ref()
            .ok_or(CodecError::Encode)?;
        if stream.data.is_null()
            || stream.data_sz > stream.capacity
            || stream.data_sz as u64 > MAX_COMPRESSED
        {
            return Err(CodecError::Limits);
        }
        crate::hdr_jpeg::dual(
            std::slice::from_raw_parts(stream.data.cast(), stream.data_sz),
            metadata,
        )
    }
}

/// Probe bounded gain-map metadata and auxiliary pixels inside a sandbox or fuzz harness.
/// # Errors
/// Returns a fixed codec error for unsupported or malformed gain maps and exceeded limits.
pub fn validate_gain_map(path: &Path, limits: &Limits) -> Result<(), CodecError> {
    let id = crate::identify(path, limits)?;
    extract(path, &id, limits).map(|_| ())
}

/// Exercise bounded ISO, container metadata, and reference JPEG mux boundaries without file IO.
/// Intended for a sandboxed parser harness. No pointers or native diagnostics escape.
/// # Errors
/// Rejects inputs above 1 MiB; individual malformed representations are ignored by the harness.
pub fn validate_gain_map_metadata(bytes: &[u8]) -> Result<(), CodecError> {
    if bytes.len() > 1 << 20 {
        return Err(CodecError::Limits);
    }
    let limits = Limits {
        max_bytes: 1 << 20,
        max_pixels: 1_048_576,
        max_dim: 1024,
    };
    let _ = crate::gain_container::iso(bytes);
    if bytes.get(4..8) == Some(b"ftyp") {
        let _ = crate::gain_container::heif(bytes, &limits);
    }
    if bytes.starts_with(b"\xff\xd8")
        && crate::hdr_jpeg::extract(bytes).is_ok()
        && let Ok(decoder) = Decoder::probe(bytes, &limits)
        && let Ok(metadata) = decoder.metadata()
    {
        // SAFETY: live probed decoder owns both compressed buffers for the duration of mux.
        // block bounds the native lengths before making slices; mux copies its result.
        unsafe {
            if let (Ok(base), Ok(gain)) = (
                block(ffi::uhdr_dec_get_base_image(decoder.0.as_ptr())),
                decoder.gain(),
            ) {
                let _ = mux(base, gain, &metadata);
            }
        }
    }
    if bytes.starts_with(b"\0\0\0\x0cJXL ") {
        let _ = crate::gain_container::jxl(bytes, &limits);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/hdr")
            .join(name)
    }
    fn half(bits: u16) -> f32 {
        let exponent = i32::from((bits >> 10) & 31);
        let fraction = f32::from(bits & 1023);
        let value = if exponent == 0 {
            fraction * 2.0_f32.powi(-24)
        } else {
            (1024.0 + fraction) * 2.0_f32.powi(exponent - 25)
        };
        if bits & 32768 != 0 { -value } else { value }
    }
    fn reference_luma(bytes: &[u8]) -> Vec<f32> {
        let _bounds = Decoder::probe(bytes, &Limits::default()).unwrap();
        // SAFETY: probe checks both geometries before decode; descriptor/plane belong to the
        // live decoder. Requested packed half RGBA contains stride*h*8 bytes. Every sample
        // coordinate is inside w/h and all pixels are read before releasing the decoder.
        unsafe {
            let decoder = Decoder(NonNull::new(ffi::uhdr_create_decoder()).unwrap());
            let p = decoder.0.as_ptr();
            status(ffi::uhdr_dec_set_image(p, &mut compressed(bytes))).unwrap();
            status(ffi::uhdr_dec_set_out_img_format(
                p,
                ffi::uhdr_img_fmt_UHDR_IMG_FMT_64bppRGBAHalfFloat,
            ))
            .expect("reference format");
            status(ffi::uhdr_dec_set_out_color_transfer(
                p,
                ffi::uhdr_color_transfer_UHDR_CT_LINEAR,
            ))
            .expect("reference transfer");
            status(ffi::uhdr_dec_probe(p)).unwrap();
            status(ffi::uhdr_decode(p)).expect("reference decode");
            let image = ffi::uhdr_get_decoded_image(p).as_ref().unwrap();
            assert_eq!(image.fmt, ffi::uhdr_img_fmt_UHDR_IMG_FMT_64bppRGBAHalfFloat);
            assert!(image.stride[0] >= image.w && !image.planes[0].is_null());
            let bytes = std::slice::from_raw_parts(
                image.planes[0].cast::<u8>(),
                image.stride[0] as usize * image.h as usize * 8,
            );
            [(1, 1), (5, 3), (11, 7)]
                .map(|(x, y)| {
                    let x = image.w as usize * x / 12;
                    let y = image.h as usize * y / 8;
                    let at = (y * image.stride[0] as usize + x) * 8;
                    let rgb = [0, 2, 4].map(|channel| {
                        half(u16::from_le_bytes([
                            bytes[at + channel],
                            bytes[at + channel + 1],
                        ]))
                    });
                    let weights = if image.cg == ffi::uhdr_color_gamut_UHDR_CG_DISPLAY_P3 {
                        [0.228_975, 0.691_739, 0.079_287]
                    } else {
                        [0.2126, 0.7152, 0.0722]
                    };
                    rgb[0] * weights[0] + rgb[1] * weights[1] + rgb[2] * weights[2]
                })
                .to_vec()
        }
    }
    fn linear(value: u8) -> f32 {
        let v = f32::from(value) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    #[test]
    fn sixteen_bit_jxl_gain_samples_normalize_without_color_conversion() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/images/16bit.png");
        let gain = native::Image::load(&path, &Kind::Png).unwrap();
        let original = gain
            .pixels()
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                u8::try_from((u32::from(u16::from_ne_bytes([p[0], p[1]])) + 128) / 257).unwrap()
            })
            .collect::<Vec<_>>();
        let out = tempfile::NamedTempFile::new().unwrap();
        gain.save(
            crate::Encode::Jxl {
                distance: 0.0,
                effort: 3,
                hdr: false,
            },
            out.path(),
        )
        .unwrap();
        let decoded =
            native::Image::gain_jxl(&std::fs::read(out.path()).unwrap(), &Limits::default())
                .unwrap();
        assert_eq!(decoded.pixels().unwrap(), original);
    }
    #[test]
    fn cached_sdr_bases_are_bounded_and_bound_to_the_working_image_and_policy() {
        let path = fixture("iso.jpg");
        let id = crate::identify(&path, &Limits::default()).unwrap();
        let working = crate::decode(&path, &id, &Limits::default()).unwrap();
        let other = crate::decode(&path, &id, &Limits::default()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sdr = dir.path().join("sdr.jpg");
        let hdr = dir.path().join("hdr.jpg");
        let enc = crate::Encode::Jpeg {
            quality: 99,
            progressive: true,
            chroma_subsampling: false,
            hdr: true,
        };
        let opts = crate::EncodeOpts {
            keep_icc: true,
            strip_metadata: true,
        };
        let (_, none) = crate::encode_sdr_cached(&working, enc.sdr(), opts, &sdr, 0).unwrap();
        assert!(none.is_none());
        let (_, base) =
            crate::encode_sdr_cached(&working, enc.sdr(), opts, &sdr, 16 << 20).unwrap();
        let base = base.unwrap();
        assert!(base.matches(&working, enc, opts));
        assert!(!base.matches(&other, enc, opts));
        assert!(!base.matches(
            &working,
            enc,
            crate::EncodeOpts {
                keep_icc: false,
                ..opts
            }
        ));
        assert!(!base.matches(
            &working,
            crate::Encode::Jpeg {
                quality: 90,
                progressive: true,
                chroma_subsampling: false,
                hdr: true
            },
            opts
        ));
        assert!(crate::encode_reusing_sdr(&other, enc, opts, &hdr, &base).is_err());
        // The retained base is an immutable byte snapshot, independent of subsequent file IO.
        std::fs::write(&sdr, b"replaced after encoding").unwrap();
        crate::encode_reusing_sdr(&working, enc, opts, &hdr, &base).unwrap();
        assert!(
            crate::identify(&hdr, &Limits::default())
                .unwrap()
                .has_gain_map
        );
        assert_eq!(
            native::Image::load(&hdr, &Kind::Jpeg)
                .unwrap()
                .pixels()
                .unwrap(),
            native::Image::gain_jpeg(&base.bytes, &Limits::default(), 1)
                .unwrap()
                .pixels()
                .unwrap()
        );
    }
    #[test]
    fn jhgm_round_trip_keeps_sdr_pixels_gain_geometry_and_hdr_luminance() {
        let source = fixture("iso.jpg");
        let id = crate::identify(&source, &Limits::default()).unwrap();
        let working = crate::decode(&source, &id, &Limits::default()).unwrap();
        let working = crate::resize(
            &working,
            crate::ResizeSpec {
                long_edge: 48,
                square: false,
            },
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sdr = dir.path().join("sdr.jxl");
        let hdr = dir.path().join("hdr.jxl");
        let jpeg = dir.path().join("hdr.jpg");
        let opts = crate::EncodeOpts {
            keep_icc: true,
            strip_metadata: true,
        };
        for (flag, path) in [(false, &sdr), (true, &hdr)] {
            crate::encode(
                &working,
                crate::Encode::Jxl {
                    distance: 0.0,
                    effort: 3,
                    hdr: flag,
                },
                opts,
                path,
            )
            .unwrap();
        }
        assert_eq!(
            native::Image::load(&sdr, &Kind::Jxl)
                .unwrap()
                .pixels()
                .unwrap(),
            native::Image::load(&hdr, &Kind::Jxl)
                .unwrap()
                .pixels()
                .unwrap()
        );
        let id = crate::identify(&hdr, &Limits::default()).unwrap();
        assert!(id.has_gain_map);
        let decoded = crate::decode(&hdr, &id, &Limits::default()).unwrap();
        assert!(decoded.has_gain_map(), "{:?}", decoded.hdr_skipped_reason());
        assert_eq!(
            decoded.gain_map.as_ref().unwrap().image.size().unwrap(),
            (24, 16)
        );
        crate::encode(
            &decoded,
            crate::Encode::Jpeg {
                quality: 99,
                progressive: true,
                chroma_subsampling: false,
                hdr: true,
            },
            opts,
            &jpeg,
        )
        .unwrap();
        for (actual, expected) in reference_luma(&std::fs::read(jpeg).unwrap())
            .iter()
            .zip(reference_luma(&std::fs::read(source).unwrap()))
        {
            assert!((actual - expected).abs() <= 0.01 + 0.04 * expected);
        }
    }
    #[test]
    fn all_containers_match_reference_hdr_luminance_and_sdr_color_at_planned_sizes() {
        for name in [
            "iso.jpg",
            "iso-p3.jpg",
            "adobe.jpg",
            "apple.heic",
            "gain-map.jxl",
            "../images/gain-map.avif",
        ] {
            let path = fixture(name);
            let id = crate::identify(&path, &Limits::default()).unwrap();
            let working = crate::decode(&path, &id, &Limits::default()).unwrap();
            assert!(
                working.has_gain_map(),
                "{name}: {:?}",
                working.hdr_skipped_reason()
            );
            let expected = if id.kind == Kind::Jpeg {
                reference_luma(&std::fs::read(&path).unwrap())
            } else {
                let base = native::Image::load(&path, &id.kind).unwrap();
                let pixels = base.pixels().unwrap();
                let (w, h) = base.size().unwrap();
                let gain = if id.kind == Kind::Heif {
                    // Apple documentation: maker33=1, maker48=0 -> 3 stops;
                    // inverse Rec.709 gain sample, then linear 1+(8-1)*gain.
                    1.0 + 7.0 * ((128.0_f32 / 255.0 + 0.099) / 1.099).powf(1.0 / 0.45)
                } else {
                    2.0_f32.powf(2.0 * 128.0 / 255.0)
                };
                [(1, 1), (5, 3), (11, 7)]
                    .map(|(x, y)| {
                        let at = ((h as usize * y / 8) * w as usize + w as usize * x / 12) * 3;
                        gain * (linear(pixels[at]) * 0.2126
                            + linear(pixels[at + 1]) * 0.7152
                            + linear(pixels[at + 2]) * 0.0722)
                    })
                    .to_vec()
            };
            // Small fixtures exercise each planned size without upscaling; 48 also resizes maps.
            for edge in [48, 1280, 2048, 3200] {
                let resized = crate::resize(
                    &working,
                    crate::ResizeSpec {
                        long_edge: edge,
                        square: false,
                    },
                )
                .unwrap();
                let dir = tempfile::tempdir().unwrap();
                let sdr = dir.path().join("sdr.jpg");
                let hdr = dir.path().join("hdr.jpg");
                for (flag, path) in [(false, &sdr), (true, &hdr)] {
                    crate::encode(
                        &resized,
                        crate::Encode::Jpeg {
                            quality: 99,
                            progressive: true,
                            chroma_subsampling: false,
                            hdr: flag,
                        },
                        crate::EncodeOpts {
                            keep_icc: true,
                            strip_metadata: true,
                        },
                        path,
                    )
                    .unwrap();
                }
                let base = native::Image::load(&sdr, &Kind::Jpeg).unwrap();
                let fallback = native::Image::load(&hdr, &Kind::Jpeg).unwrap();
                assert_eq!(
                    base.pixels().unwrap(),
                    fallback.pixels().unwrap(),
                    "{name}/{edge}"
                );
                assert_eq!(
                    base.blob("icc-profile-data"),
                    fallback.blob("icc-profile-data")
                );
                for (actual, expected) in reference_luma(&std::fs::read(&hdr).unwrap())
                    .iter()
                    .zip(&expected)
                {
                    assert!(
                        (actual - expected).abs() <= 0.01 + 0.04 * expected,
                        "{name}/{edge}: {actual} vs {expected}"
                    );
                }
            }
        }
    }
    #[test]
    fn jpeg_gain_map_is_resized_and_sdr_pixels_are_identical() {
        let path = fixture("iso.jpg");
        let id = crate::identify(&path, &Limits::default()).unwrap();
        assert!(id.has_gain_map);
        let working = crate::decode(&path, &id, &Limits::default()).unwrap();
        assert!(working.has_gain_map(), "{:?}", working.hdr_skipped_reason());
        let resized = crate::resize(
            &working,
            crate::ResizeSpec {
                long_edge: 48,
                square: false,
            },
        )
        .unwrap();
        assert_eq!(
            resized.gain_map.as_ref().unwrap().image.size().unwrap(),
            (24, 16)
        );
        let dir = tempfile::tempdir().unwrap();
        let sdr = dir.path().join("sdr.jpg");
        let hdr = dir.path().join("hdr.jpg");
        let opts = crate::EncodeOpts {
            keep_icc: true,
            strip_metadata: true,
        };
        for (hdr_flag, path) in [(false, &sdr), (true, &hdr)] {
            crate::encode(
                &resized,
                crate::Encode::Jpeg {
                    quality: 99,
                    progressive: true,
                    chroma_subsampling: false,
                    hdr: hdr_flag,
                },
                opts,
                path,
            )
            .unwrap();
        }
        assert_eq!(
            native::Image::load(&sdr, &Kind::Jpeg)
                .unwrap()
                .pixels()
                .unwrap(),
            native::Image::load(&hdr, &Kind::Jpeg)
                .unwrap()
                .pixels()
                .unwrap()
        );
        let bytes = std::fs::read(&hdr).unwrap();
        let decoder = Decoder::probe(&bytes, &Limits::default()).unwrap();
        assert!((decoder.metadata().unwrap().alternate_headroom - 2.0).abs() < 0.001);
        // SAFETY: live probed decoder; its two image geometries have passed Limits.
        unsafe {
            status(ffi::uhdr_decode(decoder.0.as_ptr())).unwrap();
        }
    }
    #[test]
    fn avif_tmap_is_extracted() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/images/gain-map.avif");
        let id = crate::identify(&path, &Limits::default()).unwrap();
        let working = crate::decode(&path, &id, &Limits::default()).unwrap();
        assert!(working.has_gain_map(), "{:?}", working.hdr_skipped_reason());
    }

    #[test]
    fn apple_and_adobe_inputs_extract_and_encode() {
        for name in ["apple.heic", "adobe.jpg"] {
            let path = fixture(name);
            let id = crate::identify(&path, &Limits::default()).unwrap();
            assert!(id.has_gain_map, "{name}");
            let working = crate::decode(&path, &id, &Limits::default()).unwrap();
            assert!(
                working.has_gain_map(),
                "{name}: {:?}",
                working.hdr_skipped_reason()
            );
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("hdr.jpg");
            crate::encode(
                &working,
                crate::Encode::Jpeg {
                    quality: 99,
                    progressive: true,
                    chroma_subsampling: false,
                    hdr: true,
                },
                crate::EncodeOpts {
                    keep_icc: true,
                    strip_metadata: true,
                },
                &output,
            )
            .unwrap();
            let bytes = std::fs::read(output).unwrap();
            assert!(bytes.windows(6).any(|b| b == b"hdrgm:"));
            assert!(
                bytes
                    .windows(b"urn:iso:std:iso:ts:21496:-1".len())
                    .any(|b| b == b"urn:iso:std:iso:ts:21496:-1")
            );
            let decoder = Decoder::probe(&bytes, &Limits::default()).unwrap();
            // SAFETY: live decoder, probe succeeded and dimensions are bounded.
            unsafe {
                status(ffi::uhdr_decode(decoder.0.as_ptr())).unwrap();
            }
        }
    }
}
