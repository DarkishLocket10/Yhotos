use std::path::{Path, PathBuf};

use crate::{Area, ContentHash, StorageError};

/// Private component whose constructors guarantee the safe-join alphabet and
/// length. Typed digest constructors make original paths infallible.
pub(crate) struct Component(String);

impl Component {
    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        if value.is_empty()
            || value.len() > 128
            || matches!(value, "." | "..")
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(StorageError::InvalidName);
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn area(area: Area) -> Self {
        Self(
            match area {
                Area::Originals => "originals",
                Area::Derivatives => "derivatives",
                Area::Video => "video",
            }
            .into(),
        )
    }

    pub(crate) fn tmp() -> Self {
        Self("tmp".into())
    }
    pub(crate) fn hash(hash: &ContentHash) -> Self {
        Self(hash.to_string())
    }
    pub(crate) fn byte(byte: u8) -> Self {
        Self(format!("{byte:02x}"))
    }
}

pub(crate) fn validate_token(value: &str) -> Result<(), StorageError> {
    if value.is_empty()
        || value.len() > 16
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(StorageError::InvalidName);
    }
    Ok(())
}

/// No separator, dot component, or absolute component can inhabit `Component`.
/// Appending them therefore preserves the canonical root's lexical prefix.
pub(crate) fn safe_join(root: &Path, components: impl IntoIterator<Item = Component>) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in components {
        path.push(component.0);
    }
    path
}
