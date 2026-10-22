//! MPF index parsing confined to its APP2 payload and the containing JPEG file.
use crate::CodecError;

struct Index<'a> {
    bytes: &'a [u8],
    little: bool,
}
impl Index<'_> {
    fn number(&self, start: usize, size: usize) -> Result<u32, CodecError> {
        let bytes = self
            .bytes
            .get(start..start.checked_add(size).ok_or(CodecError::Decode)?)
            .ok_or(CodecError::Decode)?;
        Ok(if self.little {
            bytes.iter().rev().fold(0, |v, b| (v << 8) | u32::from(*b))
        } else {
            bytes.iter().fold(0, |v, b| (v << 8) | u32::from(*b))
        })
    }
}

// base is the file offset of the TIFF byte-order field, immediately after MPF\0.
// Entries are at most 64, non-overlapping, and wholly inside the original file.
pub(crate) fn images(
    bytes: &[u8],
    base: u64,
    file_len: u64,
) -> Result<Vec<(u64, u64)>, CodecError> {
    let index = Index {
        bytes,
        little: match bytes.get(..2) {
            Some(b"II") => true,
            Some(b"MM") => false,
            _ => return Err(CodecError::Decode),
        },
    };
    if index.number(2, 2)? != 42 {
        return Err(CodecError::Decode);
    }
    let directory = index.number(4, 4)? as usize;
    if directory < 8 {
        return Err(CodecError::Decode);
    }
    let count = index.number(directory, 2)? as usize;
    if count > 64 {
        return Err(CodecError::Limits);
    }
    let end = directory
        .checked_add(2 + count * 12)
        .ok_or(CodecError::Decode)?;
    if index.number(end, 4)? != 0 {
        return Err(CodecError::Decode);
    }
    let (mut total, mut entries) = (None, None);
    for n in 0..count {
        let pos = directory + 2 + n * 12;
        let tag = index.number(pos, 2)?;
        let kind = index.number(pos + 2, 2)?;
        let len = index.number(pos + 4, 4)?;
        let value = index.number(pos + 8, 4)?;
        match tag {
            0xb001 => {
                if total.is_some() || kind != 4 || len != 1 {
                    return Err(CodecError::Decode);
                }
                total = Some(value);
            }
            0xb002 => {
                if entries.is_some() || kind != 7 {
                    return Err(CodecError::Decode);
                }
                entries = Some((value as usize, len));
            }
            _ => {}
        }
    }
    let total = total.ok_or(CodecError::Decode)?;
    if total == 0 || total > 64 {
        return Err(CodecError::Limits);
    }
    let (start, len) = entries.ok_or(CodecError::Decode)?;
    if start < end + 4 || len != total * 16 {
        return Err(CodecError::Decode);
    }
    if start
        .checked_add(len as usize)
        .is_none_or(|n| n > bytes.len())
    {
        return Err(CodecError::Decode);
    }
    let mut images = Vec::new();
    for n in 0..total as usize {
        let pos = start + n * 16;
        let attributes = index.number(pos, 4)?;
        if attributes & 0x0700_0000 != 0 {
            return Err(CodecError::Decode);
        } // JPEG only.
        let size = u64::from(index.number(pos + 4, 4)?);
        let relative = u64::from(index.number(pos + 8, 4)?);
        let offset = if n == 0 {
            if relative != 0 {
                return Err(CodecError::Decode);
            }
            0
        } else {
            if relative == 0 {
                return Err(CodecError::Decode);
            }
            base.checked_add(relative).ok_or(CodecError::Decode)?
        };
        let end = offset
            .checked_add(size)
            .filter(|n| *n <= file_len)
            .ok_or(CodecError::Decode)?;
        if size < 4 || images.iter().any(|&(a, b)| offset < b && end > a) {
            return Err(CodecError::Decode);
        }
        if n == 0 && end < base + bytes.len() as u64 {
            return Err(CodecError::Decode);
        }
        images.push((offset, end));
    }
    Ok(images)
}
