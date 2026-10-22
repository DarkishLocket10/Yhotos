//! Bounded RAW preview decode; rawler demosaic excludes CR3 entirely.
//!
//! Pre-rawler input audit (rawler 0.8.0, identify/decode/metadata share `probe`):
//! - Every TIFF make: IFD/entry/payload budgets, dimensions, sample-expanded
//!   width, tile geometry, JPEG strip/tile SOFs (including component width),
//!   and each embedded preview's own JPEG dimensions.
//! - Canon CR2/old TIFF: Cr2Id/CFAPattern strip offsets regardless of Compression;
//!   `MakerNote` IFD entries, `Cr2OldOffset` (0x0081), slice offsets/lengths
//!   (0x00c5/0x00c6), and each referenced sensor SOF before rawler selection.
//! - Canon CR3: never passed to rawler; only bounded BMFF JPEG previews decode.
//! - Panasonic RW2: sensor dimensions 0x0002/0x0003 and JPEG blob 0x002e.
//! - Sony ARW: common TIFF/sensor checks plus ARW1's eight extra coded rows.
//! - DNG (any make): common TIFF/SubIFD checks, lossless JPEG and JPEG XL
//!   (Compression=52546) frame dimensions before sensor decoding.
//! - Other TIFF-based makes use the common checks, with no make-specific probe;
//!   other proprietary containers return Unsupported, never a dummy decode.
use crate::{CodecError, Identity, Kind, Limits, Metadata, RawMake, Working, native};
use rawler::{decoders::RawDecodeParams, rawsource::RawSource};
use std::path::Path;
pub(crate) fn candidate(path: &Path, kind: &Kind) -> bool {
    matches!(kind, Kind::Raw(_))
        || path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
            rawler::decoders::supported_extensions()
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(s))
        })
}
// rawler 0.8's decode_dummy and preview APIs can decode pixels. Never call
// them to probe headers. Keep encoded JPEGs so their ICC survives import.
struct Probe {
    width: u32,
    height: u32,
    orientation: u8,
    make: String,
    preview: Option<Vec<u8>>,
    cr3: bool,
}
impl Probe {
    fn preview(&mut self, bytes: Vec<u8>, limits: &Limits) -> Result<(), CodecError> {
        let (w, h) = crate::headers::jpeg_dimensions(&bytes, limits)?;
        let old = self
            .preview
            .as_deref()
            .map(|b| crate::headers::jpeg_dimensions(b, limits))
            .transpose()?
            .map_or(0, |(w, h)| u64::from(w) * u64::from(h));
        if u64::from(w) * u64::from(h) > old {
            self.preview = Some(bytes);
        }
        Ok(())
    }
}
fn probe(path: &Path, limits: &Limits) -> Result<Probe, CodecError> {
    let mut tiff = match crate::tiff::Tiff::open(path, limits) {
        Ok(tiff) => tiff,
        Err(CodecError::Unsupported) => return probe_cr3(path, limits),
        Err(e) => return Err(e),
    };
    let mut probe = Probe {
        width: 0,
        height: 0,
        orientation: 1,
        make: String::new(),
        preview: None,
        cr3: false,
    };
    let mut sensor_budget = 16u64 << 20;
    let mut sensor_count = 0usize;
    for i in 0..tiff.count() {
        if probe.make.is_empty()
            && let Some(bytes) = tiff.blob(i, 271, 256)?
        {
            probe.make =
                crate::metadata::cap(String::from_utf8_lossy(&bytes).trim_end_matches('\0'));
        }
        if i == 0 {
            probe.orientation = tiff
                .number(i, 274)?
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| (1..=8).contains(n))
                .unwrap_or(1);
        }
        let reduced = tiff.number(i, 254)?.unwrap_or(0) & 1 != 0;
        let standard = tiff.dimensions(i)?;
        let sensor = if tiff.flavor == crate::tiff::Flavor::Panasonic {
            tiff.number(i, 2)?.zip(tiff.number(i, 3)?)
        } else {
            None
        };
        let dims = if let Some((w, h)) = sensor {
            // Validate the sensor even when this IFD also describes a small JPEG.
            Some((
                u32::try_from(w).map_err(|_| CodecError::Limits)?,
                u32::try_from(h).map_err(|_| CodecError::Limits)?,
            ))
        } else {
            standard
        };
        if let Some((w, h)) = dims {
            limits.check(w, h, 1)?;
            check_samples(&mut tiff, i, w, h, reduced, limits)?;
            if !reduced
                && u64::from(w) * u64::from(h) > u64::from(probe.width) * u64::from(probe.height)
            {
                probe.width = w;
                probe.height = h;
            }
        }
        if let Some((offset, len)) = tiff.number(i, 513)?.zip(tiff.number(i, 514)?) {
            probe.preview(tiff.data(offset, len, limits)?, limits)?;
        }
        let compression = tiff.number(i, 259)?.unwrap_or(1);
        let color = tiff.number(i, 262)?.is_some_and(|n| n == 2 || n == 6);
        // Sensor strips/tiles are independent codestreams. In particular CR2
        // allocates from SOF width * component count, not the TIFF dimensions.
        // DNG and recent Sony ARW use the same lossless JPEG allocation path.
        if compression == 32767
            && let Some((w, h)) = dims
        {
            // Sony ARW1 adds eight coded rows before returning the raw image.
            limits.check(w, h.checked_add(8).ok_or(CodecError::Limits)?, 1)?;
        }
        // CR2 ignores the TIFF Compression value and locates the sensor by
        // Cr2Id or CFAPattern. A forged Compression=1 must not bypass SOF caps.
        let sensor_compression = if tiff.has(i, 0xc5d8)
            || ((tiff.flavor == crate::tiff::Flavor::Canon || probe.make == "Canon")
                && !tiff.has(0, 50706)
                && tiff.has(i, 33422))
        {
            7
        } else {
            compression
        };
        check_sensor(
            &mut tiff,
            i,
            sensor_compression,
            limits,
            &mut sensor_budget,
            &mut sensor_count,
        )?;
        if matches!(compression, 6 | 7) && (reduced || color) {
            let offsets = tiff.numbers(i, 273, 64)?;
            let lengths = tiff.numbers(i, 279, 64)?;
            if let ([offset], [len]) = (offsets.as_slice(), lengths.as_slice()) {
                probe.preview(tiff.data(*offset, *len, limits)?, limits)?;
            }
        }
        // RW2 stores its JPEG directly in tag 46.
        if let Some(bytes) = tiff.blob(i, 46, limits.max_bytes)? {
            probe.preview(bytes, limits)?;
        }
    }
    if (tiff.flavor == crate::tiff::Flavor::Canon || probe.make == "Canon") && !tiff.has(0, 50706) {
        check_canon_makernotes(&mut tiff, limits, &mut sensor_budget, &mut sensor_count)?;
    }
    limits.check(probe.width, probe.height, 1)?;
    Ok(probe)
}
fn check_samples(
    tiff: &mut crate::tiff::Tiff,
    i: usize,
    w: u32,
    h: u32,
    reduced: bool,
    limits: &Limits,
) -> Result<(), CodecError> {
    let components = tiff.number(i, 277)?.unwrap_or(1);
    if !(1..=4).contains(&components) {
        return Err(CodecError::Limits);
    }
    if !reduced && tiff.flavor != crate::tiff::Flavor::Panasonic {
        limits.check(
            w.checked_mul(u32::try_from(components).map_err(|_| CodecError::Limits)?)
                .ok_or(CodecError::Limits)?,
            h,
            1,
        )?;
    }
    Ok(())
}
fn check_sensor(
    tiff: &mut crate::tiff::Tiff,
    i: usize,
    compression: u64,
    limits: &Limits,
    sensor_budget: &mut u64,
    sensor_count: &mut usize,
) -> Result<(), CodecError> {
    if matches!(compression, 6 | 7 | 34892 | 52546) || tiff.has(i, 0xc5d8) {
        for (offset_tag, length_tag) in [(273, 279), (324, 325)] {
            let offsets = tiff.numbers(i, offset_tag, 4096)?;
            let lengths = tiff.numbers(i, length_tag, 4096)?;
            if !lengths.is_empty() && lengths.len() != offsets.len() {
                return Err(CodecError::Decode);
            }
            for (n, offset) in offsets.iter().enumerate() {
                if compression == 52546 {
                    *sensor_count += 1;
                    if *sensor_count > 4096 {
                        return Err(CodecError::Limits);
                    }
                    let len = *lengths.get(n).ok_or(CodecError::Decode)?;
                    let len = len.min(16 << 20);
                    *sensor_budget = sensor_budget.checked_sub(len).ok_or(CodecError::Limits)?;
                    native::check_jxl_bytes(&tiff.data(*offset, len, limits)?, limits)?;
                    continue;
                }
                check_sensor_frame(
                    tiff,
                    *offset,
                    lengths.get(n).copied(),
                    limits,
                    sensor_budget,
                    sensor_count,
                )?;
            }
        }
    }
    // Tile dimensions can also exceed the enclosing image dimensions.
    if let Some((w, h)) = tiff.number(i, 322)?.zip(tiff.number(i, 323)?) {
        limits.check(
            u32::try_from(w).map_err(|_| CodecError::Limits)?,
            u32::try_from(h).map_err(|_| CodecError::Limits)?,
            1,
        )?;
    }
    Ok(())
}
fn check_sensor_frame(
    tiff: &mut crate::tiff::Tiff,
    offset: u64,
    len: Option<u64>,
    limits: &Limits,
    budget: &mut u64,
    count: &mut usize,
) -> Result<(), CodecError> {
    *count += 1;
    if *count > 4096 {
        return Err(CodecError::Limits);
    }
    let (w, h, components) = tiff.sensor_frame(offset, len, limits, budget)?;
    limits.check(w.checked_mul(components).ok_or(CodecError::Limits)?, h, 1)
}
fn probe_cr3(path: &Path, limits: &Limits) -> Result<Probe, CodecError> {
    // No rawler entry point is allowed for this container, including metadata.
    // The walker validates every candidate JPEG before allocating its buffer.
    let bytes = crate::bmff::preview(path, limits)?.ok_or(CodecError::Unsupported)?;
    let (width, height) = crate::headers::jpeg_dimensions(&bytes, limits)?;
    let orientation = native::Image::jpeg_orientation(&bytes, limits)?;
    Ok(Probe {
        width,
        height,
        orientation,
        make: "Canon".to_owned(),
        preview: Some(bytes),
        cr3: true,
    })
}

pub(crate) fn identify(path: &Path, limits: &Limits) -> Result<Identity, CodecError> {
    let probe = probe(path, limits)?;
    let profile = probe
        .preview
        .as_deref()
        .map(|bytes| native::Image::jpeg_profile(bytes, limits))
        .transpose()?
        .flatten();
    Ok(Identity {
        kind: Kind::Raw(RawMake(probe.make)),
        width: probe.width,
        height: probe.height,
        orientation: probe.orientation,
        has_icc: profile.is_some(),
        icc_desc: profile.as_deref().and_then(native::profile_description),
        bits: 16,
        has_alpha: false,
        has_gain_map: false,
        pages: 1,
    })
}
#[cfg(test)]
std::thread_local! { pub(crate) static PIXEL_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
pub(crate) fn decode(path: &Path, limits: &Limits) -> Result<Working, CodecError> {
    let probe = probe(path, limits)?;
    #[cfg(test)]
    PIXEL_DECODES.set(PIXEL_DECODES.get() + 1);
    if let Some(bytes) = probe.preview {
        // Materialize while the encoded buffer is alive. The exact JPEG, including
        // its ICC, enters the same import path as a standalone profiled JPEG.
        return native::Image::jpeg_decode(&bytes, limits, probe.orientation);
    }
    if probe.cr3 {
        return Err(CodecError::Unsupported);
    }
    let source = RawSource::new(path).map_err(|_| CodecError::Io)?;
    let decoder = rawler::get_decoder(&source).map_err(|_| CodecError::Decode)?;
    let raw = decoder
        .raw_image(&source, &RawDecodeParams::default(), false)
        .map_err(|_| CodecError::Decode)?;
    let width = u32::try_from(raw.width).map_err(|_| CodecError::Limits)?;
    let height = u32::try_from(raw.height).map_err(|_| CodecError::Limits)?;
    limits.check(width, height, 1)?;
    if width > probe.width || height > probe.height {
        return Err(CodecError::Mismatch);
    }
    let image = rawler::imgop::develop::RawDevelop::default()
        .develop_intermediate(&raw)
        .map_err(|_| CodecError::Decode)?
        .to_dynamic_image()
        .ok_or(CodecError::Decode)?
        .to_rgb16();
    crate::from_image(
        native::Image::rgb16(image.as_raw(), image.width(), image.height())?
            .with_orientation(probe.orientation)?,
    )
}
pub(crate) fn metadata(path: &Path) -> Result<Metadata, CodecError> {
    let limits = Limits::default();
    let probe = probe(path, &limits)?;
    if probe.cr3 {
        let bytes = probe.preview.ok_or(CodecError::Unsupported)?;
        let mut md = native::Image::jpeg_metadata(&bytes, &limits)?;
        md.make = Some(probe.make);
        return Ok(md);
    }
    // TIFF sensor headers must pass the same checks even for metadata calls.
    let source = RawSource::new(path).map_err(|_| CodecError::Io)?;
    let decoder = rawler::get_decoder(&source).map_err(|_| CodecError::Decode)?;
    let md = decoder
        .raw_metadata(&source, &RawDecodeParams::default())
        .map_err(|_| CodecError::Decode)?;
    let exif = md.exif;
    let rational = |r: rawler::formats::tiff::Rational| {
        let n = f64::from(r.n) / f64::from(r.d);
        n.is_finite().then_some(n)
    };
    let coordinate = |parts: Option<[rawler::formats::tiff::Rational; 3]>,
                      reference: Option<String>,
                      max: f64|
     -> Option<f64> {
        let [d, m, s] = parts?;
        let n = rational(d)? + rational(m)? / 60.0 + rational(s)? / 3600.0;
        let sign = if reference.as_deref().is_some_and(|r| r == "S" || r == "W") {
            -1.0
        } else {
            1.0
        };
        (n <= max).then_some(sign * n)
    };
    let gps = exif.gps.and_then(|g| {
        coordinate(g.gps_latitude, g.gps_latitude_ref, 90.0).zip(coordinate(
            g.gps_longitude,
            g.gps_longitude_ref,
            180.0,
        ))
    });
    Ok(Metadata {
        make: Some(crate::metadata::cap(&md.make)),
        model: Some(crate::metadata::cap(&md.model)),
        lens: exif.lens_model.as_deref().map(crate::metadata::cap),
        focal_mm: exif.focal_length.and_then(rational),
        aperture: exif.fnumber.and_then(rational),
        shutter: exif.exposure_time.and_then(rational),
        iso: exif
            .iso_speed
            .or_else(|| exif.iso_speed_ratings.map(u32::from)),
        taken_at: exif.date_time_original.as_deref().map(crate::metadata::cap),
        gps,
        rating: md
            .rating
            .and_then(|n| i8::try_from(n).ok())
            .filter(|n| (-1..=5).contains(n)),
        orientation: u8::try_from(exif.orientation.unwrap_or(1))
            .ok()
            .filter(|n| (1..=8).contains(n))
            .unwrap_or(1),
        ..Metadata::default()
    })
}

fn check_canon_makernotes(
    tiff: &mut crate::tiff::Tiff,
    limits: &Limits,
    sensor_budget: &mut u64,
    sensor_count: &mut usize,
) -> Result<(), CodecError> {
    for i in tiff.canon_makernotes()? {
        if let Some(offset) = tiff.number(i, 0x0081)? {
            check_sensor_frame(tiff, offset, None, limits, sensor_budget, sensor_count)?;
        }
        let offsets = tiff.numbers(i, 0x00c5, 4096)?;
        let lengths = tiff.numbers(i, 0x00c6, 4096)?;
        if offsets.len() != lengths.len() {
            return Err(CodecError::Decode);
        }
        for (offset, len) in offsets.into_iter().zip(lengths) {
            check_sensor_frame(tiff, offset, Some(len), limits, sensor_budget, sensor_count)?;
        }
    }
    Ok(())
}
