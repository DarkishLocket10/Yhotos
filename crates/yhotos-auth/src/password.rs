use std::{fmt, str::FromStr};

use argon2::{Algorithm, Argon2, Params, Version};
use password_hash::{PasswordHasher, PasswordVerifier, phc};
use rand::{Rng, RngCore, rngs::OsRng};
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{AuthError, common_passwords::COMMON_PASSWORDS};

const MEMORY_KIB: u32 = 65_536;
const ITERATIONS: u32 = 3;
const LANES: u32 = 1;
const TEMP_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

// Public reference vector, not an application credential. Generated with the
// reference libargon2 argon2id_hash_encoded(3, 65536, 1, "password", 8,
// "somesalt12345678", 16, 32, output, 200), version 0x13 (see reference_vector).
const DUMMY_HASH: &str = "$argon2id$v=19$m=65536,t=3,p=1$c29tZXNhbHQxMjM0NTY3OA$VGt0xwURFGFBIiQgQWSqTQNzmCXz0LkKkGG7zDrwT/k";

/// An Argon2id PHC string with validated syntax and bounded verification costs.
#[derive(Clone)]
pub struct PasswordHash(String);

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash(<redacted>)")
    }
}

impl PasswordHash {
    /// Validates a stored PHC string before allocating or performing expensive work.
    ///
    /// # Errors
    /// Rejects malformed or incomplete hashes, unsupported algorithms/versions, and
    /// costs over 256 MiB, ten iterations, or four lanes. Older, weaker Argon2id
    /// hashes remain verifiable and are flagged by `needs_rehash`.
    pub fn parse(value: &str) -> Result<Self, AuthError> {
        parse_hash(value)?;
        Ok(Self(value.to_owned()))
    }

    /// Returns the PHC storage representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PasswordHash {
    type Err = AuthError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn current_argon2() -> Result<Argon2<'static>, AuthError> {
    let params =
        Params::new(MEMORY_KIB, ITERATIONS, LANES, Some(32)).map_err(|_| AuthError::Crypto)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

fn parse_hash(value: &str) -> Result<(phc::PasswordHash, Params), AuthError> {
    if value.len() > 256 {
        return Err(AuthError::PasswordHash);
    }
    let parsed = phc::PasswordHash::new(value).map_err(|_| AuthError::PasswordHash)?;
    let params = Params::try_from(&parsed).map_err(|_| AuthError::PasswordHash)?;
    if parsed.algorithm.as_str() != "argon2id"
        || !matches!(parsed.version, Some(16 | 19))
        || parsed.params.iter().count() != 3
        || !["m", "t", "p"]
            .into_iter()
            .all(|name| parsed.params.get(name).is_some())
        || params.m_cost() > 262_144
        || params.t_cost() > 10
        || params.p_cost() > 4
        || parsed
            .salt
            .as_ref()
            .is_none_or(|salt| salt.as_ref().len() < 8)
        || !parsed
            .hash
            .as_ref()
            .is_some_and(|hash| (16..=64).contains(&hash.len()))
    {
        return Err(AuthError::PasswordHash);
    }
    Ok((parsed, params))
}

/// Hashes 8 through 256 password bytes using Argon2id, 64 MiB, t=3, p=1,
/// a fresh 16-byte OS-random salt, and a 32-byte output.
///
/// # Errors
/// Returns a length error or a generic cryptographic error. This is blocking work.
pub fn hash_password(pw: &SecretString) -> Result<PasswordHash, AuthError> {
    validate_length(pw)?;
    let mut salt = [0; 16];
    OsRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| AuthError::Crypto)?;
    let hash: phc::PasswordHash = current_argon2()?
        .hash_password_with_salt(pw.expose_secret().as_bytes(), &salt)
        .map_err(|_| AuthError::Crypto)?;
    Ok(PasswordHash(hash.to_string()))
}

fn validate_length(pw: &SecretString) -> Result<(), AuthError> {
    if !(8..=256).contains(&pw.expose_secret().len()) {
        return Err(AuthError::PasswordLength);
    }
    Ok(())
}

/// Verifies a supported stored hash in constant time at the digest comparison.
///
/// # Errors
/// Malformed hashes always return `Err`, including when the password is invalid.
/// Wrong passwords return `Ok(false)`. This is blocking work.
pub fn verify_password(pw: &SecretString, hash: &PasswordHash) -> Result<bool, AuthError> {
    let (parsed, _) = parse_hash(&hash.0)?;
    validate_length(pw)?;
    match current_argon2()?.verify_password(pw.expose_secret().as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::PasswordInvalid) => Ok(false),
        Err(_) => Err(AuthError::PasswordHash),
    }
}

/// Performs one full-cost failed verification for unknown-user paths.
pub fn dummy_verify() {
    let hash = PasswordHash(DUMMY_HASH.to_owned());
    // Public, deliberately mismatching dummy input; never an account credential.
    let input = SecretString::from("yhotos unknown-user dummy input");
    let _ = verify_password(&input, &hash);
}

/// Flags older parameters, short salts/outputs, or unsupported storage values.
#[must_use]
pub fn needs_rehash(hash: &PasswordHash) -> bool {
    let Ok((parsed, params)) = parse_hash(&hash.0) else {
        return true;
    };
    parsed.version != Some(19)
        || params.m_cost() < MEMORY_KIB
        || params.t_cost() < ITERATIONS
        || params.p_cost() < LANES
        || parsed
            .salt
            .as_ref()
            .is_none_or(|salt| salt.as_ref().len() < 16)
        || parsed.hash.as_ref().is_none_or(|output| output.len() < 32)
}

/// Requires 12 Unicode characters, at most 256 UTF-8 bytes, and rejects the
/// username and embedded common passwords, case-insensitively.
///
/// # Errors
/// Returns a length or password-policy error without echoing the input.
pub fn check_password_policy(pw: &SecretString, username: &str) -> Result<(), AuthError> {
    let value = pw.expose_secret();
    if value.len() > 256 || value.chars().count() < 12 {
        return Err(AuthError::PasswordLength);
    }
    if username.len() > 256 {
        return Err(AuthError::PasswordPolicy);
    }
    let lower = Zeroizing::new(value.to_lowercase());
    let username = username.to_lowercase();
    if bool::from(lower.as_bytes().ct_eq(username.as_bytes()))
        || COMMON_PASSWORDS.binary_search(&lower.as_str()).is_ok()
    {
        return Err(AuthError::PasswordPolicy);
    }
    Ok(())
}

/// Returns 20 uniformly sampled, unambiguous characters from the OS CSPRNG.
#[must_use]
pub fn generate_temp_password() -> SecretString {
    let value: String = (0..20)
        .filter_map(|_| TEMP_ALPHABET.get(OsRng.gen_range(0..TEMP_ALPHABET.len())))
        .map(|byte| char::from(*byte))
        .collect();
    SecretString::from(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn reference_vector() {
        // Produced using the reference C implementation, installed libargon2.so.1:
        // Python ctypes: out = create_string_buffer(200);
        // lib.argon2id_hash_encoded(3, 65536, 1, b"password", 8,
        //     b"somesalt12345678", 16, 32, out, len(out)).
        // Result is exactly DUMMY_HASH above. These are published test inputs.
        let actual: phc::PasswordHash = current_argon2()
            .unwrap()
            .hash_password_with_salt(b"password", b"somesalt12345678")
            .unwrap();
        assert_eq!(actual.to_string(), DUMMY_HASH);
        assert!(
            verify_password(
                &SecretString::from("password"),
                &PasswordHash::parse(DUMMY_HASH).unwrap()
            )
            .unwrap()
        );
    }

    #[test]
    fn hashing_verification_and_salts() {
        let pw = generate_temp_password();
        let a = hash_password(&pw).unwrap();
        let b = hash_password(&pw).unwrap();
        assert_ne!(a.as_str(), b.as_str());
        assert!(!needs_rehash(&a));
        let (parsed, params) = parse_hash(a.as_str()).unwrap();
        assert_eq!(
            (params.m_cost(), params.t_cost(), params.p_cost()),
            (65_536, 3, 1)
        );
        assert_eq!(parsed.salt.unwrap().as_ref().len(), 16);
        assert!(verify_password(&pw, &a).unwrap());
        assert!(!verify_password(&generate_temp_password(), &a).unwrap());
        assert_eq!(format!("{a:?}"), "PasswordHash(<redacted>)");
    }

    #[test]
    fn malformed_hashes_return_errors_before_work() {
        let pw = generate_temp_password();
        for invalid in [
            String::new(),
            "$argon2id".to_owned(),
            DUMMY_HASH.replace("argon2id", "argon2i"),
            DUMMY_HASH.replace("v=19", "v=99"),
            DUMMY_HASH.replace("m=65536", "m=4294967295"),
            DUMMY_HASH.replace("t=3", "t=4294967295"),
            DUMMY_HASH.replace("p=1", "p=16777215"),
            DUMMY_HASH.replace("m=65536", "m=1"),
            DUMMY_HASH.replace("t=3", "t=0"),
            DUMMY_HASH.replace("p=1", "p=0"),
            DUMMY_HASH.replace("m=65536,", ""),
            DUMMY_HASH.replace(",p=1", ",p=1,keyid=AA"),
            DUMMY_HASH.replace("c29tZXNhbHQxMjM0NTY3OA", "AA"),
            DUMMY_HASH.rsplit_once('$').unwrap().0.to_owned(),
            "a".repeat(257),
        ] {
            assert!(PasswordHash::parse(&invalid).is_err());
            // Even a corrupt in-memory storage wrapper must never panic.
            assert_eq!(
                verify_password(&pw, &PasswordHash(invalid)),
                Err(AuthError::PasswordHash)
            );
        }
    }

    #[test]
    fn length_boundaries_and_rehash() {
        for len in [0, 7, 257] {
            assert_eq!(
                hash_password(&SecretString::from("a".repeat(len))).err(),
                Some(AuthError::PasswordLength)
            );
        }
        for len in [8, 256] {
            let pw = SecretString::from("a".repeat(len));
            assert!(verify_password(&pw, &hash_password(&pw).unwrap()).unwrap());
        }
        for (from, to) in [("m=65536", "m=32768"), ("t=3", "t=2"), ("v=19", "v=16")] {
            assert!(needs_rehash(
                &PasswordHash::parse(&DUMMY_HASH.replace(from, to)).unwrap()
            ));
        }
        assert!(!needs_rehash(
            &PasswordHash::parse(&DUMMY_HASH.replace("m=65536", "m=131072")).unwrap()
        ));
    }

    #[test]
    fn policy_and_temporary_passwords() {
        assert!(COMMON_PASSWORDS.len() >= 10_000);
        assert!(COMMON_PASSWORDS.windows(2).all(|w| w[0] < w[1]));
        for common in COMMON_PASSWORDS
            .iter()
            .filter(|pw| pw.chars().count() >= 12)
        {
            assert_eq!(
                check_password_policy(&SecretString::from(*common), "unrelated"),
                Err(AuthError::PasswordPolicy)
            );
        }
        for len in [0, 11, 257] {
            assert_eq!(
                check_password_policy(&SecretString::from("x".repeat(len)), "user"),
                Err(AuthError::PasswordLength)
            );
        }
        assert!(check_password_policy(&SecretString::from("猫".repeat(12)), "user").is_ok());
        assert!(check_password_policy(&SecretString::from("猫".repeat(86)), "user").is_err());
        let pw = generate_temp_password();
        assert_eq!(
            check_password_policy(&pw, &pw.expose_secret().to_uppercase()),
            Err(AuthError::PasswordPolicy)
        );
        for _ in 0..100 {
            let pw = generate_temp_password();
            assert_eq!(pw.expose_secret().len(), 20);
            assert!(
                pw.expose_secret()
                    .bytes()
                    .all(|byte| TEMP_ALPHABET.contains(&byte))
            );
            assert!(check_password_policy(&pw, "user").is_ok());
        }
    }

    #[test]
    #[ignore = "wall-clock comparison; run in release mode on an idle machine"]
    fn dummy_timing_within_twenty_percent() {
        use std::time::{Duration, Instant};

        let hash = hash_password(&generate_temp_password()).unwrap();
        let wrong = generate_temp_password();
        dummy_verify();
        let mut real = Duration::ZERO;
        let mut dummy = Duration::ZERO;
        for i in 0..5 {
            let mut measure_real = || {
                let start = Instant::now();
                assert!(!verify_password(&wrong, &hash).unwrap());
                real += start.elapsed();
            };
            let mut measure_dummy = || {
                let start = Instant::now();
                dummy_verify();
                dummy += start.elapsed();
            };
            if i % 2 == 0 {
                measure_real();
                measure_dummy();
            } else {
                measure_dummy();
                measure_real();
            }
        }
        let ratio = dummy.as_secs_f64() / real.as_secs_f64();
        assert!(
            (0.8..=1.2).contains(&ratio),
            "dummy/real timing ratio = {ratio}"
        );
    }

    proptest! {
        #[test]
        fn arbitrary_phc_strings_never_panic(s in any::<String>()) {
            let _ = PasswordHash::parse(&s);
        }
    }
}
