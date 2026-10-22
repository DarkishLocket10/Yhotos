use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::AuthError;

/// Purpose binding prevents a token accepted in one flow from authorizing another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Browser session.
    Session,
    /// A narrowly scoped share capability.
    ShareLink,
    /// Double-submit CSRF protection.
    Csrf,
    /// One-time password reset.
    PasswordReset,
}

impl TokenKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Session => "ys",
            Self::ShareLink => "yl",
            Self::Csrf => "yc",
            Self::PasswordReset => "yr",
        }
    }

    const fn context(self) -> &'static str {
        match self {
            Self::Session => "yhotos token v1 session",
            Self::ShareLink => "yhotos token v1 share_link",
            Self::Csrf => "yhotos token v1 csrf",
            Self::PasswordReset => "yhotos token v1 password_reset",
        }
    }
}

/// A 256-bit bearer secret; its buffer is erased on drop and Debug is redacted.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Token([u8; 32]);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Generates 256 independent random bits using the operating system CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut token = Self([0; 32]);
        OsRng.fill_bytes(&mut token.0);
        token
    }

    /// Returns the canonical bearer encoding. Treat the returned string as secret.
    #[must_use]
    pub fn encode(&self, kind: TokenKind) -> String {
        let mut encoded = String::with_capacity(46);
        encoded.push_str(kind.prefix());
        encoded.push('_');
        URL_SAFE_NO_PAD.encode_string(self.0, &mut encoded);
        encoded
    }

    /// Accepts only a known prefix and exactly 32 canonical base64url bytes.
    ///
    /// Prefix, length, and alphabet are public syntax. Secret verification uses
    /// `TokenHash::ct_eq`, never encoded-string equality.
    ///
    /// # Errors
    /// Returns `AuthError::Token` for invalid syntax, padding, or noncanonical tail bits.
    pub fn decode(s: &str) -> Result<(TokenKind, Self), AuthError> {
        if s.len() != 46 {
            return Err(AuthError::Token);
        }
        let kind = match s.get(..3) {
            Some("ys_") => TokenKind::Session,
            Some("yl_") => TokenKind::ShareLink,
            Some("yc_") => TokenKind::Csrf,
            Some("yr_") => TokenKind::PasswordReset,
            _ => return Err(AuthError::Token),
        };
        let payload = s.get(3..).ok_or(AuthError::Token)?;
        let mut token = Self([0; 32]);
        let len = URL_SAFE_NO_PAD
            .decode_slice(payload, &mut token.0)
            .map_err(|_| AuthError::Token)?;
        if len != 32 {
            return Err(AuthError::Token);
        }
        Ok((kind, token))
    }

    /// Returns the purpose-separated keyed BLAKE3 storage form, never the bearer.
    #[must_use]
    pub fn hash(&self, kind: TokenKind) -> TokenHash {
        // Empty key material is intentional: this derives a public, purpose-specific
        // domain key, not a deployment secret. Security rests on the random 256-bit token.
        let key = blake3::derive_key(kind.context(), b"");
        TokenHash(*blake3::keyed_hash(&key, &self.0).as_bytes())
    }
}

/// A storage-safe token digest. Equality must use `ct_eq`.
#[derive(Clone)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Reconstructs an exactly sized digest loaded from storage.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Exposes the hashed storage representation, never a bearer token.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compares all digest bytes in constant time using `subtle`.
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use proptest::prelude::*;

    use super::*;

    const KINDS: [TokenKind; 4] = [
        TokenKind::Session,
        TokenKind::ShareLink,
        TokenKind::Csrf,
        TokenKind::PasswordReset,
    ];

    #[test]
    fn strict_decoding() {
        for kind in KINDS {
            let encoded = Token::generate().encode(kind);
            assert_eq!(encoded.len(), 46);
            assert_eq!(Token::decode(&encoded).unwrap().0, kind);
            for bad in [
                encoded.replacen(kind.prefix(), "xx", 1),
                encoded[..45].to_owned(),
                format!("{encoded}="),
                format!("{}+", &encoded[..45]),
                format!("{}/", &encoded[..45]),
                format!("{}=", &encoded[..45]),
                format!("{} ", &encoded[..45]),
                format!("{}é", &encoded[..44]),
                format!("{}B", &encoded[..45]), // non-zero unused trailing bits
            ] {
                assert!(Token::decode(&bad).is_err());
            }
        }
    }

    #[test]
    fn hashes_are_separated_and_comparison_is_constant_time() {
        let token = Token::generate();
        for kind in KINDS {
            let hash = token.hash(kind);
            assert!(hash.ct_eq(&TokenHash::from_bytes(*hash.as_bytes())));
            for other in KINDS {
                assert_eq!(hash.ct_eq(&token.hash(other)), kind == other);
            }
            for i in 0..32 {
                let mut changed = *hash.as_bytes();
                changed[i] ^= 1;
                assert!(!hash.ct_eq(&TokenHash::from_bytes(changed)));
            }
        }
        assert!(
            !token
                .hash(TokenKind::Session)
                .ct_eq(&Token::generate().hash(TokenKind::Session))
        );
    }

    #[test]
    fn bearer_is_zeroized_and_debug_is_redacted() {
        fn zeroizes_on_drop<T: Zeroize + ZeroizeOnDrop>() {}
        zeroizes_on_drop::<Token>();
        let mut token = Token::generate();
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
        token.zeroize();
        assert_eq!(token.0, [0; 32]);
    }

    proptest! {
        #[test]
        fn roundtrip(bytes in any::<[u8; 32]>(), index in 0_usize..4) {
            let kind = KINDS[index];
            let token = Token(bytes);
            let (decoded_kind, decoded) = Token::decode(&token.encode(kind)).unwrap();
            prop_assert_eq!(decoded_kind, kind);
            prop_assert!(token.hash(kind).ct_eq(&decoded.hash(kind)));
        }

        #[test]
        fn arbitrary_strings_never_panic(s in any::<String>()) {
            let _ = Token::decode(&s);
        }
    }
}
