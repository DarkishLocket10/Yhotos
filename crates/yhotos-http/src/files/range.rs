//! Bounded single byte-range parsing, shared verbatim with the fuzz target.

/// A byte offset and length; `parse` guarantees a nonempty slice within the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    /// Inclusive first byte.
    pub start: u64,
    /// Number of bytes; parsed ranges guarantee `len > 0` and `start + len <= total`.
    pub len: u64,
}

/// Malformed, multiple, overflowing or unsatisfiable ranges are indistinguishable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRange;

/// Parses exactly one RFC byte range without overflow or allocation.
///
/// # Errors
/// Rejects unknown units, multiple ranges, empty files and invalid/unsatisfiable bounds.
pub fn parse(value: &str, total: u64) -> Result<ByteRange, InvalidRange> {
    if value.len() > 64 || total == 0 {
        return Err(InvalidRange);
    }
    let (first, last) = value
        .strip_prefix("bytes=")
        .and_then(|s| s.split_once('-'))
        .ok_or(InvalidRange)?;
    let number = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(InvalidRange);
        }
        s.parse::<u64>().map_err(|_| InvalidRange)
    };
    if first.is_empty() {
        let len = number(last)?.min(total);
        if len == 0 {
            return Err(InvalidRange);
        }
        return Ok(ByteRange {
            start: total - len,
            len,
        });
    }
    let start = number(first)?;
    if start >= total {
        return Err(InvalidRange);
    }
    let end = if last.is_empty() {
        total - 1
    } else {
        number(last)?.min(total - 1)
    };
    if end < start {
        return Err(InvalidRange);
    }
    Ok(ByteRange {
        start,
        len: end - start + 1,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn bounded_ranges_and_extreme_arithmetic() {
        for (text, start, len) in [
            ("bytes=0-99", 0, 100),
            ("bytes=99-", 99, 101),
            ("bytes=-10", 190, 10),
            ("bytes=-999", 0, 200),
            ("bytes=199-999", 199, 1),
        ] {
            assert_eq!(parse(text, 200), Ok(ByteRange { start, len }));
        }
        for text in [
            "bytes=5-2",
            "bytes=0-0,2-3",
            "bytes=200-",
            "bytes=-0",
            "bytes=-",
            "items=0-1",
            "bytes=+0-1",
            "bytes= 0-1",
            "bytes=0-18446744073709551616",
        ] {
            assert!(parse(text, 200).is_err(), "{text}");
        }
        assert!(parse("bytes=0-0", 0).is_err());
        assert_eq!(parse("bytes=0-", u64::MAX).unwrap().len, u64::MAX);
        assert_eq!(
            parse("bytes=18446744073709551614-", u64::MAX).unwrap().len,
            1
        );
    }
}
