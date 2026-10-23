#![allow(clippy::indexing_slicing)] // Fixed-size header fields and checked read lengths.
//! Cheap bounded checks before dispatching to any decoder.
use crate::{CodecError, Kind, Limits};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};
pub(crate) fn inspect(path: &Path, limits: &Limits) -> Result<Kind, CodecError> {
    let mut file = File::open(path).map_err(|_| CodecError::Io)?;
    let meta = file.metadata().map_err(|_| CodecError::Io)?;
    if !meta.is_file() {
        return Err(CodecError::Io);
    }
    if meta.len() > limits.max_bytes {
        return Err(CodecError::Limits);
    }
    let mut head = [0; 64];
    let len = file.read(&mut head).map_err(|_| CodecError::Io)?;
    let h = head.get(..len).ok_or(CodecError::Decode)?;
    let kind = if h.starts_with(b"\xff\xd8\xff") {
        Kind::Jpeg
    } else if h.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(
            h.get(16..20)
                .ok_or(CodecError::Decode)?
                .try_into()
                .map_err(|_| CodecError::Decode)?,
        );
        let height = u32::from_be_bytes(
            h.get(20..24)
                .ok_or(CodecError::Decode)?
                .try_into()
                .map_err(|_| CodecError::Decode)?,
        );
        limits.check(width, height, 1)?;
        Kind::Png
    } else if h.starts_with(b"II*\0")
        || h.starts_with(b"MM\0*")
        || h.starts_with(b"II+\0")
        || h.starts_with(b"MM\0+")
    {
        let raw = tiff_raw(&mut file, meta.len(), limits)?;
        if h.get(8..12) == Some(b"CR\x02\0") || raw {
            Kind::Raw(crate::RawMake(String::new()))
        } else {
            Kind::Tiff
        }
    } else if h.starts_with(b"\xff\x0a") || h.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        Kind::Jxl
    } else if h.starts_with(b"RIFF") && h.get(8..12) == Some(b"WEBP") {
        Kind::Webp
    } else if h.get(4..8) == Some(b"ftyp") {
        crate::bmff::brand_kind(File::open(path).map_err(|_| CodecError::Io)?)?
    } else if h.starts_with(b"IIU\0")
        || h.starts_with(b"IIRO")
        || h.starts_with(b"MMOR")
        || h.starts_with(b"FUJIFILMCCD-RAW")
    {
        Kind::Raw(crate::RawMake(String::new()))
    } else {
        return Err(CodecError::BadMagic);
    };
    // Content-addressed originals have no extension. If a known extension is present it must agree.
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let expected = match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Kind::Jpeg),
            "png" => Some(Kind::Png),
            "tif" | "tiff" => Some(Kind::Tiff),
            "avif" => Some(Kind::Avif),
            "jxl" => Some(Kind::Jxl),
            "webp" => Some(Kind::Webp),
            _ => None,
        };
        if expected.is_some_and(|k| k != kind) {
            return Err(CodecError::Mismatch);
        }
    }
    if kind == Kind::Jpeg {
        if meta.len() < 4 {
            return Err(CodecError::Decode);
        }
        file.seek(SeekFrom::End(-2)).map_err(|_| CodecError::Io)?;
        let mut end = [0; 2];
        file.read_exact(&mut end).map_err(|_| CodecError::Io)?;
        if end != [0xff, 0xd9] {
            return Err(CodecError::Mismatch);
        }
    }
    Ok(kind)
}
// Classic TIFF camera formats identify themselves through DNG/CFA tags, including
// nested SubIFDs. Do not rely on a filename: stored originals are extensionless.
fn tiff_raw(file: &mut File, len: u64, limits: &Limits) -> Result<bool, CodecError> {
    let tiff = crate::tiff::Tiff::from_file(file, len, limits)?;
    Ok((0..tiff.count()).any(|i| [50706, 33421, 33422].iter().any(|tag| tiff.has(i, *tag))))
}
pub(crate) fn has_gain_signature(bytes: &[u8]) -> bool {
    let signatures: &[&[u8]] = &[
        b"urn:iso:std:iso:ts:21496",
        b"hdrgm:",
        b"http://ns.adobe.com/hdr-gain-map/1.0/",
        b"HDRGainMap",
        b"urn:com:apple:photo:2020:aux:hdrgainmap",
        b"urn:com:apple:photo:2020:aux:hdr-gain-map",
    ];
    signatures
        .iter()
        .any(|s| bytes.windows(s.len()).any(|w| w == *s))
}
pub(crate) fn gain_map(path: &Path) -> Result<bool, CodecError> {
    let mut file = File::open(path).map_err(|_| CodecError::Io)?;
    let mut magic = [0; 2];
    file.read_exact(&mut magic)
        .map_err(|_| CodecError::Decode)?;
    if magic == [0xff, 0xd8] {
        let file_len = file.metadata().map_err(|_| CodecError::Io)?.len();
        let mut budget = 16 << 20;
        let (mut found, images) = jpeg_gain_headers(&mut file, 0, file_len, true, &mut budget)?;
        for (start, end) in images {
            file.seek(SeekFrom::Start(end - 2))
                .map_err(|_| CodecError::Io)?;
            let mut eoi = [0; 2];
            file.read_exact(&mut eoi).map_err(|_| CodecError::Decode)?;
            if eoi != [0xff, 0xd9] {
                return Err(CodecError::Decode);
            }
            if start != 0 {
                // Only APP1 XMP in indexed auxiliary JPEGs identifies an Apple
                // gain map. Never search scan bytes or the primary MPF table.
                found |= jpeg_gain_headers(&mut file, start, end, false, &mut budget)?.0;
            }
        }
        return Ok(found);
    }
    // The shared BMFF walker seeks past pixel payloads and bounds metadata,
    // nesting, box counts and item references before interpreting gain maps.
    crate::bmff::gain_map(file)
}

// Scan bounded JPEG metadata ranges, without entering entropy data. A single
// budget covers the primary and all MPF auxiliaries, including marker padding.
fn jpeg_gain_headers(
    file: &mut File,
    start: u64,
    end: u64,
    primary: bool,
    budget: &mut u64,
) -> Result<(bool, Vec<(u64, u64)>), CodecError> {
    // The primary caller has already consumed and checked SOI. Auxiliary
    // ranges need their own seek and signature check.
    if !primary {
        file.seek(SeekFrom::Start(start))
            .map_err(|_| CodecError::Io)?;
        let mut soi = [0; 2];
        file.read_exact(&mut soi).map_err(|_| CodecError::Decode)?;
        if soi != [0xff, 0xd8] {
            return Err(CodecError::Decode);
        }
    }
    let mut pos = start + 2;
    let mut found = false;
    let mut images = None;
    while pos + 2 <= end {
        let mut marker = [0; 2];
        file.read_exact(&mut marker)
            .map_err(|_| CodecError::Decode)?;
        pos += 2;
        *budget = budget.checked_sub(2).ok_or(CodecError::Limits)?;
        if marker[0] != 0xff {
            return Err(CodecError::Decode);
        }
        while marker[1] == 0xff {
            if pos == end {
                return Err(CodecError::Decode);
            }
            file.read_exact(&mut marker[1..])
                .map_err(|_| CodecError::Decode)?;
            pos += 1;
            *budget = budget.checked_sub(1).ok_or(CodecError::Limits)?;
        }
        let marker = marker[1];
        if marker == 0xda || marker == 0xd9 {
            return Ok((found, images.unwrap_or_default()));
        }
        if marker == 1 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if pos + 2 > end {
            return Err(CodecError::Decode);
        }
        let mut length = [0; 2];
        file.read_exact(&mut length)
            .map_err(|_| CodecError::Decode)?;
        let length = u16::from_be_bytes(length);
        let len = usize::from(length.checked_sub(2).ok_or(CodecError::Decode)?);
        if pos.checked_add(u64::from(length)).is_none_or(|n| n > end) {
            return Err(CodecError::Decode);
        }
        *budget = budget
            .checked_sub(u64::from(length))
            .ok_or(CodecError::Limits)?;
        let mut payload = vec![0; len];
        file.read_exact(&mut payload)
            .map_err(|_| CodecError::Decode)?;
        if primary && marker == 0xe2 && payload.starts_with(b"MPF\0") {
            if images.is_some() {
                return Err(CodecError::Decode);
            }
            images = Some(crate::mpf::images(&payload[4..], pos + 6, end)?);
        }
        if primary && matches!(marker, 0xe1 | 0xe2) {
            let signatures: &[&[u8]] = &[
                b"urn:iso:std:iso:ts:21496",
                b"hdrgm:",
                b"http://ns.adobe.com/hdr-gain-map/1.0/",
            ];
            found |= signatures
                .iter()
                .any(|s| payload.windows(s.len()).any(|w| w == *s));
        } else if !primary
            && marker == 0xe1
            && let Some(xmp) = payload.strip_prefix(b"http://ns.adobe.com/xap/1.0/\0")
        {
            found |= [
                b"urn:com:apple:photo:2020:aux:hdrgainmap".as_slice(),
                b"urn:com:apple:photo:2020:aux:hdr-gain-map",
            ]
            .iter()
            .any(|s| xmp.windows(s.len()).any(|w| w == *s));
        }
        pos += u64::from(length);
    }
    Err(CodecError::Decode)
}

// Retain metadata through SOS for native header access, never entropy data.
#[cfg(feature = "raw")]
pub(crate) fn jpeg_header<R: Read>(mut reader: R) -> Result<Vec<u8>, CodecError> {
    let mut magic = [0; 2];
    reader
        .read_exact(&mut magic)
        .map_err(|_| CodecError::Decode)?;
    if magic != [0xff, 0xd8] {
        return Err(CodecError::BadMagic);
    }
    let mut bytes = magic.to_vec();
    loop {
        if bytes.len() + 2 > 64 << 10 {
            return Err(CodecError::Limits);
        }
        let mut marker = [0; 2];
        reader
            .read_exact(&mut marker)
            .map_err(|_| CodecError::Decode)?;
        if marker[0] != 0xff {
            return Err(CodecError::Decode);
        }
        bytes.extend(marker);
        while marker[1] == 0xff {
            if bytes.len() >= 64 << 10 {
                return Err(CodecError::Limits);
            }
            reader
                .read_exact(&mut marker[1..])
                .map_err(|_| CodecError::Decode)?;
            bytes.push(marker[1]);
        }
        if marker[1] == 0xd9 {
            return Ok(bytes);
        }
        if marker[1] == 1 || (0xd0..=0xd7).contains(&marker[1]) {
            continue;
        }
        let mut size = [0; 2];
        reader
            .read_exact(&mut size)
            .map_err(|_| CodecError::Decode)?;
        let len = usize::from(
            u16::from_be_bytes(size)
                .checked_sub(2)
                .ok_or(CodecError::Decode)?,
        );
        if bytes.len() + 2 + len > 64 << 10 {
            return Err(CodecError::Limits);
        }
        bytes.extend(size);
        let start = bytes.len();
        bytes.resize(start + len, 0);
        reader
            .read_exact(bytes.get_mut(start..).ok_or(CodecError::Decode)?)
            .map_err(|_| CodecError::Decode)?;
        if marker[1] == 0xda {
            return Ok(bytes);
        } // Stop before entropy data.
    }
}

#[cfg(feature = "raw")]
// Read only JPEG marker headers, stopping at SOF without entering entropy data.
pub(crate) fn jpeg_dimensions(bytes: &[u8], limits: &Limits) -> Result<(u32, u32), CodecError> {
    let (w, h, _) = jpeg_frame(bytes, limits, &mut (16 << 20))?;
    Ok((w, h))
}

#[cfg(feature = "raw")]
pub(crate) fn jpeg_frame<R: Read>(
    mut reader: R,
    limits: &Limits,
    budget: &mut u64,
) -> Result<(u32, u32, u32), CodecError> {
    let mut magic = [0; 2];
    reader
        .read_exact(&mut magic)
        .map_err(|_| CodecError::Decode)?;
    if magic != [0xff, 0xd8] {
        return Err(CodecError::BadMagic);
    }
    loop {
        let mut marker = [0; 2];
        reader
            .read_exact(&mut marker)
            .map_err(|_| CodecError::Decode)?;
        *budget = budget.checked_sub(2).ok_or(CodecError::Limits)?;
        if marker[0] != 0xff {
            return Err(CodecError::Decode);
        }
        while marker[1] == 0xff {
            *budget = budget.checked_sub(1).ok_or(CodecError::Limits)?;
            reader
                .read_exact(&mut marker[1..])
                .map_err(|_| CodecError::Decode)?;
        }
        let marker = marker[1];
        if marker == 0xda || marker == 0xd9 {
            return Err(CodecError::Decode);
        }
        if marker == 1 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let mut size = [0; 2];
        reader
            .read_exact(&mut size)
            .map_err(|_| CodecError::Decode)?;
        let size = u16::from_be_bytes(size);
        *budget = budget
            .checked_sub(u64::from(size))
            .ok_or(CodecError::Limits)?;
        let len = usize::from(size.checked_sub(2).ok_or(CodecError::Decode)?);
        let mut segment = vec![0; len];
        reader
            .read_exact(&mut segment)
            .map_err(|_| CodecError::Decode)?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let dims = segment.get(1..5).ok_or(CodecError::Decode)?;
            let height = u32::from(u16::from_be_bytes([dims[0], dims[1]]));
            let width = u32::from(u16::from_be_bytes([dims[2], dims[3]]));
            limits.check(width, height, 1)?;
            let components = u32::from(*segment.get(5).ok_or(CodecError::Decode)?);
            if !(1..=4).contains(&components) || segment.len() != 6 + components as usize * 3 {
                return Err(CodecError::Decode);
            }
            return Ok((width, height, components));
        }
    }
}

// Structural-only entry point for the walker boundary regressions.
#[cfg(all(test, feature = "raw"))]
pub(crate) fn cr3(path: &Path, limits: &Limits) -> Result<(), CodecError> {
    crate::bmff::preview(path, limits).map(|_| ())
}
