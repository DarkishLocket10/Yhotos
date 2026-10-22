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
            entry_budget: 4096,
            // Count inline and overlapping values too: rawler allocates each.
            payload_budget: 256 << 20,
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
    // Shared by normal TIFF and Canon MakerNote IFDs, with one allocation budget.
    fn directory(&mut self, offset: u64) -> Result<(usize, u64), CodecError> {
        if self.ifds.len() == 64 {
            return Err(CodecError::Limits);
        }
        let (count_size, entry_size, pointer_size) = if self.big { (8, 20, 8) } else { (2, 12, 4) };
        let bytes = self.read(offset, count_size)?;
        let count = self.num(&bytes, 0, count_size)?;
        self.entry_budget = self
            .entry_budget
            .checked_sub(count)
            .ok_or(CodecError::Limits)?;
        let entries_start = offset.checked_add(count_size).ok_or(CodecError::Decode)?;
        let entries_bytes = count * entry_size;
        let bytes = self.read(entries_start, entries_bytes + pointer_size)?;
        let mut entries = Vec::new();
        for i in 0..count {
            let start = i * entry_size;
            let value_start = start + if self.big { 12 } else { 8 };
            let entry = Entry {
                tag: u16::try_from(self.num(&bytes, start, 2)?).map_err(|_| CodecError::Decode)?,
                kind: u16::try_from(self.num(&bytes, start + 2, 2)?)
                    .map_err(|_| CodecError::Decode)?,
                count: self.num(&bytes, start + 4, if self.big { 8 } else { 4 })?,
                position: entries_start + value_start,
                value: self.num(&bytes, value_start, pointer_size)?,
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
        Ok((index, self.num(&bytes, entries_bytes, pointer_size)?))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn canon_makernotes(&mut self) -> Result<Vec<usize>, CodecError> {
        let mut indices = Vec::new();
        for i in 0..self.count() {
            let Some(entry) = self.entry(i, 37500) else {
                continue;
            };
            // rawler's CR2 decoder parses a classic IFD at the blob's absolute
            // file offset, using the enclosing TIFF byte order and base zero.
            if self.big {
                return Err(CodecError::Unsupported);
            }
            let (offset, len) = self.range(&entry, entry.size()?)?;
            if len <= 4 {
                return Err(CodecError::Decode);
            }
            // rawler does not follow MakerNote next-IFD or sub-IFD links.
            indices.push(self.directory(offset)?.0);
        }
        Ok(indices)
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
        let start = usize::try_from(start).map_err(|_| CodecError::Decode)?;
        let size = usize::try_from(size).map_err(|_| CodecError::Decode)?;
        let b = bytes
            .get(start..start.checked_add(size).ok_or(CodecError::Decode)?)
            .ok_or(CodecError::Decode)?;
        Ok(if self.little {
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
        let start = if bytes <= if self.big { 8 } else { 4 } {
            e.position
        } else {
            e.value
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
            .map(|i| self.num(&bytes, i * size, size))
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
        let inline = if self.big { 8 } else { 4 };
        if size <= inline {
            // Scalar integers are already in the directory entry. In big endian
            // TIFF, short values occupy its most significant bytes; ignore padding.
            let shift = if self.little { 0 } else { (inline - size) * 8 };
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1u64 << (size * 8)) - 1
            };
            return Ok(Some((e.value >> shift) & mask));
        }
        let bytes = self.read(e.value, size)?;
        Ok(Some(self.num(&bytes, 0, size)?))
    }
    #[cfg(feature = "raw")]
    pub(crate) fn blob(
        &mut self,
        index: usize,
        tag: u16,
        cap: u64,
    ) -> Result<Option<Vec<u8>>, CodecError> {
        let Some(e) = self.entry(index, tag) else {
            return Ok(None);
        };
        if !matches!(e.kind, 1 | 2 | 7) {
            return Err(CodecError::Decode);
        }
        let (offset, len) = self.range(&e, 1)?;
        // Strings can be truncated without allocating their full declared size.
        Ok(Some(self.read(offset, len.min(cap))?))
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
