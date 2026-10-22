use std::fmt;
use std::str::FromStr;

use crate::StorageError;

/// A validated BLAKE3 digest, formatted as exactly 64 lowercase hex characters.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContentHash(pub(crate) [u8; 32]);

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ContentHash {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StorageError::InvalidHash);
        }
        let mut hash = [0; 32];
        for (byte, hex) in hash.iter_mut().zip(s.as_bytes().as_chunks::<2>().0) {
            let [hi, lo] = hex;
            let nibble = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
            *byte = (nibble(*hi) << 4) | nibble(*lo);
        }
        Ok(Self(hash))
    }
}
