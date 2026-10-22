//! Bounded TIFF/BigTIFF directory reads. No image loader or pixel data is evaluated.
use crate::{CodecError, Limits};
#[cfg(feature = "raw")]
use std::path::Path;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

#[derive(Clone)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u64,
    position: u64,
    value: u64,
    layout: Layout,
}
#[derive(Clone, Copy)]
struct Layout {
    little: bool,
    big: bool,
    base: u64,
}
impl Entry {
    fn size(&self) -> Result<u64, CodecError> {
        match self.kind {
            1 | 2 | 6 | 7 => Ok(1),
            3 | 8 => Ok(2),
            4 | 9 | 11 | 13 => Ok(4),
            5 | 10 | 12 | 16 | 17 | 18 => Ok(8),
            _ => Err(CodecError::Decode),
        }
    }
}
#[derive(PartialEq, Eq)]
pub(crate) enum Flavor {
    Standard,
    Panasonic,
    Canon,
}
pub(crate) struct Tiff<F = File> {
    file: F,
    len: u64,
    little: bool,
    big: bool,
    pub(crate) flavor: Flavor,
    ifds: Vec<Vec<Entry>>,
    entry_budget: u64,
    payload_budget: u64,
}
// A zero-copy, seekable view of CameraIFD's embedded TIFF. Its offsets are
// relative to the blob, and neither directory nor payload reads can escape it.
#[cfg(feature = "raw")]
struct Window<'a, F> {
    file: &'a mut F,
    base: u64,
    len: u64,
    position: u64,
}
#[cfg(feature = "raw")]
impl<F: Read + Seek> Read for Window<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let available = usize::try_from(self.len - self.position).unwrap_or(usize::MAX);
        let size = buffer.len().min(available);
        self.file.seek(SeekFrom::Start(self.base + self.position))?;
        let slice = buffer
            .get_mut(..size)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        let n = self.file.read(slice)?;
        self.position += n as u64;
        Ok(n)
    }
}
#[cfg(feature = "raw")]
impl<F: Seek> Seek for Window<'_, F> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
        };
        if position < 0 || position > i128::from(self.len) {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.position = u64::try_from(position).map_err(std::io::Error::other)?;
        Ok(self.position)
    }
}
#[cfg(feature = "raw")]
impl Tiff<File> {
    pub(crate) fn open(path: &Path, limits: &Limits) -> Result<Self, CodecError> {
        let file = File::open(path).map_err(|_| CodecError::Io)?;
        let len = file.metadata().map_err(|_| CodecError::Io)?.len();
        Self::from_file(file, len, limits)
    }
}
impl<F: Read + Seek> Tiff<F> {
    // Reuse the sniffer's open file and kernel-reported length on the hot path.
    pub(crate) fn from_file(file: F, len: u64, limits: &Limits) -> Result<Self, CodecError> {
        Self::with_budgets(file, len, limits, 4096, 256 << 20)
    }
    fn with_budgets(
        file: F,
        len: u64,
        limits: &Limits,
        entry_budget: u64,
        payload_budget: u64,
    ) -> Result<Self, CodecError> {
        if len > limits.max_bytes {
            return Err(CodecError::Limits);
        }
        let mut t = Self {
            file,
            len,
            little: true,
            big: false,
            flavor: Flavor::Standard,
            ifds: Vec::new(),
            entry_budget,
            // Count inline and overlapping values too: rawler allocates each.
            payload_budget,
        };
        let header = t.read(0, len.min(16))?;
        t.little = match header.get(..2) {
            Some(b"II") => true,
            Some(b"MM") => false,
            _ => return Err(CodecError::Unsupported),
        };
        let magic = t.num(&header, 2, 2)?;
        t.big = magic == 43;
        t.flavor = if magic == 85 {
            Flavor::Panasonic
        } else if magic == 42 && header.get(8..12) == Some(b"CR\x02\0") {
            Flavor::Canon
        } else {
            Flavor::Standard
        };
        if !matches!(magic, 42 | 43 | 85 | 0x4f52 | 0x5352) {
            return Err(CodecError::Unsupported);
        }
        let first = if t.big {
            if t.num(&header, 4, 2)? != 8 || t.num(&header, 6, 2)? != 0 {
                return Err(CodecError::Decode);
            }
            let offset = t.read(8, 8)?;
            t.num(&offset, 0, 8)?
        } else {
            t.num(&header, 4, 4)?
        };
        let mut pending = vec![first];
        let mut visited = Vec::new();
        while let Some(offset) = pending.pop() {
            if offset == 0 {
                continue;
            }
            if visited.contains(&offset) {
                return Err(CodecError::Decode);
            }
            if visited.len() == 64 {
                return Err(CodecError::Limits);
            }
            visited.push(offset);
            let (index, next) = t.directory(offset)?;
            if let Some((w, h)) = t.dimensions(index)? {
                limits.check(w, h, 1)?;
            }
            pending.push(next);
            pending.extend(t.links(index)?);
        }
        if t.ifds.is_empty() {
            return Err(CodecError::Decode);
        }
        Ok(t)
    }
    #[cfg(feature = "raw")]
    pub(crate) fn panasonic_camera_ifds(&mut self, limits: &Limits) -> Result<(), CodecError> {
        for i in 0..self.count() {
            let Some((base, len)) = self.blob_range(i, 0x0120)? else {
                continue;
            };
            let embedded = Tiff::with_budgets(
                Window {
                    file: &mut self.file,
                    base,
                    len,
                    position: 0,
                },
                len,
                limits,
                self.entry_budget,
                self.payload_budget,
            )?;
            self.entry_budget = embedded.entry_budget;
            self.payload_budget = embedded.payload_budget;
        }
        Ok(())
    }
    // Shared by normal TIFF and all MakerNote IFDs, with one allocation budget.
    fn directory(&mut self, offset: u64) -> Result<(usize, u64), CodecError> {
        self.directory_at(
            offset,
            Layout {
                little: self.little,
                big: self.big,
                base: 0,
            },
            self.len,
        )
    }
    fn directory_at(
        &mut self,
        offset: u64,
        layout: Layout,
        end: u64,
    ) -> Result<(usize, u64), CodecError> {
        if self.ifds.len() == 64 {
            return Err(CodecError::Limits);
        }
        let (count_size, entry_size, pointer_size) =
            if layout.big { (8, 20, 8) } else { (2, 12, 4) };
        if offset.checked_add(count_size).is_none_or(|n| n > end) {
            return Err(CodecError::Decode);
        }
        let bytes = self.read(offset, count_size)?;
        let num = |bytes: &[u8], start, size| Self::integer(bytes, start, size, layout.little);
        let count = num(&bytes, 0, count_size)?;
        self.entry_budget = self
            .entry_budget
            .checked_sub(count)
            .ok_or(CodecError::Limits)?;
        let entries_start = offset.checked_add(count_size).ok_or(CodecError::Decode)?;
        let entries_bytes = count * entry_size;
        if entries_start
            .checked_add(entries_bytes + pointer_size)
            .is_none_or(|n| n > end)
        {
            return Err(CodecError::Decode);
        }
        let bytes = self.read(entries_start, entries_bytes + pointer_size)?;
        let mut entries = Vec::new();
        for i in 0..count {
            let start = i * entry_size;
            let value_start = start + if layout.big { 12 } else { 8 };
            let entry = Entry {
                tag: u16::try_from(num(&bytes, start, 2)?).map_err(|_| CodecError::Decode)?,
                kind: u16::try_from(num(&bytes, start + 2, 2)?).map_err(|_| CodecError::Decode)?,
                count: num(&bytes, start + 4, if layout.big { 8 } else { 4 })?,
                position: entries_start + value_start,
                value: num(&bytes, value_start, pointer_size)?,
                layout,
            };
            let (_, len) = self.range(&entry, entry.size()?)?;
            self.payload_budget = self
                .payload_budget
                .checked_sub(len)
                .ok_or(CodecError::Limits)?;
            entries.push(entry);
        }
        entries.sort_unstable_by_key(|e| e.tag);
        if entries
            .iter()
            .zip(entries.iter().skip(1))
            .any(|(a, b)| a.tag == b.tag)
        {
            return Err(CodecError::Decode);
        }
        let index = self.ifds.len();
        self.ifds.push(entries);
        Ok((index, num(&bytes, entries_bytes, pointer_size)?))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn makernotes(&mut self, make: &str) -> Result<Vec<usize>, CodecError> {
        // DNG dispatch in rawler precedes vendor dispatch and never parses
        // MakerNote IFDs (including Apple ProRAW notes). All entry extents and
        // declared allocations were already checked by directory_at; keep the
        // blob opaque instead of imposing a vendor IFD layout it does not use.
        if self.has(0, 50706) {
            return Ok(Vec::new());
        }
        // These TIFF decoders do not parse EXIF MakerNote IFDs in rawler
        // 0.8.0. IIQ/DCR use separate vendor directories, not this blob.
        // Kodak's DCS560C model is dispatched to Canon before Make dispatch.
        if matches!(
            make,
            "Panasonic"
                | "LEICA"
                | "LEICA CAMERA AG"
                | "Hasselblad"
                | "Phase One"
                | "Phase One A/S"
                | "Leaf"
                | "Mamiya-OP Co.,Ltd."
                | "KODAK"
                | "Kodak"
        ) && self
            .blob(0, 272, 256)?
            .is_none_or(|model| model.strip_suffix(&[0]).unwrap_or(&model) != b"DCS560C")
        {
            return Ok(Vec::new());
        }
        let mut indices = Vec::new();
        for i in 0..self.count() {
            let Some(entry) = self.entry(i, 37500) else {
                continue;
            };
            if self.big || entry.kind != 7 {
                return Err(CodecError::Unsupported);
            }
            let (offset, len) = self.range(&entry, entry.size()?)?;
            if len <= 4 {
                return Err(CodecError::Decode);
            }
            let end = offset.checked_add(len).ok_or(CodecError::Decode)?;
            let header = if make == "Canon" {
                Vec::new()
            } else {
                self.read(offset, len.min(18))?
            };
            let mut layout = Layout {
                little: self.little,
                big: false,
                base: 0,
            };
            let skip = match make {
                "Canon" => 0,
                "SONY" | "Sony" => {
                    // The supported ARW layout is a plain IFD. Do not let
                    // rawler's generic parser reinterpret an unaudited header.
                    if header.starts_with(b"SONY")
                        || header.starts_with(b"II")
                        || header.starts_with(b"MM")
                    {
                        return Err(CodecError::Unsupported);
                    }
                    if len < 10 {
                        return Err(CodecError::Decode);
                    }
                    0
                }
                "Panasonic" if header.starts_with(b"Panasonic\0\0\0") => 12,
                "NIKON" | "Nikon" | "NIKON CORPORATION"
                    if header.starts_with(b"Nikon\0\x02\0\0\0") =>
                {
                    layout.little = match header.get(10..12) {
                        Some(b"II") => true,
                        Some(b"MM") => false,
                        _ => return Err(CodecError::Unsupported),
                    };
                    // rawler 0.8 starts at exactly eight bytes after this TIFF
                    // header. Reject other offsets instead of validating a
                    // different directory from the one it will parse.
                    if Self::integer(&header, 12, 2, layout.little)? != 42
                        || Self::integer(&header, 14, 4, layout.little)? != 8
                    {
                        return Err(CodecError::Unsupported);
                    }
                    layout.base = offset + 10;
                    18
                }
                m if m.starts_with("OLYMPUS") && header.starts_with(b"OLYMPUS\0") => 12,
                m if m.starts_with("OLYMPUS") && header.starts_with(b"OLYMP\0") => 8,
                "FUJIFILM" if header.starts_with(b"FUJIFILM\x0c\0\0\0") => {
                    layout.little = true;
                    layout.base = offset;
                    12
                }
                _ => return Err(CodecError::Unsupported),
            };
            // Bound the directory table by the declared note, even if trailing
            // file bytes could otherwise satisfy a truncated table read. Value
            // offsets retain their vendor base and are checked against EOF.
            let index = self.directory_at(offset + skip, layout, end)?.0;
            indices.push(index);
            if make.starts_with("OLYMPUS") {
                self.olympus_directories(index, offset, skip, layout, end)?;
            }
        }
        Ok(indices)
    }
    #[cfg(feature = "raw")]
    fn olympus_directories(
        &mut self,
        index: usize,
        offset: u64,
        skip: u64,
        layout: Layout,
        end: u64,
    ) -> Result<(), CodecError> {
        // These are the additional directories rawler's ORF constructor
        // parses. Equipment/ImageProcessing offsets are note-relative;
        // 0x3000 is an absolute sub-IFD in its root parser.
        for tag in [0x2010, 0x2040, 0x3000] {
            if tag == 0x2040 && skip != 12 {
                continue;
            }
            let pointers = if tag == 0x3000 {
                let Some(e) = self.entry(index, tag) else {
                    continue;
                };
                match e.kind {
                    4 => self.numbers(index, tag, 64)?,
                    // rawler treats the *location* of an undefined value as
                    // the sub-IFD, even when that value is inline or empty.
                    7 => vec![self.range(&e, 1)?.0],
                    _ => return Err(CodecError::Unsupported),
                }
            } else if let Some(e) = self.entry(index, tag) {
                // ORF reads four raw bytes even for UNDEFINED
                // count=1, rather than using the parsed value type.
                let (at, _) = self.range(&e, e.size()?)?;
                let bytes = self.read(at, 4)?;
                self.payload_budget = self
                    .payload_budget
                    .checked_sub(4)
                    .ok_or(CodecError::Limits)?;
                vec![Self::integer(&bytes, 0, 4, layout.little)?]
            } else {
                Vec::new()
            };
            for pointer in pointers.into_iter().filter(|p| *p != 0) {
                let base = if tag == 0x3000 { 0 } else { offset };
                let at = base.checked_add(pointer).ok_or(CodecError::Decode)?;
                self.directory_at(at, Layout { base, ..layout }, end)?;
            }
        }
        Ok(())
    }
    #[cfg(feature = "raw")]
    pub(crate) fn rawler_fujifilm_makernotes(&mut self) -> Result<(), CodecError> {
        // DNG dispatch does not enter the generic RAF MakerNote helper.
        if self.has(0, 50706) {
            return Ok(());
        }
        // The supported note format is little-endian/note-relative. rawler
        // 0.8's generic RAF helper instead inherits the outer byte order and
        // absolute offsets. Before using it, also budget the exact view it
        // will read; identify/embedded-preview decode need only the normal view.
        let count = self.count();
        self.makernotes("FUJIFILM")?;
        for i in 0..count {
            if let Some(e) = self.entry(i, 37500) {
                let (offset, len) = self.range(&e, e.size()?)?;
                self.directory_at(
                    offset + 12,
                    Layout {
                        little: self.little,
                        big: false,
                        base: 0,
                    },
                    offset + len,
                )?;
            }
        }
        Ok(())
    }
    fn links(&mut self, index: usize) -> Result<Vec<u64>, CodecError> {
        // Follow standard SubIFD, EXIF, GPS and interoperability links, as
        // well as explicitly typed IFD pointers on otherwise unknown tags.
        let tags: Vec<_> = self
            .ifds
            .get(index)
            .ok_or(CodecError::Decode)?
            .iter()
            .filter(|e| matches!(e.tag, 330 | 34665 | 34853 | 40965) || matches!(e.kind, 13 | 18))
            .map(|e| e.tag)
            .collect();
        let mut links = Vec::new();
        for tag in tags {
            links.extend(self.numbers(index, tag, 64)?);
        }
        Ok(links)
    }
    fn num(&self, bytes: &[u8], start: u64, size: u64) -> Result<u64, CodecError> {
        Self::integer(bytes, start, size, self.little)
    }
    fn integer(bytes: &[u8], start: u64, size: u64, little: bool) -> Result<u64, CodecError> {
        let start = usize::try_from(start).map_err(|_| CodecError::Decode)?;
        let size = usize::try_from(size).map_err(|_| CodecError::Decode)?;
        let b = bytes
            .get(start..start.checked_add(size).ok_or(CodecError::Decode)?)
            .ok_or(CodecError::Decode)?;
        Ok(if little {
            b.iter().rev().fold(0, |v, b| (v << 8) | u64::from(*b))
        } else {
            b.iter().fold(0, |v, b| (v << 8) | u64::from(*b))
        })
    }
    fn read(&mut self, offset: u64, size: u64) -> Result<Vec<u8>, CodecError> {
        if offset.checked_add(size).is_none_or(|end| end > self.len) {
            return Err(CodecError::Decode);
        }
        let mut bytes = vec![0; usize::try_from(size).map_err(|_| CodecError::Limits)?];
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| CodecError::Io)?;
        self.file
            .read_exact(&mut bytes)
            .map_err(|_| CodecError::Decode)?;
        Ok(bytes)
    }
    fn entry(&self, index: usize, tag: u16) -> Option<Entry> {
        let entries = self.ifds.get(index)?;
        entries
            .get(entries.binary_search_by_key(&tag, |e| e.tag).ok()?)
            .cloned()
    }
    fn range(&self, e: &Entry, size: u64) -> Result<(u64, u64), CodecError> {
        // rawler uses u32 entry lengths internally, even on 64-bit hosts.
        let count = u32::try_from(e.count).map_err(|_| CodecError::Limits)?;
        let size = u32::try_from(size).map_err(|_| CodecError::Limits)?;
        let bytes = u64::from(count.checked_mul(size).ok_or(CodecError::Limits)?);
        let start = if bytes <= if e.layout.big { 8 } else { 4 } {
            e.position
        } else {
            e.layout
                .base
                .checked_add(e.value)
                .ok_or(CodecError::Decode)?
        };
        if start.checked_add(bytes).is_none_or(|end| end > self.len) {
            return Err(CodecError::Decode);
        }
        Ok((start, bytes))
    }
    pub(crate) fn count(&self) -> usize {
        self.ifds.len()
    }
    pub(crate) fn has(&self, index: usize, tag: u16) -> bool {
        self.entry(index, tag).is_some()
    }
    pub(crate) fn numbers(
        &mut self,
        index: usize,
        tag: u16,
        cap: u64,
    ) -> Result<Vec<u64>, CodecError> {
        let Some(e) = self.entry(index, tag) else {
            return Ok(Vec::new());
        };
        if e.count > cap {
            return Err(CodecError::Limits);
        }
        let size = match e.kind {
            1 => 1,
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => return Err(CodecError::Decode),
        };
        let (offset, len) = self.range(&e, size)?;
        let bytes = self.read(offset, len)?;
        (0..e.count)
            .map(|i| Self::integer(&bytes, i * size, size, e.layout.little))
            .collect()
    }
    pub(crate) fn number(&mut self, index: usize, tag: u16) -> Result<Option<u64>, CodecError> {
        let Some(e) = self.entry(index, tag) else {
            return Ok(None);
        };
        if e.count == 0 {
            return Ok(None);
        }
        if e.count != 1 {
            return Err(CodecError::Limits);
        }
        let size = match e.kind {
            1 => 1,
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => return Err(CodecError::Decode),
        };
        let inline = if e.layout.big { 8 } else { 4 };
        if size <= inline {
            // Scalar integers are already in the directory entry. In big endian
            // TIFF, short values occupy its most significant bytes; ignore padding.
            let shift = if e.layout.little {
                0
            } else {
                (inline - size) * 8
            };
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1u64 << (size * 8)) - 1
            };
            return Ok(Some((e.value >> shift) & mask));
        }
        let (offset, _) = self.range(&e, size)?;
        let bytes = self.read(offset, size)?;
        Ok(Some(Self::integer(&bytes, 0, size, e.layout.little)?))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn blob(
        &mut self,
        index: usize,
        tag: u16,
        cap: u64,
    ) -> Result<Option<Vec<u8>>, CodecError> {
        self.blob_range(index, tag)?
            .map(|(offset, len)| self.read(offset, len.min(cap)))
            .transpose()
    }
    #[cfg(feature = "raw")]
    pub(crate) fn blob_range(
        &self,
        index: usize,
        tag: u16,
    ) -> Result<Option<(u64, u64)>, CodecError> {
        let Some(e) = self.entry(index, tag) else {
            return Ok(None);
        };
        if !matches!(e.kind, 1 | 2 | 7) {
            return Err(CodecError::Decode);
        }
        Ok(Some(self.range(&e, 1)?))
    }
    pub(crate) fn dimensions(&mut self, index: usize) -> Result<Option<(u32, u32)>, CodecError> {
        let pair = self.number(index, 256)?.zip(self.number(index, 257)?);
        pair.map(|(w, h)| {
            Ok((
                u32::try_from(w).map_err(|_| CodecError::Limits)?,
                u32::try_from(h).map_err(|_| CodecError::Limits)?,
            ))
        })
        .transpose()
    }
    #[cfg(feature = "raw")]
    pub(crate) fn data(
        &mut self,
        offset: u64,
        len: u64,
        limits: &Limits,
    ) -> Result<Vec<u8>, CodecError> {
        if len > limits.max_bytes {
            return Err(CodecError::Limits);
        }
        self.read(offset, len)
    }
    // Only the marker prefix is needed to check a sensor codestream. CR2 can
    // omit StripByteCounts, in which case the bounded prefix ends at EOF.
    #[cfg(feature = "raw")]
    pub(crate) fn preview_frame(
        &mut self,
        offset: u64,
        len: u64,
        limits: &Limits,
        budget: &mut u64,
    ) -> Result<(u32, u32, u32), CodecError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(CodecError::Decode);
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| CodecError::Io)?;
        // Buffer small marker reads while bounding the underlying reader,
        // including its read-ahead, to the first 64 KiB of this candidate.
        crate::headers::jpeg_frame(
            std::io::BufReader::with_capacity(4096, (&mut self.file).take(len.min(64 << 10))),
            limits,
            budget,
        )
    }
    #[cfg(feature = "raw")]
    pub(crate) fn sensor_frame(
        &mut self,
        offset: u64,
        len: Option<u64>,
        limits: &Limits,
        budget: &mut u64,
    ) -> Result<(u32, u32, u32), CodecError> {
        let available = self.len.checked_sub(offset).ok_or(CodecError::Decode)?;
        let len = len.unwrap_or(available);
        if len > available {
            return Err(CodecError::Decode);
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| CodecError::Io)?;
        crate::headers::jpeg_frame((&mut self.file).take(len), limits, budget)
    }
}
