#![forbid(unsafe_code)]

//! Cryptographic primitives and pure authentication policies, without HTTP or storage.
//!
//! Password operations are blocking and must run off the async executor. Callers must
//! persist TOTP replay counters and consume `WebAuthn` ceremony state atomically.

mod common_passwords;
mod keyring;
mod password;
mod policy;
mod token;
pub mod totp;
pub mod webauthn;

pub use keyring::Keyring;
pub use password::{
    PasswordHash, check_password_policy, dummy_verify, generate_temp_password, hash_password,
    needs_rehash, verify_password,
};
pub use policy::{
    LockedUntil, LockoutPolicy, LockoutState, SessionPolicy, SessionState, Timestamp, session_state,
};
pub use secrecy::SecretString;
pub use token::{Token, TokenHash, TokenKind};

/// Authentication failures contain no credentials or dependency details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// A password falls outside the supported length.
    #[error("invalid password length")]
    PasswordLength,
    /// A new password equals the username or occurs in the common-password list.
    #[error("password does not meet policy")]
    PasswordPolicy,
    /// A stored password hash is malformed or outside bounded verification costs.
    #[error("invalid password hash")]
    PasswordHash,
    /// Cryptographic processing failed without exposing its inputs.
    #[error("authentication operation failed")]
    Crypto,
    /// An opaque token is not a canonical supported encoding.
    #[error("invalid token")]
    Token,
    /// An MFA code is malformed, outside its time window, or already consumed.
    #[error("invalid authentication code")]
    Totp,
    /// The configured master key is not exactly 32 standard-base64-encoded bytes.
    #[error("invalid encryption key")]
    Key,
    /// A sealed value has an invalid format, version, key, tag, or row binding.
    #[error("invalid encrypted value")]
    Ciphertext,
    /// The relying party configuration or ceremony response is invalid.
    #[error("invalid passkey operation")]
    Passkey,
}
