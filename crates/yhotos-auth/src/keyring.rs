use std::fmt;

use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rand::{RngCore, rngs::OsRng};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::AuthError;

const VERSION: u8 = 1;
const NONCE_BYTES: usize = 24;
const HEADER_BYTES: usize = 1 + NONCE_BYTES;
const TAG_BYTES: usize = 16;

/// A master key erased on drop. Ciphertexts are versioned and bound to caller-supplied row IDs.
pub struct Keyring {
    key: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for Keyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keyring(<redacted>)")
    }
}

impl Keyring {
    /// Validates exactly 32 bytes encoded in canonical padded standard base64.
    ///
    /// # Errors
    /// Returns a generic key error for invalid configuration.
    pub fn from_master(key: &SecretString) -> Result<Self, AuthError> {
        if key.expose_secret().len() != 44 {
            return Err(AuthError::Key);
        }
        let mut decoded = Zeroizing::new([0; 32]);
        let len = STANDARD
            .decode_slice(key.expose_secret(), decoded.as_mut())
            .map_err(|_| AuthError::Key)?;
        if len != 32 {
            return Err(AuthError::Key);
        }
        Ok(Self { key: decoded })
    }

    /// Authenticates row ID AAD and encrypts with an independent random 24-byte nonce.
    /// The result is version || nonce || ciphertext || tag.
    ///
    /// # Errors
    /// Returns a generic error if the OS RNG or encryption fails.
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, AuthError> {
        let mut nonce = [0; NONCE_BYTES];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| AuthError::Crypto)?;
        self.seal_with_nonce(aad, plaintext, nonce)
    }

    fn seal_with_nonce(
        &self,
        aad: &[u8],
        plaintext: &[u8],
        nonce: [u8; NONCE_BYTES],
    ) -> Result<Vec<u8>, AuthError> {
        let cipher = XChaCha20Poly1305::new((&*self.key).into());
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| AuthError::Crypto)?;
        let mut sealed = Vec::with_capacity(HEADER_BYTES + ciphertext.len());
        sealed.push(VERSION);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Releases plaintext only after version, authentication, and row binding succeed.
    /// Callers should immediately wrap secret plaintext in a zeroizing buffer.
    ///
    /// # Errors
    /// All malformed, truncated, moved, tampered, or wrongly keyed values return
    /// the same generic error without releasing any plaintext.
    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, AuthError> {
        if sealed.len() < HEADER_BYTES + TAG_BYTES || sealed.first() != Some(&VERSION) {
            return Err(AuthError::Ciphertext);
        }
        let nonce: [u8; NONCE_BYTES] = sealed
            .get(1..HEADER_BYTES)
            .ok_or(AuthError::Ciphertext)?
            .try_into()
            .map_err(|_| AuthError::Ciphertext)?;
        let ciphertext = sealed.get(HEADER_BYTES..).ok_or(AuthError::Ciphertext)?;
        XChaCha20Poly1305::new((&*self.key).into())
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| AuthError::Ciphertext)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn random_keyring() -> Keyring {
        let mut bytes = Zeroizing::new([0_u8; 32]);
        OsRng.fill_bytes(bytes.as_mut());
        Keyring::from_master(&SecretString::from(STANDARD.encode(bytes.as_ref()))).unwrap()
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn xchacha20poly1305_known_answer() {
        // RustCrypto chacha20poly1305 0.11.0, tests/lib.rs, mod xchacha20:
        // https://github.com/RustCrypto/AEADs/blob/chacha20poly1305-v0.11.0/chacha20poly1305/tests/lib.rs
        // Reproduces the draft-irtf-cfrg-xchacha Appendix A.1 public test vector.
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce: [u8; 24] = hex("404142434445464748494a4b4c4d4e4f5051525354555657")
            .try_into()
            .unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let expected = hex(concat!(
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb",
            "731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452",
            "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9",
            "21f9664c97637da9768812f615c68b13b52e",
            "c0875924c1c7987947deafd8780acf49",
        ));
        let ring = Keyring::from_master(&SecretString::from(STANDARD.encode(key))).unwrap();
        let sealed = ring.seal_with_nonce(&aad, plaintext, nonce).unwrap();
        assert_eq!(&sealed[HEADER_BYTES..], expected);
        let mut known = vec![VERSION];
        known.extend_from_slice(&nonce);
        known.extend_from_slice(&expected);
        assert_eq!(ring.open(&aad, &known).unwrap(), plaintext);
    }

    #[test]
    fn every_byte_is_authenticated_and_bound_to_row() {
        let ring = random_keyring();
        let aad = crate::Token::generate().encode(crate::TokenKind::Session);
        let secret = crate::totp::generate_secret();
        let plaintext = secret.expose_secret();
        let sealed = ring.seal(aad.as_bytes(), plaintext).unwrap();
        assert_eq!(ring.open(aad.as_bytes(), &sealed).unwrap(), plaintext);
        assert_ne!(sealed, ring.seal(aad.as_bytes(), plaintext).unwrap());
        for i in 0..sealed.len() {
            let mut changed = sealed.clone();
            changed[i] ^= 1;
            assert_eq!(
                ring.open(aad.as_bytes(), &changed),
                Err(AuthError::Ciphertext)
            );
        }
        for i in 0..sealed.len() {
            assert_eq!(
                ring.open(aad.as_bytes(), &sealed[..i]),
                Err(AuthError::Ciphertext)
            );
        }
        assert_eq!(
            ring.open(b"another row", &sealed),
            Err(AuthError::Ciphertext)
        );
        assert_eq!(
            random_keyring().open(aad.as_bytes(), &sealed),
            Err(AuthError::Ciphertext)
        );
        assert_eq!(format!("{ring:?}"), "Keyring(<redacted>)");
        let empty = ring.seal(b"", b"").unwrap();
        assert_eq!(ring.open(b"", &empty).unwrap(), b"");
    }

    #[test]
    fn rejects_unknown_versions_and_invalid_keys() {
        let ring = random_keyring();
        let mut sealed = ring.seal(b"row", b"").unwrap();
        for version in 0..=255 {
            if version != VERSION {
                sealed[0] = version;
                assert_eq!(ring.open(b"row", &sealed), Err(AuthError::Ciphertext));
            }
        }
        for invalid in [
            String::new(),
            "!".repeat(44),
            STANDARD.encode([0; 31]),
            STANDARD.encode([0; 33]),
        ] {
            assert_eq!(
                Keyring::from_master(&SecretString::from(invalid)).err(),
                Some(AuthError::Key)
            );
        }
    }
}
