// Bounded native buffers and reference numeric kernels use checked dimensions.
#![allow(
    clippy::cast_possible_wrap,
    clippy::borrow_as_ptr,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]
//! All native handles own one `GObject` reference; no handle crosses threads.
use crate::{CodecError, Encode, Identity, Kind};
use std::{
    ffi::{CStr, CString, c_void},
    path::Path,
    ptr::{self, NonNull},
    sync::OnceLock,
};
#[allow(warnings, clippy::all, clippy::pedantic)]
pub(crate) mod ffi {
    include!(concat!(env!("OUT_DIR"), "/native.rs"));
}
pub(crate) const UCHAR: i32 = ffi::VipsBandFormat_VIPS_FORMAT_UCHAR;
pub(crate) const USHORT: i32 = ffi::VipsBandFormat_VIPS_FORMAT_USHORT;
const END: *const std::ffi::c_char = ptr::null();

#[cfg(test)]
std::thread_local! { pub(crate) static GAIN_PIXEL_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub(crate) fn init() -> Result<(), CodecError> {
    init_with_concurrency(2)
}

pub(crate) fn init_with_concurrency(concurrency: u8) -> Result<(), CodecError> {
    static INIT: OnceLock<bool> = OnceLock::new();
    if !(1..=64).contains(&concurrency) {
        return Err(CodecError::Options);
    }
    let ok = INIT.get_or_init(|| {
        // SAFETY: process-global initialization is serialized and its static name outlives the call.
        unsafe {
            // SAFETY: null requests default initialization; no input is parsed.
            // Plugins remain loaded for the lifetime of this one-job process.
            if ffi::heif_init(ptr::null_mut()).code != 0 {
                return false;
            }
            let status = ffi::vips_init(c"yhotos-codec".as_ptr());
            ffi::vips_concurrency_set(i32::from(concurrency));
            ffi::vips_cache_set_max(0);
            ffi::vips_cache_set_max_files(0);
            for domain in [c"VIPS", c"GLib", c"GLib-GObject"] {
                ffi::g_log_set_handler(domain.as_ptr(), 0xff, Some(quiet_log), ptr::null_mut());
            }
            status == 0
        }
    });
    if *ok {
        Ok(())
    } else {
        Err(CodecError::Unsupported)
    }
}

pub(crate) fn concurrency() -> Result<u8, CodecError> {
    // SAFETY: callers initialize libvips first; this only reads its process-global thread limit.
    u8::try_from(unsafe { ffi::vips_concurrency_get() }).map_err(|_| CodecError::Options)
}
unsafe extern "C" fn quiet_log(
    _: *const std::ffi::c_char,
    _: ffi::GLogLevelFlags,
    _: *const std::ffi::c_char,
    _: *mut c_void,
) {
}

pub(crate) fn path_string(path: &Path) -> Result<CString, CodecError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| CodecError::Options)
}

pub(crate) struct Image(NonNull<ffi::VipsImage>);
impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: each Image owns exactly one live GObject reference, released once here.
        unsafe {
            ffi::g_object_unref(self.0.as_ptr().cast());
        }
    }
}
macro_rules! operation {
    ($($call:tt)*) => {{
        let mut output = ptr::null_mut();
        // SAFETY: callers supply live image handles, correctly promoted varargs, and a null sentinel;
        // the output slot is writable and is adopted only after checking status and nullness.
        let status = unsafe { ($($call)*)(&mut output) };
        Image::result(status, output)
    }};
}
impl Image {
    fn result(status: i32, output: *mut ffi::VipsImage) -> Result<Self, CodecError> {
        if status != 0 {
            // SAFETY: clears the calling thread's native diagnostic buffer; it is never exposed.
            unsafe {
                ffi::vips_error_clear();
            }
            return Err(CodecError::Decode);
        }
        NonNull::new(output).map(Self).ok_or(CodecError::Decode)
    }
    pub(crate) fn load(path: &Path, kind: &Kind) -> Result<Self, CodecError> {
        Self::load_inner(path, kind, false)
    }
    pub(crate) fn load_sdr(path: &Path, kind: &Kind) -> Result<Self, CodecError> {
        Self::load_inner(path, kind, true)
    }
    fn load_inner(path: &Path, kind: &Kind, force_sdr: bool) -> Result<Self, CodecError> {
        init()?;
        let info = if matches!(kind, Kind::Heif | Kind::Avif) {
            Some(crate::bmff::heif_info(
                std::fs::File::open(path).map_err(|_| CodecError::Io)?,
            )?)
        } else {
            None
        };
        let input_path = path;
        let path = path_string(path)?;
        // Dispatch only to the decoder selected by our magic sniffer. Generic
        // foreign_find_load probes unrelated fallback loaders (including Magick
        // and RAW), which may decode while probing or hang on short JXL input.
        // Each explicit loader independently verifies its own container signature.
        #[cfg(not(feature = "heif"))]
        if matches!(kind, Kind::Heif) {
            return Err(CodecError::Unsupported);
        }
        let image = if force_sdr || info.as_ref().is_some_and(|i| i.sdr_only) {
            Err(CodecError::Decode)
        } else {
            operation!(|out| match kind {
                Kind::Jpeg =>
                    ffi::vips_jpegload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
                Kind::Png => ffi::vips_pngload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
                Kind::Tiff =>
                    ffi::vips_tiffload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
                Kind::Heif | Kind::Avif =>
                    ffi::vips_heifload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
                Kind::Jxl => ffi::vips_jxlload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
                Kind::Webp =>
                    ffi::vips_webpload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
                _ => -1,
            })
        };
        let image = match image {
            Ok(image) => image,
            Err(error) if info.as_ref().is_some_and(|i| i.gain_map) => {
                let bytes = crate::hdr::read(input_path, &crate::Limits::default())?;
                let Ok(bytes) = crate::gain_container::heif_sdr(&bytes) else {
                    return Err(error);
                };
                Self::heif_source(&bytes)?
            }
            Err(_) if *kind == Kind::Jxl => {
                let bytes = crate::hdr::read(input_path, &crate::Limits::default())?;
                let bytes = crate::gain_container::jxl_sdr(&bytes)?;
                Self::jxl_sdr_buffer(&bytes, &crate::Limits::default())?
            }
            Err(error) => return Err(error),
        };
        if let Some(info) = &info {
            // SAFETY: image owns a live private header; this bounded scalar is copied.
            unsafe {
                ffi::vips_image_set_int(
                    image.0.as_ptr(),
                    c"yhotos-gain-map".as_ptr(),
                    i32::from(info.gain_map),
                );
            }
        }
        if !image.has("icc-profile-data")
            && let Some(nclx) = info.and_then(|info| info.nclx)
        {
            return image.with_profile(nclx_profile(nclx)?);
        }
        Ok(image)
    }
    pub(crate) fn jxl_sdr_buffer(bytes: &[u8], limits: &crate::Limits) -> Result<Self, CodecError> {
        init()?;
        // Metadata-only and fuzz callers also check dimensions before pixels.
        jxl_stored_identity(bytes, limits)?;
        // SAFETY: the buffer loader borrows bytes until evaluation. Materialization
        // completes before this checked container buffer is released. The
        // malformed-JXL corpus decodes/encodes after this function returns.
        let image = operation!(|out| ffi::vips_jxlload_buffer(
            bytes.as_ptr().cast_mut().cast(),
            bytes.len(),
            out,
            c"fail_on".as_ptr(),
            3i32,
            END
        ))?;
        image.materialize()
    }
    fn heif_source(bytes: &[u8]) -> Result<Self, CodecError> {
        // SAFETY: blob_copy owns a copy of the bounded container. The source
        // refs the blob and the lazy loader refs the source, so both remain live
        // through header probing and later pixel evaluation. Local references
        // are released exactly once on both success and failure. Mutation tests
        // decode and encode after this function's source buffer has been freed.
        let source = unsafe {
            let blob = NonNull::new(ffi::vips_blob_copy(bytes.as_ptr().cast(), bytes.len()))
                .ok_or(CodecError::Decode)?;
            let source = NonNull::new(ffi::vips_source_new_from_blob(blob.as_ptr()));
            ffi::vips_area_unref(blob.as_ptr().cast());
            source.ok_or(CodecError::Decode)?
        };
        let result = operation!(|out| ffi::vips_heifload_source(
            source.as_ptr(),
            out,
            c"fail_on".as_ptr(),
            3i32,
            END
        ));
        // SAFETY: release the unique local source reference after the loader
        // has taken its own reference (or failed).
        unsafe {
            ffi::g_object_unref(source.as_ptr().cast());
        }
        result
    }
    pub(crate) fn gain_jpeg(
        bytes: &[u8],
        limits: &crate::Limits,
        base: &Identity,
    ) -> Result<Self, CodecError> {
        crate::hdr::check_geometry(crate::headers::jpeg_dimensions(bytes, limits)?, base)?;
        #[cfg(test)]
        GAIN_PIXEL_DECODES.set(GAIN_PIXEL_DECODES.get() + 1);
        let image = Self::jpeg(bytes, limits)?;
        if image.format() != UCHAR || !matches!(image.bands(), 1 | 3) || image.has_alpha() {
            return Err(CodecError::Unsupported);
        }
        image.oriented(base.orientation)
    }
    pub(crate) fn oriented(&self, orientation: u8) -> Result<Self, CodecError> {
        if !(1..=8).contains(&orientation) {
            return Err(CodecError::Decode);
        }
        let image = self.copy()?;
        // SAFETY: the private header is live, the validated EXIF orientation is copied.
        unsafe {
            ffi::vips_image_set_int(
                image.0.as_ptr(),
                c"orientation".as_ptr(),
                i32::from(orientation),
            );
        }
        image.autorot()?.materialize()
    }
    pub(crate) fn undo_orientation(&self, orientation: u8) -> Result<Self, CodecError> {
        self.oriented(match orientation {
            6 => 8,
            8 => 6,
            other => other,
        })
    }
    pub(crate) fn gain_jxl(
        bytes: &[u8],
        limits: &crate::Limits,
        base: &Identity,
    ) -> Result<Self, CodecError> {
        let identity = jxl_stored_identity(bytes, limits)?;
        let dimensions = identity
            .width
            .zip(identity.height)
            .ok_or(CodecError::Decode)?;
        crate::hdr::check_geometry(dimensions, base)?;
        if identity.has_alpha {
            return Err(CodecError::Unsupported);
        }
        init()?;
        #[cfg(test)]
        GAIN_PIXEL_DECODES.set(GAIN_PIXEL_DECODES.get() + 1);
        let image = operation!(|out| ffi::vips_jxlload_buffer(
            bytes.as_ptr().cast_mut().cast(),
            bytes.len(),
            out,
            c"fail_on".as_ptr(),
            3i32,
            END
        ))?;
        let decoded_dimensions = if identity.orientation >= 5 {
            (dimensions.1, dimensions.0)
        } else {
            dimensions
        };
        if image.size()? != decoded_dimensions {
            return Err(CodecError::Mismatch);
        }
        if !matches!(image.bands(), 1 | 3) {
            return Err(CodecError::Unsupported);
        }
        let image = match image.format() {
            UCHAR => image,
            USHORT => image.linear_offset(1.0 / 257.0, 0.5)?.cast(UCHAR)?,
            _ => return Err(CodecError::Unsupported),
        };
        // libvips/libjxl already applied the auxiliary codestream orientation.
        // Undo it, then apply only the base transform, as required for gain maps.
        // Evaluation completes while the borrowed compressed slice remains live.
        let image = if identity.orientation == 1 {
            image
        } else {
            image.undo_orientation(identity.orientation)?
        };
        image.gain_header()?.oriented(base.orientation)
    }
    pub(crate) fn gain_heif(
        path: &Path,
        id: u32,
        limits: &crate::Limits,
        base: &Identity,
        auxiliary_transforms: &[u8],
    ) -> Result<Self, CodecError> {
        let path = path_string(path)?;
        init()?;
        // SAFETY: context owns all parsed container state; out pointers start null, are checked
        // before use, and are released exactly once below on success and every error path.
        // Header dimensions are bounded before decode. The library guarantees stride*height
        // allocated bytes for the requested interleaved RGB plane; copied rows cannot escape it.
        unsafe {
            let context = NonNull::new(ffi::heif_context_alloc()).ok_or(CodecError::Decode)?;
            let mut handle = ptr::null_mut();
            let mut decoded = ptr::null_mut();
            let result = (|| {
                if ffi::heif_context_read_from_file(context.as_ptr(), path.as_ptr(), ptr::null())
                    .code
                    != 0
                    || ffi::heif_context_get_image_handle(context.as_ptr(), id, &mut handle).code
                        != 0
                    || handle.is_null()
                {
                    return Err(CodecError::Decode);
                }
                let w = u32::try_from(ffi::heif_image_handle_get_width(handle))
                    .map_err(|_| CodecError::Limits)?;
                let h = u32::try_from(ffi::heif_image_handle_get_height(handle))
                    .map_err(|_| CodecError::Limits)?;
                limits.check(w, h, 1)?;
                let mut dimensions = (w, h);
                for orientation in auxiliary_transforms {
                    if *orientation >= 5 {
                        dimensions = (dimensions.1, dimensions.0);
                    }
                }
                crate::hdr::check_geometry(dimensions, base)?;
                #[cfg(test)]
                GAIN_PIXEL_DECODES.set(GAIN_PIXEL_DECODES.get() + 1);
                if ffi::heif_decode_image(
                    handle,
                    &mut decoded,
                    ffi::heif_colorspace_heif_colorspace_RGB,
                    ffi::heif_chroma_heif_chroma_interleaved_RGB,
                    ptr::null(),
                )
                .code
                    != 0
                    || decoded.is_null()
                {
                    return Err(CodecError::Decode);
                }
                if ffi::heif_image_get_width(decoded, ffi::heif_channel_heif_channel_interleaved)
                    != i32::try_from(w).map_err(|_| CodecError::Limits)?
                    || ffi::heif_image_get_height(
                        decoded,
                        ffi::heif_channel_heif_channel_interleaved,
                    ) != i32::try_from(h).map_err(|_| CodecError::Limits)?
                {
                    return Err(CodecError::Decode);
                }
                let mut stride = 0;
                let data = ffi::heif_image_get_plane_readonly(
                    decoded,
                    ffi::heif_channel_heif_channel_interleaved,
                    &mut stride,
                );
                let stride = usize::try_from(stride).map_err(|_| CodecError::Limits)?;
                let row = w as usize * 3;
                if data.is_null()
                    || stride < row
                    || stride.checked_mul(h as usize).is_none_or(|n| n > 1 << 30)
                {
                    return Err(CodecError::Limits);
                }
                let mut bytes = Vec::with_capacity(row * h as usize);
                for y in 0..h as usize {
                    bytes.extend_from_slice(std::slice::from_raw_parts(data.add(y * stride), row));
                }
                Self::samples(&bytes, w, h, 3)
            })();
            if !decoded.is_null() {
                ffi::heif_image_release(decoded);
            }
            if !handle.is_null() {
                ffi::heif_image_handle_release(handle);
            }
            ffi::heif_context_free(context.as_ptr());
            result
        }
    }
    pub(crate) fn samples(
        bytes: &[u8],
        width: u32,
        height: u32,
        bands: u32,
    ) -> Result<Self, CodecError> {
        init()?;
        crate::Limits::default().check(width, height, 1)?;
        if !matches!(bands, 1 | 3)
            || bytes.len() != width as usize * height as usize * bands as usize
        {
            return Err(CodecError::Decode);
        }
        // SAFETY: dimensions and channel count cover exactly the checked slice; vips copies it.
        let image = Self::result(0, unsafe {
            ffi::vips_image_new_from_memory_copy(
                bytes.as_ptr().cast(),
                bytes.len(),
                width as i32,
                height as i32,
                bands as i32,
                UCHAR,
            )
        })?;
        operation!(|out| ffi::vips_copy(
            image.0.as_ptr(),
            out,
            c"interpretation".as_ptr(),
            if bands == 1 {
                ffi::VipsInterpretation_VIPS_INTERPRETATION_B_W
            } else {
                ffi::VipsInterpretation_VIPS_INTERPRETATION_sRGB
            },
            END
        ))
    }
    fn gain_header(&self) -> Result<Self, CodecError> {
        let bands = self.bands();
        if self.format() != UCHAR || !matches!(bands, 1 | 3) {
            return Err(CodecError::Unsupported);
        }
        let image = operation!(|out| ffi::vips_copy(
            self.0.as_ptr(),
            out,
            c"interpretation".as_ptr(),
            if bands == 1 {
                ffi::VipsInterpretation_VIPS_INTERPRETATION_B_W
            } else {
                ffi::VipsInterpretation_VIPS_INTERPRETATION_sRGB
            },
            END
        ))?;
        // SAFETY: the live private header is owned here; remove releases only its ICC metadata.
        unsafe {
            ffi::vips_image_remove(image.0.as_ptr(), c"icc-profile-data".as_ptr());
        }
        Ok(image)
    }
    pub(crate) fn gain_save_jxl(&self, path: &Path) -> Result<(), CodecError> {
        self.gain_header()?.save(
            Encode::Jxl {
                distance: 0.0,
                effort: 3,
                hdr: false,
            },
            path,
        )
    }
    pub(crate) fn gain_save(&self, path: &Path) -> Result<(), CodecError> {
        let path = path_string(path)?;
        // SAFETY: owned image and path remain live; all promoted varargs match their option types.
        let status = unsafe {
            ffi::vips_jpegsave(
                self.0.as_ptr(),
                path.as_ptr(),
                c"Q".as_ptr(),
                100i32,
                c"subsample_mode".as_ptr(),
                ffi::VipsForeignSubsample_VIPS_FOREIGN_SUBSAMPLE_OFF,
                c"keep".as_ptr(),
                ffi::VipsForeignKeep_VIPS_FOREIGN_KEEP_NONE,
                END,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(CodecError::Encode)
        }
    }
    fn with_profile(&self, profile: &[u8]) -> Result<Self, CodecError> {
        let image = self.copy()?;
        // SAFETY: image owns a private header; set_blob_copy copies this bounded ICC slice.
        unsafe {
            ffi::vips_image_set_blob_copy(
                image.0.as_ptr(),
                c"icc-profile-data".as_ptr(),
                profile.as_ptr().cast(),
                profile.len(),
            );
            ffi::vips_image_set_int(image.0.as_ptr(), c"yhotos-nclx-profile".as_ptr(), 1);
        }
        Ok(image)
    }
    fn jpeg(bytes: &[u8], limits: &crate::Limits) -> Result<Self, CodecError> {
        let dimensions = crate::headers::jpeg_dimensions(bytes, limits)?;
        init()?;
        // The private callers below keep bytes alive through every lazy operation.
        let image = operation!(|out| ffi::vips_jpegload_buffer(
            bytes.as_ptr().cast::<c_void>().cast_mut(),
            bytes.len(),
            out,
            c"fail_on".as_ptr(),
            3i32,
            END
        ))?;
        if image.size()? != dimensions {
            return Err(CodecError::Mismatch);
        }
        Ok(image)
    }
    #[cfg(feature = "raw")]
    pub(crate) fn jpeg_profile(
        bytes: &[u8],
        limits: &crate::Limits,
    ) -> Result<Option<Vec<u8>>, CodecError> {
        // Drop the lazy loader before returning; pixel evaluation is never requested.
        Ok(Self::jpeg(bytes, limits)?.blob("icc-profile-data"))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn jpeg_info(
        bytes: &[u8],
        limits: &crate::Limits,
    ) -> Result<(u8, Option<Vec<u8>>), CodecError> {
        let image = Self::jpeg(bytes, limits)?;
        let orientation = u8::try_from(image.orientation())
            .ok()
            .filter(|n| (1..=8).contains(n))
            .ok_or(CodecError::Decode)?;
        Ok((orientation, image.blob("icc-profile-data")))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn jpeg_metadata(
        bytes: &[u8],
        limits: &crate::Limits,
    ) -> Result<crate::Metadata, CodecError> {
        // Only owned fields escape; the lazy loader never outlives its buffer.
        Ok(crate::metadata::from_image(&Self::jpeg(bytes, limits)?))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn jpeg_decode(
        bytes: &[u8],
        limits: &crate::Limits,
        orientation: u8,
    ) -> Result<crate::Working, CodecError> {
        // from_image materializes the pipeline before this borrowed buffer expires.
        crate::from_image(Self::jpeg(bytes, limits)?.with_orientation(orientation)?)
    }
    pub(crate) fn size(&self) -> Result<(u32, u32), CodecError> {
        // SAFETY: self owns a live image; header access never evaluates pixels.
        let (w, h) = unsafe {
            (
                ffi::vips_image_get_width(self.0.as_ptr()),
                ffi::vips_image_get_height(self.0.as_ptr()),
            )
        };
        Ok((
            u32::try_from(w).map_err(|_| CodecError::Limits)?,
            u32::try_from(h).map_err(|_| CodecError::Limits)?,
        ))
    }
    pub(crate) fn bands(&self) -> i32 {
        // SAFETY: self owns a live image.
        unsafe { ffi::vips_image_get_bands(self.0.as_ptr()) }
    }
    pub(crate) fn orientation(&self) -> i32 {
        // SAFETY: self owns a live image; missing orientation defaults to 1 in libvips.
        unsafe { ffi::vips_image_get_orientation(self.0.as_ptr()) }
    }
    pub(crate) fn bits(&self) -> u8 {
        if let Some(bits) = self
            .int("bits-per-sample")
            .and_then(|v| u8::try_from(v).ok())
        {
            return bits;
        }
        self.sample_bits()
    }
    pub(crate) fn sample_bits(&self) -> u8 {
        match self.format() {
            ffi::VipsBandFormat_VIPS_FORMAT_USHORT | ffi::VipsBandFormat_VIPS_FORMAT_SHORT => 16,
            ffi::VipsBandFormat_VIPS_FORMAT_FLOAT
            | ffi::VipsBandFormat_VIPS_FORMAT_UINT
            | ffi::VipsBandFormat_VIPS_FORMAT_INT => 32,
            _ => 8,
        }
    }
    fn format(&self) -> i32 {
        // SAFETY: self owns a live image; native format is an enum.
        unsafe { ffi::vips_image_get_format(self.0.as_ptr()) }
    }
    pub(crate) fn alpha16(&self) -> Result<Self, CodecError> {
        // Most integer loaders span the sample type. HEIF instead left-shifts
        // 10/12-bit samples into ushort, leaving the low bits zero at opacity.
        // Floating TIFF/scRGB alpha spans 0..1, independent of RGB luminance.
        let scale = match self.format() {
            ffi::VipsBandFormat_VIPS_FORMAT_UCHAR => 257.0,
            ffi::VipsBandFormat_VIPS_FORMAT_USHORT
                if matches!(
                    self.string("vips-loader").as_deref(),
                    Some("heifload" | "heifload_source")
                ) && matches!(self.bits(), 10 | 12) =>
            {
                let bits = self.bits();
                let opaque = ((1u32 << bits) - 1) << (16 - bits);
                65535.0 / f64::from(opaque)
            }
            ffi::VipsBandFormat_VIPS_FORMAT_USHORT => 1.0,
            ffi::VipsBandFormat_VIPS_FORMAT_FLOAT | ffi::VipsBandFormat_VIPS_FORMAT_DOUBLE => {
                65535.0
            }
            _ => return Err(CodecError::Unsupported),
        };
        self.extract(self.bands() - 1, 1)?
            // Round before the integer cast, including full HEIF opacity when
            // floating arithmetic lands just below 65535.
            .linear_offset(scale, 0.5)?
            .cast(USHORT)
    }
    pub(crate) fn has_alpha(&self) -> bool {
        // SAFETY: self owns a live image.
        unsafe { ffi::vips_image_hasalpha(self.0.as_ptr()) != 0 }
    }
    pub(crate) fn has(&self, name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: name and self remain valid for the header query.
        unsafe { ffi::vips_image_get_typeof(self.0.as_ptr(), name.as_ptr()) != 0 }
    }
    pub(crate) fn int(&self, name: &str) -> Option<i32> {
        if !self.has(name) {
            return None;
        }
        let name = CString::new(name).ok()?;
        let mut value = 0;
        // SAFETY: name is NUL terminated; value is a writable integer output.
        let result = unsafe { ffi::vips_image_get_int(self.0.as_ptr(), name.as_ptr(), &mut value) };
        (result == 0).then_some(value)
    }
    pub(crate) fn string(&self, name: &str) -> Option<String> {
        if !self.has(name) {
            return None;
        }
        let name = CString::new(name).ok()?;
        let mut value = ptr::null();
        // SAFETY: the borrowed NUL-terminated result belongs to self and is copied before return.
        unsafe {
            if ffi::vips_image_get_string(self.0.as_ptr(), name.as_ptr(), &mut value) != 0
                || value.is_null()
            {
                return None;
            }
            let bytes = CStr::from_ptr(value).to_bytes();
            let bytes = bytes.get(..bytes.len().min(4096))?;
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
    pub(crate) fn blob(&self, name: &str) -> Option<Vec<u8>> {
        if !self.has(name) {
            return None;
        }
        let name = CString::new(name).ok()?;
        let mut data = ptr::null();
        let mut len = 0;
        // SAFETY: native blob remains owned by self; copy only the reported initialized extent.
        unsafe {
            if ffi::vips_image_get_blob(self.0.as_ptr(), name.as_ptr(), &mut data, &mut len) != 0
                || data.is_null()
                || len > 16 << 20
            {
                return None;
            }
            Some(std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec())
        }
    }
    pub(crate) fn copy(&self) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_copy(self.0.as_ptr(), out, END))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn with_orientation(&self, orientation: u8) -> Result<Self, CodecError> {
        let image = self.copy()?;
        // SAFETY: this private image header is writable and orientation was range checked.
        unsafe {
            ffi::vips_image_set_int(
                image.0.as_ptr(),
                c"orientation".as_ptr(),
                i32::from(orientation),
            );
        }
        Ok(image)
    }
    pub(crate) fn autorot(&self) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_autorot(self.0.as_ptr(), out, END))
    }
    pub(crate) fn cast(&self, format: i32) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_cast(self.0.as_ptr(), out, format, END))
    }
    pub(crate) fn xyz(&self) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_copy(
            self.0.as_ptr(),
            out,
            c"interpretation".as_ptr(),
            ffi::VipsInterpretation_VIPS_INTERPRETATION_XYZ,
            END
        ))
    }
    pub(crate) fn linear(&self, scale: f64) -> Result<Self, CodecError> {
        self.linear_offset(scale, 0.0)
    }
    fn linear_offset(&self, scale: f64, offset: f64) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_linear1(self.0.as_ptr(), out, scale, offset, END))
    }
    pub(crate) fn extract(&self, start: i32, n: i32) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_extract_band(self.0.as_ptr(), out, start, c"n".as_ptr(), n, END))
    }
    pub(crate) fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_extract_area(
            self.0.as_ptr(),
            out,
            x as i32,
            y as i32,
            w as i32,
            h as i32,
            END
        ))
    }
    pub(crate) fn resize(&self, scale: f64) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_resize(
            self.0.as_ptr(),
            out,
            scale,
            c"kernel".as_ptr(),
            ffi::VipsKernel_VIPS_KERNEL_LANCZOS3,
            END
        ))
    }
    pub(crate) fn premultiply(&self) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_premultiply(
            self.0.as_ptr(),
            out,
            c"max_alpha".as_ptr(),
            65535.0f64,
            END
        ))
    }
    pub(crate) fn unpremultiply(&self) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_unpremultiply(
            self.0.as_ptr(),
            out,
            c"max_alpha".as_ptr(),
            65535.0f64,
            END
        ))
    }
    pub(crate) fn join(&self, other: &Self) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_bandjoin2(self.0.as_ptr(), other.0.as_ptr(), out, END))
    }
    pub(crate) fn join_constant(&self, value: f64) -> Result<Self, CodecError> {
        operation!(|out| ffi::vips_bandjoin_const1(self.0.as_ptr(), out, value, END))
    }
    pub(crate) fn import(&self) -> Result<Self, CodecError> {
        if matches!(
            self.format(),
            ffi::VipsBandFormat_VIPS_FORMAT_FLOAT | ffi::VipsBandFormat_VIPS_FORMAT_DOUBLE
        ) {
            // icc_import casts device RGB floats to uchar in libvips 8.15+.
            // scRGB is already linear sRGB, normalized to 1.0, so transform
            // directly to libvips' D65 XYZ without quantizing device samples.
            // SAFETY: self owns a live image; this is a header-only enum query.
            let interpretation = unsafe { ffi::vips_image_get_interpretation(self.0.as_ptr()) };
            if interpretation != ffi::VipsInterpretation_VIPS_INTERPRETATION_scRGB {
                return Err(CodecError::Unsupported);
            }
            return operation!(|out| ffi::vips_scRGB2XYZ(self.0.as_ptr(), out, END));
        }
        operation!(|out| ffi::vips_icc_import(
            self.0.as_ptr(),
            out,
            c"embedded".as_ptr(),
            1i32,
            c"input_profile".as_ptr(),
            c"srgb".as_ptr(),
            c"pcs".as_ptr(),
            ffi::VipsPCS_VIPS_PCS_XYZ,
            c"intent".as_ptr(),
            ffi::VipsIntent_VIPS_INTENT_RELATIVE,
            END
        ))
    }
    pub(crate) fn export(&self, profile: Option<&[u8]>) -> Result<Self, CodecError> {
        let image = self.copy()?;
        // SAFETY: set_blob_copy copies the immutable profile, remove only mutates our private header.
        unsafe {
            if let Some(profile) = profile {
                ffi::vips_image_set_blob_copy(
                    image.0.as_ptr(),
                    c"icc-profile-data".as_ptr(),
                    profile.as_ptr().cast(),
                    profile.len(),
                );
            } else {
                ffi::vips_image_remove(image.0.as_ptr(), c"icc-profile-data".as_ptr());
            }
        }
        if profile.is_some() {
            operation!(|out| ffi::vips_icc_export(
                image.0.as_ptr(),
                out,
                c"depth".as_ptr(),
                16i32,
                c"intent".as_ptr(),
                ffi::VipsIntent_VIPS_INTENT_RELATIVE,
                END
            ))
        } else {
            operation!(|out| ffi::vips_icc_export(
                image.0.as_ptr(),
                out,
                c"depth".as_ptr(),
                16i32,
                c"output_profile".as_ptr(),
                c"srgb".as_ptr(),
                c"intent".as_ptr(),
                ffi::VipsIntent_VIPS_INTENT_RELATIVE,
                END
            ))
        }
    }
    pub(crate) fn materialize(&self) -> Result<Self, CodecError> {
        // SAFETY: copy_memory returns a new owned reference or null; pixels are evaluated once.
        let output = unsafe { ffi::vips_image_copy_memory(self.0.as_ptr()) };
        Self::result(0, output)
    }
    pub(crate) fn pixels(&self) -> Result<Vec<u8>, CodecError> {
        let mut len = 0;
        // SAFETY: write_to_memory returns a GLib allocation of len bytes, copied then freed once.
        unsafe {
            let data = ffi::vips_image_write_to_memory(self.0.as_ptr(), &mut len);
            if data.is_null() {
                return Err(CodecError::Decode);
            }
            let result = std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec();
            ffi::g_free(data);
            Ok(result)
        }
    }
    fn flatten_white(&self) -> Result<Self, CodecError> {
        // SAFETY: array_double_new copies one valid f64. The non-null owned
        // array stays alive through flatten and is unreferenced exactly once.
        let background = unsafe { ffi::vips_array_double_new(&65535.0, 1) };
        if background.is_null() {
            return Err(CodecError::Encode);
        }
        let image = operation!(|out| ffi::vips_flatten(
            self.0.as_ptr(),
            out,
            c"background".as_ptr(),
            background,
            c"max_alpha".as_ptr(),
            65535.0f64,
            END
        ));
        // SAFETY: background is the live owned VipsArrayDouble above, whose
        // first member is VipsArea; flatten retains its own reference if needed.
        unsafe { ffi::vips_area_unref(background.cast()) };
        image
    }
    pub(crate) fn save(&self, enc: Encode, out: &Path) -> Result<(), CodecError> {
        let path = path_string(out)?;
        let keep = ffi::VipsForeignKeep_VIPS_FOREIGN_KEEP_ICC;
        // SAFETY: all arguments use their declared C types and valid owned images/strings;
        // option ranges are checked in the match guards and every vararg list is null-terminated.
        let status = unsafe {
            match enc {
                Encode::Avif { hdr: true, .. }
                | Encode::Jxl { hdr: true, .. }
                | Encode::Jpeg { hdr: true, .. } => return Err(CodecError::Unsupported),
                Encode::Avif { quality, speed, .. }
                    if (1..=100).contains(&quality) && speed <= 9 =>
                {
                    ffi::vips_heifsave(
                        self.0.as_ptr(),
                        path.as_ptr(),
                        c"Q".as_ptr(),
                        i32::from(quality),
                        c"effort".as_ptr(),
                        i32::from(9 - speed),
                        c"compression".as_ptr(),
                        ffi::VipsForeignHeifCompression_VIPS_FOREIGN_HEIF_COMPRESSION_AV1,
                        c"bitdepth".as_ptr(),
                        10i32,
                        c"keep".as_ptr(),
                        keep,
                        END,
                    )
                }
                Encode::Jxl {
                    distance, effort, ..
                } if distance.is_finite()
                    && (0.0..=25.0).contains(&distance)
                    && (1..=9).contains(&effort) =>
                {
                    ffi::vips_jxlsave(
                        self.0.as_ptr(),
                        path.as_ptr(),
                        c"distance".as_ptr(),
                        f64::from(distance),
                        c"effort".as_ptr(),
                        i32::from(effort),
                        c"keep".as_ptr(),
                        keep,
                        END,
                    )
                }
                Encode::Jpeg {
                    quality,
                    progressive,
                    chroma_subsampling,
                    ..
                } if (1..=100).contains(&quality) => {
                    // Flatten while opacity and RGB still span 0..65535.
                    // Casting first leaves RGB16 interpretation on uchar data,
                    // causing the JPEG saver's implicit flatten to misread alpha.
                    // flatten and the subsequent casts retain the ICC header.
                    let flattened = self.has_alpha().then(|| self.flatten_white()).transpose()?;
                    let image = flattened
                        .as_ref()
                        .unwrap_or(self)
                        .linear(1.0 / 257.0)?
                        .cast(UCHAR)?;
                    ffi::vips_jpegsave(
                        image.0.as_ptr(),
                        path.as_ptr(),
                        c"Q".as_ptr(),
                        i32::from(quality),
                        c"interlace".as_ptr(),
                        i32::from(progressive),
                        c"subsample_mode".as_ptr(),
                        if chroma_subsampling {
                            ffi::VipsForeignSubsample_VIPS_FOREIGN_SUBSAMPLE_ON
                        } else {
                            ffi::VipsForeignSubsample_VIPS_FOREIGN_SUBSAMPLE_OFF
                        },
                        c"keep".as_ptr(),
                        keep,
                        END,
                    )
                }
                _ => return Err(CodecError::Options),
            }
        };
        if status == 0 {
            Ok(())
        } else {
            Err(CodecError::Encode)
        }
    }
    #[cfg(feature = "raw")]
    pub(crate) fn rgb16(data: &[u16], w: u32, h: u32) -> Result<Self, CodecError> {
        init()?;
        if data.len() != w as usize * h as usize * 3 {
            return Err(CodecError::Decode);
        }
        // SAFETY: the checked slice covers w*h*3 native-endian u16 samples; libvips copies them.
        let p = unsafe {
            ffi::vips_image_new_from_memory_copy(
                data.as_ptr().cast(),
                std::mem::size_of_val(data),
                w as i32,
                h as i32,
                3,
                USHORT,
            )
        };
        let image = Self::result(0, p)?;
        operation!(|out| ffi::vips_copy(
            image.0.as_ptr(),
            out,
            c"interpretation".as_ptr(),
            ffi::VipsInterpretation_VIPS_INTERPRETATION_RGB16,
            END
        ))
    }
}

pub(crate) fn profile_description(bytes: &[u8]) -> Option<String> {
    // SAFETY: lcms reads the checked slice only during OpenProfileFromMem; the handle is closed once.
    unsafe {
        let p = ffi::cmsOpenProfileFromMem(bytes.as_ptr().cast(), u32::try_from(bytes.len()).ok()?);
        if p.is_null() {
            return None;
        }
        let mut buf = [0u8; 257];
        let len = ffi::cmsGetProfileInfoASCII(
            p,
            ffi::cmsInfoType_cmsInfoDescription,
            c"en".as_ptr(),
            c"US".as_ptr(),
            buf.as_mut_ptr().cast(),
            257,
        );
        ffi::cmsCloseProfile(p);
        if len == 0 {
            return None;
        }
        let end = buf.iter().position(|b| *b == 0).unwrap_or(256);
        Some(crate::metadata::cap(&String::from_utf8_lossy(&buf[..end])))
    }
}

// Validate basic-info and color-header completion with a bounded libjxl state
// machine before entering the explicit loader. Short JXL streams must never
// enter the generic foreign loader probe. No pixel buffers are requested here.
pub(crate) fn identify_jxl(path: &Path, limits: &crate::Limits) -> Result<Identity, CodecError> {
    identify_jxl_reader(
        std::fs::File::open(path).map_err(|_| CodecError::Io)?,
        limits,
    )
}
#[cfg(feature = "raw")]
pub(crate) fn check_jxl_bytes(bytes: &[u8], limits: &crate::Limits) -> Result<(), CodecError> {
    identify_jxl_reader(bytes, limits).map(|_| ())
}
pub(crate) fn identify_jxl_reader(
    input: impl std::io::Read,
    limits: &crate::Limits,
) -> Result<Identity, CodecError> {
    jxl_header_reader(input, limits, false)
}
// Internal gain-map extraction needs stored dimensions and the original transform.
// Public identification stays consistent with libvips' already-oriented base pixels.
pub(crate) fn jxl_stored_identity(
    input: impl std::io::Read,
    limits: &crate::Limits,
) -> Result<Identity, CodecError> {
    jxl_header_reader(input, limits, true)
}
fn jxl_header_reader(
    mut input: impl std::io::Read,
    limits: &crate::Limits,
    keep_orientation: bool,
) -> Result<Identity, CodecError> {
    let cap = usize::try_from(limits.max_bytes.min(16 << 20)).map_err(|_| CodecError::Limits)?;
    // Keep this buffer alive until after decoder destruction, including errors.
    let mut bytes = Vec::new();
    // SAFETY: each SetInput borrows this live buffer until ReleaseInput or
    // Destroy. Mutation occurs only after ReleaseInput; no pixel buffers or
    // callbacks are installed, and all exits destroy the decoder exactly once.
    // The ICC output uses the checked size from that same live decoder.
    unsafe {
        let decoder = ffi::JxlDecoderCreate(ptr::null());
        if decoder.is_null() {
            return Err(CodecError::Decode);
        }
        let result = (|| {
            if ffi::JxlDecoderSetKeepOrientation(decoder, i32::from(keep_orientation))
                != ffi::JxlDecoderStatus_JXL_DEC_SUCCESS
                || ffi::JxlDecoderSubscribeEvents(
                    decoder,
                    (ffi::JxlDecoderStatus_JXL_DEC_BASIC_INFO
                        | ffi::JxlDecoderStatus_JXL_DEC_COLOR_ENCODING) as i32,
                ) != ffi::JxlDecoderStatus_JXL_DEC_SUCCESS
            {
                return Err(CodecError::Decode);
            }
            let mut basic = None;
            let mut total = 0;
            loop {
                match ffi::JxlDecoderProcessInput(decoder) {
                    ffi::JxlDecoderStatus_JXL_DEC_NEED_MORE_INPUT => {
                        let remaining = ffi::JxlDecoderReleaseInput(decoder);
                        if remaining > bytes.len() {
                            return Err(CodecError::Decode);
                        }
                        let start = bytes.len() - remaining;
                        bytes.copy_within(start.., 0);
                        bytes.truncate(remaining);
                        if total == cap {
                            return Err(CodecError::Limits);
                        }
                        bytes.resize(remaining + (cap - total).min(4096), 0);
                        let n = input
                            .read(&mut bytes[remaining..])
                            .map_err(|_| CodecError::Io)?;
                        if n == 0 {
                            return Err(CodecError::Decode);
                        }
                        total += n;
                        bytes.truncate(remaining + n);
                        if ffi::JxlDecoderSetInput(decoder, bytes.as_ptr(), bytes.len())
                            != ffi::JxlDecoderStatus_JXL_DEC_SUCCESS
                        {
                            return Err(CodecError::Decode);
                        }
                    }
                    ffi::JxlDecoderStatus_JXL_DEC_BASIC_INFO if basic.is_none() => {
                        let mut info = std::mem::MaybeUninit::<ffi::JxlBasicInfo>::uninit();
                        if ffi::JxlDecoderGetBasicInfo(decoder, info.as_mut_ptr())
                            != ffi::JxlDecoderStatus_JXL_DEC_SUCCESS
                        {
                            return Err(CodecError::Decode);
                        }
                        let info = info.assume_init();
                        if !(1..=8).contains(&info.orientation) {
                            return Err(CodecError::Decode);
                        }
                        limits.check(info.xsize, info.ysize, 1)?;
                        // Counting animated frames requires scanning past the
                        // color header; do not silently report one page.
                        if info.have_animation != 0 {
                            return Err(CodecError::Unsupported);
                        }
                        basic = Some(info);
                    }
                    ffi::JxlDecoderStatus_JXL_DEC_COLOR_ENCODING => {
                        let info = basic.as_ref().ok_or(CodecError::Decode)?;
                        let target = ffi::JxlColorProfileTarget_JXL_COLOR_PROFILE_TARGET_DATA;
                        let mut len = 0;
                        #[cfg(libjxl_legacy_icc)]
                        let status = ffi::JxlDecoderGetICCProfileSize(
                            decoder,
                            ptr::null(),
                            target,
                            &mut len,
                        );
                        #[cfg(not(libjxl_legacy_icc))]
                        let status = ffi::JxlDecoderGetICCProfileSize(decoder, target, &mut len);
                        if status != ffi::JxlDecoderStatus_JXL_DEC_SUCCESS {
                            return Err(CodecError::Decode);
                        }
                        if len > 16 << 20 {
                            return Err(CodecError::Limits);
                        }
                        let mut profile = vec![0; len];
                        #[cfg(libjxl_legacy_icc)]
                        let status = ffi::JxlDecoderGetColorAsICCProfile(
                            decoder,
                            ptr::null(),
                            target,
                            profile.as_mut_ptr(),
                            len,
                        );
                        #[cfg(not(libjxl_legacy_icc))]
                        let status = ffi::JxlDecoderGetColorAsICCProfile(
                            decoder,
                            target,
                            profile.as_mut_ptr(),
                            len,
                        );
                        if status != ffi::JxlDecoderStatus_JXL_DEC_SUCCESS {
                            return Err(CodecError::Decode);
                        }
                        return Ok(Identity {
                            kind: Kind::Jxl,
                            width: Some(info.xsize),
                            height: Some(info.ysize),
                            orientation: u8::try_from(info.orientation)
                                .map_err(|_| CodecError::Decode)?,
                            has_icc: len != 0,
                            icc_desc: profile_description(&profile),
                            bits: u8::try_from(info.bits_per_sample)
                                .map_err(|_| CodecError::Decode)?,
                            has_alpha: info.alpha_bits != 0,
                            has_gain_map: false,
                            pages: 1,
                        });
                    }
                    _ => return Err(CodecError::Decode),
                }
            }
        })();
        ffi::JxlDecoderDestroy(decoder);
        result
    }
}

pub(crate) fn rgb_profile(bytes: &[u8]) -> bool {
    // SAFETY: lcms reads only this slice and the profile handle is closed exactly once.
    unsafe {
        let Ok(len) = u32::try_from(bytes.len()) else {
            return false;
        };
        let p = ffi::cmsOpenProfileFromMem(bytes.as_ptr().cast(), len);
        if p.is_null() {
            return false;
        }
        let rgb = ffi::cmsGetColorSpace(p) == 0x5247_4220; // ICC 'RGB ' signature.
        ffi::cmsCloseProfile(p);
        rgb
    }
}

// RGB is returned by libheif after its NCLX matrix/range conversion. The ICC
// describes the remaining RGB primaries and transfer, never a second YCbCr
// conversion. CICP 16 (PQ) and 18 (HLG) explicitly flag HDR transfer functions;
// these profiles normalize PQ's 10000 nit peak / HLG scene white to XYZ Y=100.
// HDR output/gain-map encoding remains unsupported by Encode::hdr.
pub(crate) fn nclx_profile(nclx: crate::bmff::Nclx) -> Result<&'static [u8], CodecError> {
    static PROFILES: [OnceLock<Option<Vec<u8>>>; 5] = [const { OnceLock::new() }; 5];
    if !matches!(nclx.matrix, 0 | 1 | 5 | 6 | 9) {
        return Err(CodecError::Unsupported);
    }
    let index = match (nclx.primaries, nclx.transfer) {
        (1, 13) => 0,
        (1, 1) => 1,
        (12, 13) => 2,
        (9, 16) => 3,
        (9, 18) => 4,
        _ => return Err(CodecError::Unsupported),
    };
    PROFILES[index]
        .get_or_init(|| make_nclx_profile(index))
        .as_deref()
        .ok_or(CodecError::Unsupported)
}

#[allow(clippy::cast_possible_truncation)] // Normalized [0,1] lcms floating tone-curve samples.
fn make_nclx_profile(index: usize) -> Option<Vec<u8>> {
    let xy = |x, y| ffi::cmsCIExyY { x, y, Y: 1.0 };
    let white = xy(0.3127, 0.3290);
    let (red, green, blue, description) = match index {
        0 => (
            xy(0.64, 0.33),
            xy(0.30, 0.60),
            xy(0.15, 0.06),
            c"sRGB (NCLX)",
        ),
        1 => (
            xy(0.64, 0.33),
            xy(0.30, 0.60),
            xy(0.15, 0.06),
            c"BT.709 (NCLX)",
        ),
        2 => (
            xy(0.68, 0.32),
            xy(0.265, 0.69),
            xy(0.15, 0.06),
            c"Display P3 (NCLX)",
        ),
        3 => (
            xy(0.708, 0.292),
            xy(0.170, 0.797),
            xy(0.131, 0.046),
            c"BT.2020 PQ (NCLX)",
        ),
        4 => (
            xy(0.708, 0.292),
            xy(0.170, 0.797),
            xy(0.131, 0.046),
            c"BT.2020 HLG (NCLX)",
        ),
        _ => return None,
    };
    let primaries = ffi::cmsCIExyYTRIPLE {
        Red: red,
        Green: green,
        Blue: blue,
    };
    let samples: Vec<f32> = (0..4096)
        .map(|i| nclx_linear(index, f64::from(i) / 4095.0) as f32)
        .collect();
    // SAFETY: all structures/samples live through their calls. lcms copies
    // curve/profile data; each successful allocation is freed exactly once,
    // including error paths. SaveProfileToMem receives its queried buffer size.
    unsafe {
        let curve = ffi::cmsBuildTabulatedToneCurveFloat(ptr::null_mut(), 4096, samples.as_ptr());
        if curve.is_null() {
            return None;
        }
        let curves = [curve; 3];
        let profile = ffi::cmsCreateRGBProfile(&white, &primaries, curves.as_ptr());
        ffi::cmsFreeToneCurve(curve);
        if profile.is_null() {
            return None;
        }
        let info = ffi::cmsMLUalloc(ptr::null_mut(), 1);
        let mut ok = !info.is_null();
        if ok {
            ok = ffi::cmsMLUsetASCII(info, c"en".as_ptr(), c"US".as_ptr(), description.as_ptr())
                != 0
                && ffi::cmsWriteTag(
                    profile,
                    ffi::cmsTagSignature_cmsSigProfileDescriptionTag,
                    info.cast(),
                ) != 0;
            ffi::cmsMLUfree(info);
        }
        let mut len = 0;
        ok = ok && ffi::cmsSaveProfileToMem(profile, ptr::null_mut(), &mut len) != 0;
        let mut bytes = if ok && len > 0 && len <= 1 << 20 {
            vec![0; len as usize]
        } else {
            Vec::new()
        };
        ok = ok
            && !bytes.is_empty()
            && ffi::cmsSaveProfileToMem(profile, bytes.as_mut_ptr().cast(), &mut len) != 0;
        ffi::cmsCloseProfile(profile);
        ok.then_some(bytes)
    }
}

pub(crate) fn nclx_linear(index: usize, value: f64) -> f64 {
    // IEC 61966-2-1 inverse transfer, BT.709 inverse OETF, BT.2100 PQ EOTF
    // and HLG inverse OETF. HLG stays scene-linear; no display OOTF/tone map.
    match index {
        0 | 2 => {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        1 => {
            if value < 0.081 {
                value / 4.5
            } else {
                ((value + 0.099) / 1.099).powf(1.0 / 0.45)
            }
        }
        3 => {
            let p = value.powf(32.0 / 2523.0);
            ((p - 3424.0 / 4096.0).max(0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * p))
                .powf(16384.0 / 2610.0)
        }
        4 => {
            if value <= 0.5 {
                value * value / 3.0
            } else {
                let a = 0.178_832_77f64;
                let b = 1.0 - 4.0 * a;
                let c = 0.5 - a * (4.0 * a).ln();
                (((value - c) / a).exp() + b) / 12.0
            }
        }
        _ => 0.0,
    }
}
