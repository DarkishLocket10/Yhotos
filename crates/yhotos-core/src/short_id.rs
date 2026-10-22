//! Short ids are lookup keys, never secrets. Only the low 60 UUID bits survive;
//! the full UUID cannot be recovered. The database must store the mapping.

use crate::PhotoId;
use std::{fmt, str::FromStr};

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A validated, lowercase, twelve-character base32 lookup key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ShortId([u8; 12]);

/// Rejects any spelling outside the canonical twelve-character alphabet.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShortIdError {
    /// The key did not contain exactly twelve bytes.
    #[error("short id must contain twelve characters")]
    Length,
    /// A byte was outside lowercase a-z and digits 2-7.
    #[error("short id contains an invalid character")]
    Alphabet,
}

impl ShortId {
    /// Encodes the low sixty bits in most-significant-digit-first order.
    #[must_use]
    #[allow(
        clippy::indexing_slicing,
        reason = "the five-bit mask is bounded by the 32-entry alphabet"
    )]
    pub fn from_photo_id(id: PhotoId) -> Self {
        let mut value = id.0.as_u128();
        let mut bytes = [b'a'; 12];
        for byte in bytes.iter_mut().rev() {
            // The mask proves this index is within the alphabet.
            *byte = ALPHABET[(value & 31) as usize];
            value >>= 5;
        }
        Self(bytes)
    }
}
impl fmt::Display for ShortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{}", char::from(byte))?;
        }
        Ok(())
    }
}
impl FromStr for ShortId {
    type Err = ShortIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 12] = s.as_bytes().try_into().map_err(|_| ShortIdError::Length)?;
        if !bytes.iter().all(|b| ALPHABET.contains(b)) {
            return Err(ShortIdError::Alphabet);
        }
        Ok(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn encoding_and_non_reversibility() {
        for (bits, expected) in [
            (0, "aaaaaaaaaaaa"),
            (1, "aaaaaaaaaaab"),
            ((1 << 60) - 1, "777777777777"),
        ] {
            let short = ShortId::from_photo_id(PhotoId(uuid::Uuid::from_u128(bits)));
            assert_eq!(short.to_string(), expected);
            assert_eq!(expected.parse::<ShortId>().unwrap(), short);
            assert_eq!(
                ShortId::from_photo_id(PhotoId(uuid::Uuid::from_u128(bits | (1 << 100)))),
                short
            );
        }
    }
    #[test]
    fn rejects_every_invalid_class() {
        for value in ["", "a", "aaaaaaaaaaa", "aaaaaaaaaaaaa", "éaaaaaaaaaaa"] {
            assert_eq!(value.parse::<ShortId>(), Err(ShortIdError::Length));
        }
        for value in [
            "Aaaaaaaaaaaa",
            "0aaaaaaaaaaa",
            "1aaaaaaaaaaa",
            "8aaaaaaaaaaa",
            "9aaaaaaaaaaa",
            "aaaaaaaaaaa=",
            "aaaaaaaaaaa-",
            "aaaaaaaaaaa ",
            "aaaaaaaaaaé",
        ] {
            assert_eq!(value.parse::<ShortId>(), Err(ShortIdError::Alphabet));
        }
    }
}
