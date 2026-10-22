//! SHA-1 TOTP with six digits, a 30-second step, one-step drift, and replay rejection.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use rand::{RngCore, rngs::OsRng};
use secrecy::SecretString;
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{AuthError, Timestamp};

/// Exactly 160 secret bits, erased on drop and never printed by Debug.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Secret([u8; 20]);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Secret {
    /// Wraps an exactly sized secret decrypted from storage.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Exposes the secret for encryption at rest. Never log or store it in plaintext.
    #[must_use]
    pub const fn expose_secret(&self) -> &[u8; 20] {
        &self.0
    }
}

/// Generates an independent 20-byte secret using the OS CSPRNG.
#[must_use]
pub fn generate_secret() -> Secret {
    let mut secret = Secret([0; 20]);
    OsRng.fill_bytes(&mut secret.0);
    secret
}

/// Returns a secret otpauth URI with percent-encoded labels and unpadded base32.
#[must_use]
pub fn provisioning_uri(issuer: &str, account: &str, secret: &Secret) -> SecretString {
    let encoded = Zeroizing::new(base32(&secret.0));
    SecretString::from(format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        percent_encode(issuer),
        percent_encode(account),
        encoded.as_str(),
        percent_encode(issuer),
    ))
}

fn percent_encode(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            // Formatting into String is infallible.
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn base32(bytes: &[u8; 20]) -> String {
    let mut output = String::with_capacity(32);
    let mut buffer = 0_u16;
    let mut bits = 0_u8;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let value = u8::try_from((buffer >> bits) & 31).unwrap_or(0);
            output.push(char::from(if value < 26 {
                b'A' + value
            } else {
                b'2' + value - 26
            }));
        }
    }
    output
}

fn code_at(secret: &Secret, step: u64) -> Result<[u8; 6], AuthError> {
    let mut mac = Hmac::<Sha1>::new_from_slice(&secret.0).map_err(|_| AuthError::Crypto)?;
    mac.update(&step.to_be_bytes());
    let digest: [u8; 20] = mac.finalize().into_bytes().into();
    let digest = Zeroizing::new(digest);
    let offset = usize::from(digest.last().ok_or(AuthError::Crypto)? & 0x0f);
    let truncated: [u8; 4] = digest
        .get(offset..offset + 4)
        .ok_or(AuthError::Crypto)?
        .try_into()
        .map_err(|_| AuthError::Crypto)?;
    let mut value = (u32::from_be_bytes(truncated) & 0x7fff_ffff) % 1_000_000;
    let mut digits = [b'0'; 6];
    for digit in digits.iter_mut().rev() {
        *digit += u8::try_from(value % 10).map_err(|_| AuthError::Crypto)?;
        value /= 10;
    }
    Ok(digits)
}

/// Generates the current six-digit code for a locally held secret.
///
/// # Errors
/// Returns a generic cryptographic failure; callers must never log this value.
pub fn generate(secret: &Secret, now: Timestamp) -> Result<SecretString, AuthError> {
    let bytes = code_at(secret, now.as_secs() / 30)?;
    let code = std::str::from_utf8(&bytes).map_err(|_| AuthError::Crypto)?;
    Ok(SecretString::from(code))
}

/// Accepts a six-digit ASCII code at the previous, current, or next 30-second step.
/// Returns the matched step, which must be persisted atomically before granting access.
///
/// # Errors
/// Rejects malformed codes, mismatches, and every matched step at or below
/// `last_used_step`. All valid candidate codes are compared using `subtle`.
pub fn verify(
    secret: &Secret,
    code: &str,
    now: Timestamp,
    last_used_step: Option<u64>,
) -> Result<u64, AuthError> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AuthError::Totp);
    }
    let step = now.as_secs() / 30;
    let mut matched = None;
    for candidate in [step.checked_sub(1), Some(step), step.checked_add(1)]
        .into_iter()
        .flatten()
    {
        let expected = Zeroizing::new(code_at(secret, candidate)?);
        let equal = bool::from(expected.as_slice().ct_eq(code.as_bytes()));
        if equal && last_used_step.is_none_or(|last| candidate > last) {
            matched = Some(candidate);
        }
    }
    matched.ok_or(AuthError::Totp)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use proptest::prelude::*;
    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn rfc6238_sha1_six_digit_vectors() {
        // RFC 6238 Appendix B, SHA1; the public test key is ASCII 12345678901234567890.
        // Six digits are the RFC's eight-digit values reduced modulo 1_000_000.
        // https://www.rfc-editor.org/rfc/rfc6238#appendix-B
        let secret = Secret(*b"12345678901234567890");
        for (seconds, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(
                verify(&secret, code, Duration::from_secs(seconds), None),
                Ok(seconds / 30)
            );
        }
    }

    #[test]
    fn drift_replay_and_boundaries() {
        // Public RFC key makes neighbouring-code collisions deterministic.
        let secret = Secret(*b"12345678901234567890");
        let step = 1000;
        let code = code_at(&secret, step).unwrap();
        let code = std::str::from_utf8(&code).unwrap();
        let used = verify(&secret, code, Duration::from_secs(step * 30), None).unwrap();
        assert_eq!(
            verify(&secret, code, Duration::from_secs(step * 30), Some(used)),
            Err(AuthError::Totp)
        );
        for now in [step - 1, step, step + 1] {
            assert_eq!(
                verify(&secret, code, Duration::from_secs(now * 30), None),
                Ok(step)
            );
        }
        assert_eq!(
            verify(
                &secret,
                code,
                Duration::from_secs(step * 30),
                Some(step - 1)
            ),
            Ok(step)
        );
        assert_eq!(
            verify(&secret, code, Duration::from_secs(step * 30), Some(step)),
            Err(AuthError::Totp)
        );
        assert_eq!(
            verify(
                &secret,
                code,
                Duration::from_secs(step * 30),
                Some(step + 1)
            ),
            Err(AuthError::Totp)
        );
        // A consumed step is rejected throughout its drift window.
        assert_eq!(
            verify(
                &secret,
                code,
                Duration::from_secs((step + 1) * 30),
                Some(step)
            ),
            Err(AuthError::Totp)
        );
        let zero = code_at(&secret, 0).unwrap();
        assert_eq!(
            verify(
                &secret,
                std::str::from_utf8(&zero).unwrap(),
                Duration::ZERO,
                None
            ),
            Ok(0)
        );
        let _ = verify(&secret, code, Duration::MAX, None);
    }

    #[test]
    fn out_of_window_and_malformed_codes() {
        let secret = Secret(*b"12345678901234567890");
        assert_eq!(
            verify(&secret, "287082", Duration::from_secs(120), None),
            Err(AuthError::Totp)
        );
        for bad in [
            "",
            "12345",
            "1234567",
            "１２３４５６",
            "12 456",
            "+12345",
            "12345\n",
            "abcdef",
        ] {
            assert_eq!(
                verify(&secret, bad, Duration::ZERO, None),
                Err(AuthError::Totp)
            );
        }
    }

    #[test]
    fn secret_zeroizes_and_debug_is_redacted() {
        fn zeroizes_on_drop<T: Zeroize + ZeroizeOnDrop>() {}
        zeroizes_on_drop::<Secret>();
        let mut secret = generate_secret();
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        secret.zeroize();
        assert_eq!(secret.expose_secret(), &[0; 20]);
    }

    #[test]
    fn provisioning_encodes_labels_and_parameters() {
        let secret = Secret(*b"12345678901234567890");
        assert_eq!(base32(&secret.0), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let uri = provisioning_uri("Yhotos &:猫", "a+b@example.com?x=1", &secret);
        assert_eq!(
            uri.expose_secret(),
            "otpauth://totp/Yhotos%20%26%3A%E7%8C%AB:a%2Bb%40example.com%3Fx%3D1?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Yhotos%20%26%3A%E7%8C%AB&algorithm=SHA1&digits=6&period=30"
        );
    }

    proptest! {
        #[test]
        fn arbitrary_codes_never_panic(code in any::<String>(), seconds in any::<u64>(), last in any::<Option<u64>>()) {
            let secret = generate_secret();
            let _ = verify(&secret, &code, Duration::from_secs(seconds), last);
        }
    }
}
