#![allow(clippy::indexing_slicing)] // Fixed-width headers and checked array indices.
//! Bounded HEIF item parsing and CR3 JPEG previews, independent of rawler.
use crate::CodecError;
#[cfg(feature = "raw")]
use crate::Limits;
#[cfg(feature = "raw")]
use std::path::Path;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
};

/// CICP values associated with the primary image's colr/nclx property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nclx {
    pub(crate) primaries: u16,
    pub(crate) transfer: u16,
    pub(crate) matrix: u16,
    pub(crate) full_range: bool,
}

#[derive(Default)]
pub(crate) struct HeifInfo {
    pub(crate) gain_map: bool,
    pub(crate) nclx: Option<Nclx>,
}

const MAX_BOXES: usize = 4096;
const MAX_DEPTH: u8 = 8;
const MAX_METADATA: u64 = 16 << 20;

struct BoxHeader {
    kind: [u8; 4],
    payload: u64,
    end: u64,
    #[cfg(feature = "raw")]
    header: u64,
}

#[derive(Clone, Copy)]
enum Context {
    Top,
    Meta,
    Info,
    References(bool), // 32-bit item ids
    Properties,
    PropertyValues,
}

#[derive(Default)]
struct GainMap {
    primary: Option<u32>,
    items: BTreeMap<u32, [u8; 4]>,
    derived: BTreeMap<u32, [u32; 2]>,
    references: usize,
    signature: bool,
    properties: Vec<Option<Nclx>>,
    associations: BTreeMap<u32, Vec<u16>>,
    association_count: usize,
    ipco: bool,
}

impl Walker {
    fn associations(
        &mut self,
        payload: u64,
        end: u64,
        meta: &mut GainMap,
    ) -> Result<(), CodecError> {
        let header = self.read::<8>(payload, end)?;
        if header[0] > 1 || header[1..3] != [0, 0] || header[3] > 1 {
            return Err(CodecError::Unsupported);
        }
        let entries = u32::from_be_bytes(header[4..8].try_into().map_err(|_| CodecError::Decode)?);
        if entries as usize > MAX_BOXES {
            return Err(CodecError::Limits);
        }
        let mut at = payload + 8;
        for _ in 0..entries {
            let id = self.item_id(at, end, header[0] == 1)?;
            at += if header[0] == 1 { 4 } else { 2 };
            let count = usize::from(self.read::<1>(at, end)?[0]);
            at += 1;
            meta.association_count += count;
            if meta.association_count > MAX_BOXES {
                return Err(CodecError::Limits);
            }
            let mut properties = Vec::with_capacity(count);
            for _ in 0..count {
                let property = if header[3] == 1 {
                    let value = u16::from_be_bytes(self.read(at, end)?) & 0x7fff;
                    at += 2;
                    value
                } else {
                    let value = u16::from(self.read::<1>(at, end)?[0] & 0x7f);
                    at += 1;
                    value
                };
                properties.push(property);
            }
            if id == 0 || meta.associations.insert(id, properties).is_some() {
                return Err(CodecError::Decode);
            }
        }
        if at != end {
            return Err(CodecError::Decode);
        }
        Ok(())
    }
    fn nclx(&mut self, payload: u64, end: u64) -> Result<Option<Nclx>, CodecError> {
        if self.read::<4>(payload, end)? != *b"nclx" {
            return Ok(None);
        }
        let bytes = self.read::<7>(payload + 4, end)?;
        if end - payload != 11 || bytes[6] & 0x7f != 0 {
            return Err(CodecError::Decode);
        }
        Ok(Some(Nclx {
            primaries: u16::from_be_bytes([bytes[0], bytes[1]]),
            transfer: u16::from_be_bytes([bytes[2], bytes[3]]),
            matrix: u16::from_be_bytes([bytes[4], bytes[5]]),
            full_range: bytes[6] & 0x80 != 0,
        }))
    }
    fn version(&mut self, payload: u64, end: u64) -> Result<u8, CodecError> {
        Ok(self.read::<4>(payload, end)?[0])
    }
    fn item_id(&mut self, offset: u64, end: u64, wide: bool) -> Result<u32, CodecError> {
        if wide {
            Ok(u32::from_be_bytes(self.read(offset, end)?))
        } else {
            Ok(u32::from(u16::from_be_bytes(self.read(offset, end)?)))
        }
    }
    fn item(&mut self, payload: u64, end: u64, gain: &mut GainMap) -> Result<(), CodecError> {
        let version = self.version(payload, end)?;
        if !matches!(version, 2 | 3) {
            return Ok(()); // Legacy infe has no item_type.
        }
        let wide = version == 3;
        let id = self.item_id(payload + 4, end, wide)?;
        let at = payload + if wide { 10 } else { 8 }; // id and protection index
        let kind = self.read(at, end)?;
        if id == 0 || gain.items.insert(id, kind).is_some() {
            return Err(CodecError::Decode);
        }
        Ok(())
    }
    fn references(
        &mut self,
        head: &BoxHeader,
        wide: bool,
        gain: &mut GainMap,
    ) -> Result<(), CodecError> {
        let width = if wide { 4 } else { 2 };
        let from = self.item_id(head.payload, head.end, wide)?;
        let count = usize::from(u16::from_be_bytes(
            self.read(head.payload + width, head.end)?,
        ));
        gain.references = gain
            .references
            .checked_add(count)
            .ok_or(CodecError::Limits)?;
        if gain.references > MAX_BOXES {
            return Err(CodecError::Limits);
        }
        let start = head.payload + width + 2;
        if from == 0 || start + count as u64 * width != head.end {
            return Err(CodecError::Decode);
        }
        if &head.kind == b"dimg" && count == 2 {
            let inputs = [
                self.item_id(start, head.end, wide)?,
                self.item_id(start + width, head.end, wide)?,
            ];
            if inputs.contains(&0) || gain.derived.insert(from, inputs).is_some() {
                return Err(CodecError::Decode);
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_lines)] // One bounded property/item dispatch with shared budgets.
    fn heif_chain(
        &mut self,
        mut offset: u64,
        end: u64,
        depth: u8,
        context: Context,
        gain: &mut GainMap,
        info: &mut HeifInfo,
    ) -> Result<usize, CodecError> {
        if depth > MAX_DEPTH {
            return Err(CodecError::Limits);
        }
        let mut count = 0;
        while offset < end {
            let head = self.header(offset, end)?;
            let BoxHeader {
                kind,
                payload,
                end: box_end,
                ..
            } = head;
            match (context, &kind) {
                (Context::Top, b"meta") => {
                    self.metadata_remaining = self
                        .metadata_remaining
                        .checked_sub(box_end - payload)
                        .ok_or(CodecError::Limits)?;
                    // Read metadata once, then parse checked slices. Walking
                    // every small item/property with seek+read regresses header
                    // latency; mdat remains opaque and is never buffered.
                    let mut bytes = vec![
                        0;
                        usize::try_from(box_end - payload)
                            .map_err(|_| CodecError::Limits)?
                    ];
                    self.file
                        .seek(SeekFrom::Start(payload))
                        .map_err(|_| CodecError::Io)?;
                    self.file
                        .read_exact(&mut bytes)
                        .map_err(|_| CodecError::Decode)?;
                    self.metadata = Some((payload, bytes));
                    if self.version(payload, box_end)? != 0 {
                        return Err(CodecError::Decode);
                    }
                    // Each meta has its own item-id namespace.
                    let mut meta = GainMap::default();
                    self.heif_chain(
                        payload + 4,
                        box_end,
                        depth + 1,
                        Context::Meta,
                        &mut meta,
                        info,
                    )?;
                    info.gain_map |= meta.found();
                    if let Some(nclx) = meta.primary_nclx()?
                        && info.nclx.replace(nclx).is_some()
                    {
                        return Err(CodecError::Unsupported);
                    }
                    self.metadata = None;
                }
                (Context::Meta, b"pitm") => {
                    let version = self.version(payload, box_end)?;
                    if version > 1 || gain.primary.is_some() {
                        return Err(CodecError::Decode);
                    }
                    gain.primary = Some(self.item_id(payload + 4, box_end, version == 1)?);
                }
                (Context::Meta, b"iinf") => {
                    let version = self.version(payload, box_end)?;
                    if version > 1 {
                        return Err(CodecError::Decode);
                    }
                    let entries = self.item_id(payload + 4, box_end, version == 1)?;
                    if entries as usize > MAX_BOXES {
                        return Err(CodecError::Limits);
                    }
                    let start = payload + if version == 0 { 6 } else { 8 };
                    if self.heif_chain(start, box_end, depth + 1, Context::Info, gain, info)?
                        != entries as usize
                    {
                        return Err(CodecError::Decode);
                    }
                }
                (Context::Info, b"infe") => self.item(payload, box_end, gain)?,
                (Context::Meta, b"iref") => {
                    let version = self.version(payload, box_end)?;
                    if version > 1 {
                        return Err(CodecError::Decode);
                    }
                    self.heif_chain(
                        payload + 4,
                        box_end,
                        depth + 1,
                        Context::References(version == 1),
                        gain,
                        info,
                    )?;
                }
                (Context::References(wide), _) => self.references(&head, wide, gain)?,
                (Context::Meta, b"iprp") => {
                    self.heif_chain(payload, box_end, depth + 1, Context::Properties, gain, info)?;
                }
                (Context::Properties, b"ipco") => {
                    if gain.ipco {
                        return Err(CodecError::Decode);
                    }
                    gain.ipco = true;
                    self.heif_chain(
                        payload,
                        box_end,
                        depth + 1,
                        Context::PropertyValues,
                        gain,
                        info,
                    )?;
                }
                (Context::Properties, b"ipma") => self.associations(payload, box_end, gain)?,
                (Context::PropertyValues, b"colr") => {
                    let nclx = self.nclx(payload, box_end)?;
                    gain.properties.push(nclx);
                }
                (Context::PropertyValues, b"auxC") => {
                    gain.properties.push(None);
                    self.version(payload, box_end)?;
                    let (start, bytes) = self.metadata.as_ref().ok_or(CodecError::Decode)?;
                    let from =
                        usize::try_from(payload + 4 - start).map_err(|_| CodecError::Limits)?;
                    let to = usize::try_from(box_end - start).map_err(|_| CodecError::Limits)?;
                    gain.signature |= crate::headers::has_gain_signature(
                        bytes.get(from..to).ok_or(CodecError::Decode)?,
                    );
                }
                (Context::PropertyValues, _) => gain.properties.push(None),
                _ => {} // Pixel payloads, unknown properties and UUIDs stay opaque.
            }
            offset = box_end;
            count += 1;
        }
        Ok(count)
    }
}
impl GainMap {
    fn primary_nclx(&self) -> Result<Option<Nclx>, CodecError> {
        let mut nclx = None;
        for (&id, properties) in &self.associations {
            for &index in properties {
                if index == 0 {
                    continue;
                } // No property associated.
                let property = self
                    .properties
                    .get(usize::from(index - 1))
                    .ok_or(CodecError::Decode)?;
                if Some(id) == self.primary
                    && let Some(profile) = property
                    && nclx.replace(*profile).is_some()
                {
                    return Err(CodecError::Unsupported);
                }
            }
        }
        Ok(nclx)
    }
    fn found(&self) -> bool {
        self.signature
            || self.primary.is_some_and(|primary| {
                self.derived.iter().any(|(id, inputs)| {
                    self.items.get(id) == Some(b"tmap")
                        && *id != primary
                        && inputs[0] == primary
                        && inputs[1] != primary
                        && inputs[1] != *id
                        && self.items.contains_key(&primary)
                        && self.items.contains_key(&inputs[1])
                })
            })
    }
}

pub(crate) fn gain_map(file: File) -> Result<bool, CodecError> {
    Ok(heif_info(file)?.gain_map)
}

pub(crate) fn heif_info(file: File) -> Result<HeifInfo, CodecError> {
    let len = file.metadata().map_err(|_| CodecError::Io)?.len();
    let mut walker = Walker {
        file,
        remaining: MAX_BOXES,
        metadata_remaining: MAX_METADATA,
        metadata: None,
        #[cfg(feature = "raw")]
        seen: [false; 3],
        #[cfg(feature = "raw")]
        limits: Limits::default(),
        #[cfg(feature = "raw")]
        preview: None,
        #[cfg(feature = "raw")]
        jpeg_budget: MAX_METADATA,
        #[cfg(feature = "raw")]
        preview_count: 0,
    };
    if len < 8 || walker.read::<4>(4, len)? != *b"ftyp" {
        return Ok(HeifInfo::default());
    }
    let mut gain = GainMap::default();
    let mut info = HeifInfo::default();
    walker.heif_chain(0, len, 0, Context::Top, &mut gain, &mut info)?;
    Ok(info)
}

struct Walker {
    file: File,
    remaining: usize,
    metadata_remaining: u64,
    metadata: Option<(u64, Vec<u8>)>,
    #[cfg(feature = "raw")]
    seen: [bool; 3],
    #[cfg(feature = "raw")]
    limits: Limits,
    #[cfg(feature = "raw")]
    preview: Option<crate::raw::Preview>,
    #[cfg(feature = "raw")]
    jpeg_budget: u64,
    #[cfg(feature = "raw")]
    preview_count: u8,
}
impl Walker {
    fn header(&mut self, offset: u64, end: u64) -> Result<BoxHeader, CodecError> {
        self.remaining = self.remaining.checked_sub(1).ok_or(CodecError::Limits)?;
        let head = self.read::<8>(offset, end)?;
        let (size, header) =
            match u32::from_be_bytes(head[..4].try_into().map_err(|_| CodecError::Decode)?) {
                0 => return Err(CodecError::Decode),
                1 => (u64::from_be_bytes(self.read(offset + 8, end)?), 16),
                size => (u64::from(size), 8),
            };
        let kind = head[4..8].try_into().map_err(|_| CodecError::Decode)?;
        let header_len = header + if &kind == b"uuid" { 16 } else { 0 };
        let box_end = offset
            .checked_add(size)
            .filter(|n| *n <= end)
            .ok_or(CodecError::Decode)?;
        if size < header_len || box_end <= offset {
            return Err(CodecError::Decode);
        }
        Ok(BoxHeader {
            kind,
            payload: offset + header_len,
            end: box_end,
            #[cfg(feature = "raw")]
            header,
        })
    }
    /// Top-level header for video containers. `QuickTime` and ISO BMFF allow the last
    /// top-level box to declare size 0, meaning it extends to the end of the file;
    /// nested boxes never may, so this is only called for top-level walks.
    fn top_level_header(&mut self, offset: u64, file_len: u64) -> Result<BoxHeader, CodecError> {
        let head = self.read::<8>(offset, file_len)?;
        if u32::from_be_bytes(head[..4].try_into().map_err(|_| CodecError::Decode)?) != 0 {
            return self.header(offset, file_len);
        }
        self.remaining = self.remaining.checked_sub(1).ok_or(CodecError::Limits)?;
        let kind: [u8; 4] = head[4..8].try_into().map_err(|_| CodecError::Decode)?;
        if &kind == b"uuid" || file_len.checked_sub(offset).is_none_or(|n| n < 8) {
            return Err(CodecError::Decode);
        }
        Ok(BoxHeader {
            kind,
            payload: offset + 8,
            end: file_len,
            #[cfg(feature = "raw")]
            header: 8,
        })
    }
    fn read<const N: usize>(&mut self, offset: u64, end: u64) -> Result<[u8; N], CodecError> {
        if offset.checked_add(N as u64).is_none_or(|n| n > end) {
            return Err(CodecError::Decode);
        }
        if let Some((start, bytes)) = &self.metadata
            && let Some(at) = offset
                .checked_sub(*start)
                .and_then(|at| usize::try_from(at).ok())
            && let Some(bytes) = bytes.get(at..at + N)
        {
            return bytes.try_into().map_err(|_| CodecError::Decode);
        }
        let mut bytes = [0; N];
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| CodecError::Io)?;
        self.file
            .read_exact(&mut bytes)
            .map_err(|_| CodecError::Decode)?;
        Ok(bytes)
    }
    #[cfg(feature = "raw")]
    fn jpeg(&mut self, payload: u64, end: u64, thumbnail: bool) -> Result<(), CodecError> {
        // Canon PRVW and THMB v0 have 16-byte prefixes before the JPEG.
        // PRVW: flags(4), type(2), width(2), height(2), type(2), length(4).
        // THMB: flags(4), width(2), height(2), length(4), reserved(4).
        // Layout: https://github.com/Exiv2/exiv2/blob/main/src/bmffimage.cpp
        let head = self.read::<16>(payload, end)?;
        if head[0] != 0 {
            return Ok(()); // HDR/unknown versions are not JPEG previews.
        }
        self.preview_count += 1;
        if self.preview_count > 64 {
            return Err(CodecError::Limits);
        }
        let at = if thumbnail { 4 } else { 6 };
        let width = u32::from(u16::from_be_bytes([head[at], head[at + 1]]));
        let height = u32::from(u16::from_be_bytes([head[at + 2], head[at + 3]]));
        self.limits.check(width, height, 1)?;
        let at = if thumbnail { 8 } else { 12 };
        let len = u64::from(u32::from_be_bytes(
            head[at..at + 4]
                .try_into()
                .map_err(|_| CodecError::Decode)?,
        ));
        let start = payload + 16;
        if len < 4 || len > end - start {
            return Err(CodecError::Decode);
        }
        // Bound the one encoded buffer retained for libvips, independently of
        // the file's (potentially much larger) sensor payload.
        if len > self.limits.max_bytes.min(64 << 20) {
            return Err(CodecError::Limits);
        }
        if self.read::<2>(start + len - 2, end)? != [0xff, 0xd9] {
            return Err(CodecError::Decode);
        }
        self.file
            .seek(SeekFrom::Start(start))
            .map_err(|_| CodecError::Io)?;
        let (w, h, _) = crate::headers::jpeg_frame(
            (&mut self.file).take(len.min(64 << 10)),
            &self.limits,
            &mut self.jpeg_budget,
        )?;
        if (width, height) != (w, h) {
            return Err(CodecError::Mismatch);
        }
        let pixels = u64::from(w) * u64::from(h);
        if self
            .preview
            .as_ref()
            .is_none_or(|p| pixels > u64::from(p.width) * u64::from(p.height))
        {
            self.preview = Some(crate::raw::Preview {
                offset: start,
                len,
                width: w,
                height: h,
            });
        }
        Ok(())
    }
    #[cfg(feature = "raw")]
    fn file_type(&mut self, offset: u64, payload: u64, end: u64) -> Result<(), CodecError> {
        if offset != 0 || payload != 8 || end < 16 || !end.is_multiple_of(4) {
            return Err(CodecError::Decode);
        }
        // Check the actual ftyp extent, not bytes from later boxes.
        if end > 4096 {
            return Err(CodecError::Limits);
        }
        let mut canon = self.read::<4>(payload, end)? == *b"crx ";
        let mut brand = payload + 8; // skip minor version
        while brand < end {
            canon |= self.read::<4>(brand, end)? == *b"crx ";
            brand += 4;
        }
        if !canon {
            return Err(CodecError::Unsupported);
        }
        self.seen[0] = true;
        Ok(())
    }
    #[cfg(feature = "raw")]
    fn chain(&mut self, mut offset: u64, end: u64, depth: u8) -> Result<(), CodecError> {
        if depth > MAX_DEPTH {
            return Err(CodecError::Limits);
        }
        let mut sample_entries = [false; 2];
        while offset < end {
            let BoxHeader {
                kind,
                payload,
                end: box_end,
                header,
            } = self.header(offset, end)?;
            let size = box_end - offset;
            let uuid = &kind == b"uuid";
            if &kind == b"CMP1" {
                // CRX allocation uses CMP1's frame/tile geometry, independently
                // of the 16-bit dimensions in the enclosing CRAW sample entry.
                for at in [payload + 8, payload + 16] {
                    let w = u32::from_be_bytes(self.read(at, box_end)?);
                    let h = u32::from_be_bytes(self.read(at + 4, box_end)?);
                    self.limits.check(w, h, 1)?;
                }
            }
            let uuid_value = if uuid {
                self.read::<16>(offset + header, box_end)?
            } else {
                [0; 16]
            };
            let descriptor = uuid
                && uuid_value
                    == [
                        0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce,
                        0x46, 0x2b, 0x6a, 0x48,
                    ];
            let preview = uuid
                && uuid_value
                    == [
                        0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc,
                        0x40, 0x6e, 0x4d, 0x16,
                    ];
            if depth == 0 {
                if offset == 0 && &kind != b"ftyp" {
                    return Err(CodecError::Unsupported);
                }
                match &kind {
                    b"ftyp" => self.file_type(offset, payload, box_end)?,
                    b"moov" => {
                        if self.seen[1] {
                            return Err(CodecError::Decode);
                        }
                        if size > MAX_METADATA {
                            return Err(CodecError::Limits);
                        }
                        self.seen[1] = true;
                    }
                    b"mdat" => self.seen[2] = true,
                    _ => {}
                }
            }
            // Keep rejecting ambiguous duplicate sample descriptions.
            for (i, sample_kind) in [b"CRAW", b"CTMD"].iter().enumerate() {
                if &kind == *sample_kind {
                    if sample_entries[i] {
                        return Err(CodecError::Decode);
                    }
                    sample_entries[i] = true;
                }
            }
            if matches!(&kind, b"PRVW" | b"THMB") {
                self.jpeg(payload, box_end, &kind == b"THMB")?;
            }
            // Fixed prefixes are the full-box/sample-entry fields, not boxes.
            // All other payloads (including mdat, TIFF and JPEG) remain opaque.
            let skip = match &kind {
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"dinf" | b"edts" => Some(0),
                b"meta" | b"CDI1" => Some(4),
                b"stsd" | b"dref" => Some(8),
                b"CRAW" => Some(82),
                b"CCTP" => Some(12),
                _ if descriptor => Some(0),
                _ if preview => Some(8), // Canon's version/item-count prefix
                _ => None,
            };
            if let Some(skip) = skip {
                let child = payload
                    .checked_add(skip)
                    .filter(|n| *n <= box_end)
                    .ok_or(CodecError::Decode)?;
                self.chain(child, box_end, depth + 1)?;
            }
            offset = box_end;
        }
        Ok(())
    }
}
#[cfg(feature = "raw")]
pub(crate) fn preview(
    path: &Path,
    limits: &Limits,
) -> Result<Option<crate::raw::Preview>, CodecError> {
    let file = File::open(path).map_err(|_| CodecError::Io)?;
    let len = file.metadata().map_err(|_| CodecError::Io)?.len();
    if len > limits.max_bytes {
        return Err(CodecError::Limits);
    }
    let mut walker = Walker {
        file,
        remaining: MAX_BOXES,
        metadata_remaining: MAX_METADATA,
        metadata: None,
        seen: [false; 3],
        limits: limits.clone(),
        preview: None,
        jpeg_budget: 16 << 20,
        preview_count: 0,
    };
    walker.chain(0, len, 0)?;
    if walker.seen != [true; 3] {
        return Err(CodecError::Decode);
    }
    Ok(walker.preview)
}

/// Recognizes only brands inside the bounded ftyp box, never later payload bytes.
pub(crate) fn brand_kind(mut file: File) -> Result<crate::Kind, CodecError> {
    use crate::{Kind, VideoContainer};
    let len = file.metadata().map_err(|_| CodecError::Io)?.len();
    let mut head = [0; 8];
    file.read_exact(&mut head).map_err(|_| CodecError::Decode)?;
    let size = u64::from(u32::from_be_bytes(
        head[..4].try_into().map_err(|_| CodecError::Decode)?,
    ));
    if &head[4..] != b"ftyp"
        || !(16..=4096).contains(&size)
        || size > len
        || !size.is_multiple_of(4)
    {
        return Err(CodecError::Decode);
    }
    let mut brands = vec![0; usize::try_from(size - 8).map_err(|_| CodecError::Limits)?];
    file.read_exact(&mut brands)
        .map_err(|_| CodecError::Decode)?;
    let brands: Vec<_> = brands
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter_map(|(i, b)| (i != 1).then_some(b))
        .collect();
    if brands.contains(&b"crx ") {
        return Ok(Kind::Raw(crate::RawMake("Canon".into())));
    }
    if brands.iter().any(|b| matches!(*b, b"avif" | b"avis")) {
        return Ok(Kind::Avif);
    }
    if brands.iter().any(|b| {
        matches!(
            *b,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
        )
    }) {
        return Ok(Kind::Heif);
    }
    if brands.contains(&b"qt  ") {
        return Ok(Kind::Video {
            container: VideoContainer::Mov,
        });
    }
    if brands.iter().any(|b| {
        matches!(
            *b,
            b"isom"
                | b"iso2"
                | b"iso3"
                | b"iso4"
                | b"iso5"
                | b"iso6"
                | b"mp41"
                | b"mp42"
                | b"avc1"
                | b"M4V "
        )
    }) {
        return Ok(Kind::Video {
            container: VideoContainer::Mp4,
        });
    }
    Err(CodecError::BadMagic)
}

pub(crate) fn video_dimensions(
    file: File,
    limits: &crate::Limits,
) -> Result<Option<(u32, u32)>, CodecError> {
    let len = file.metadata().map_err(|_| CodecError::Io)?.len();
    let mut walker = Walker {
        file,
        remaining: MAX_BOXES,
        metadata_remaining: MAX_METADATA,
        metadata: None,
        #[cfg(feature = "raw")]
        seen: [false; 3],
        #[cfg(feature = "raw")]
        limits: limits.clone(),
        #[cfg(feature = "raw")]
        preview: None,
        #[cfg(feature = "raw")]
        jpeg_budget: MAX_METADATA,
        #[cfg(feature = "raw")]
        preview_count: 0,
    };
    let mut at = 0;
    let mut first = None;
    while at < len {
        let movie = walker.top_level_header(at, len)?;
        if movie.kind == *b"moov" {
            let mut track_offset = movie.payload;
            while track_offset < movie.end {
                let track = walker.header(track_offset, movie.end)?;
                if track.kind == *b"trak" {
                    let mut field_offset = track.payload;
                    let mut dims = None;
                    let mut video = false;
                    while field_offset < track.end {
                        let field = walker.header(field_offset, track.end)?;
                        if field.kind == *b"tkhd" {
                            let version = walker.read::<1>(field.payload, field.end)?[0];
                            let offset = match version {
                                0 => 76,
                                1 => 88,
                                _ => return Err(CodecError::Decode),
                            };
                            if field.end - field.payload != offset + 8 {
                                return Err(CodecError::Decode);
                            }
                            let width =
                                u32::from_be_bytes(walker.read(field.payload + offset, field.end)?)
                                    >> 16;
                            let height = u32::from_be_bytes(
                                walker.read(field.payload + offset + 4, field.end)?,
                            ) >> 16;
                            dims = Some((width, height));
                        } else if field.kind == *b"mdia" {
                            let mut child_offset = field.payload;
                            while child_offset < field.end {
                                let child = walker.header(child_offset, field.end)?;
                                if child.kind == *b"hdlr" {
                                    video =
                                        walker.read::<4>(child.payload + 8, child.end)? == *b"vide";
                                }
                                child_offset = child.end;
                            }
                        }
                        field_offset = field.end;
                    }
                    if video && first.is_none() {
                        if let Some((width, height)) = dims {
                            limits.check(width, height, 1)?;
                        }
                        first = Some(dims);
                    }
                }
                track_offset = track.end;
            }
        }
        at = movie.end;
    }
    Ok(first.flatten())
}
