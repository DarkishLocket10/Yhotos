//! Bounded tus parsers shared verbatim with the fuzz harness.
use base64::{Engine, engine::general_purpose::STANDARD};
/// Parsed creation metadata. Values remain untrusted until boundary validation.
#[derive(Debug)]
pub struct Metadata {
    /// Display name only.
    pub filename: String,
    /// Destination album UUID text.
    pub album: String,
    /// Advisory MIME hint.
    pub filetype: String,
}
/// Accepts unsigned decimal lengths representable by PostgreSQL's signed bigint.
#[must_use]
pub fn number(text: &str) -> Option<i64> {
    if text.is_empty() || text.len() > 19 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
/// Accepts bounded base64 UTF-8 metadata, rejecting duplicate and unknown keys.
#[must_use]
pub fn metadata(text: &str) -> Option<Metadata> {
    if text.is_empty() || text.len() > 4096 {
        return None;
    }
    let (mut filename, mut album, mut filetype) = (None, None, None);
    for field in text.split(',') {
        let (key, value) = field.trim().split_once(' ').unwrap_or((field.trim(), ""));
        if value.contains(' ') {
            return None;
        }
        let value = String::from_utf8(STANDARD.decode(value).ok()?).ok()?;
        let slot = match key {
            "filename" => &mut filename,
            "album" => &mut album,
            "filetype" => &mut filetype,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    let filename = filename?;
    let album = album?;
    let filetype = filetype.unwrap_or_default();
    if filename.is_empty()
        || album.len() != 36
        || filetype.len() > 255
        || filetype.chars().any(char::is_control)
    {
        return None;
    }
    Some(Metadata {
        filename,
        album,
        filetype,
    })
}
/// Produces a UTF-8 display name of at most 255 bytes with no path or control characters.
#[must_use]
pub fn filename(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| {
        !c.is_control()
            && !matches!(
                c,
                '/' | '\\' | '\u{202e}' | '\u{202d}' | '\u{2066}'..='\u{2069}'
            )
    }) {
        if out.len() + c.len_utf8() > 255 {
            break;
        }
        out.push(c);
    }
    let out = out.trim().trim_matches('.').trim().to_owned();
    if out.is_empty() { "upload".into() } else { out }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_numbers_and_metadata_abuse() {
        for s in ["", "-1", "+1", "1.0", " 1", "9223372036854775808"] {
            assert!(number(s).is_none());
        }
        assert_eq!(number("0"), Some(0));
        for s in [
            "filename @@@",
            "filename YQ==,filename Yg==",
            "unexpected YQ==",
            "filename /w==",
        ] {
            assert!(metadata(s).is_none());
        }
    }
    #[test]
    fn display_names_are_safe_and_byte_capped() {
        for s in [
            "../../etc/passwd".to_owned(),
            "é雪".repeat(1000),
            "x".repeat(1000),
            "../\\\n".to_owned(),
        ] {
            let name = filename(&s);
            assert!(name.len() <= 255);
            assert_ne!(name, "");
            assert!(!name.contains(['/', '\\']));
            assert!(!name.chars().any(char::is_control));
        }
    }
}
