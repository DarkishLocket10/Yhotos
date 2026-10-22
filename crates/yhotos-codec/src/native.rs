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
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/native.rs"));
}
pub(crate) const UCHAR: i32 = ffi::VipsBandFormat_VIPS_FORMAT_UCHAR;
pub(crate) const USHORT: i32 = ffi::VipsBandFormat_VIPS_FORMAT_USHORT;
const END: *const std::ffi::c_char = ptr::null();

fn init() -> Result<(), CodecError> {
    static INIT: OnceLock<bool> = OnceLock::new();
    let ok = INIT.get_or_init(|| {
        // SAFETY: process-global initialization is serialized and its static name outlives the call.
        unsafe {
            let status = ffi::vips_init(c"yhotos-codec".as_ptr());
            ffi::vips_concurrency_set(2);
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
        init()?;
        let path = path_string(path)?;
        // Dispatch only to the decoder selected by our magic sniffer. Generic
        // foreign_find_load probes unrelated fallback loaders (including Magick
        // and RAW), which may decode while probing or hang on short JXL input.
        // Each explicit loader independently verifies its own container signature.
        #[cfg(not(feature = "heif"))]
        if matches!(kind, Kind::Heif) {
            return Err(CodecError::Unsupported);
        }
        operation!(|out| match kind {
            Kind::Jpeg => ffi::vips_jpegload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
            Kind::Png => ffi::vips_pngload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
            Kind::Tiff => ffi::vips_tiffload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
            Kind::Heif | Kind::Avif =>
                ffi::vips_heifload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
            Kind::Jxl => ffi::vips_jxlload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
            Kind::Webp => ffi::vips_webpload(path.as_ptr(), out, c"fail_on".as_ptr(), 3i32, END),
            _ => -1,
        })
    }
    #[cfg(feature = "raw")]
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
                if self.string("vips-loader").as_deref() == Some("heifload")
                    && matches!(self.bits(), 10 | 12) =>
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
    mut input: impl std::io::Read,
    limits: &crate::Limits,
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
            if ffi::JxlDecoderSubscribeEvents(
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
                            width: info.xsize,
                            height: info.ysize,
                            // libjxl defaults to applying codestream orientation,
                            // just like the libvips loader used for pixel decode.
                            orientation: 1,
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
