#![no_main]
#![forbid(unsafe_code)]

use std::time::Duration;

use arbitrary::Arbitrary;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use libfuzzer_sys::fuzz_target;
use yhotos_auth::{PasswordHash, Token, TokenKind, totp};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    bytes: [u8; 32],
    seconds: u64,
    last_used_step: Option<u64>,
    value: &'a str,
}

fuzz_target!(|input: Input<'_>| {
    let _ = Token::decode(input.value);
    let _ = PasswordHash::parse(input.value);
    // Keep structured PHC paths reachable without making expensive Argon2 calls.
    let output = STANDARD_NO_PAD.encode(input.bytes);
    let phc = format!(
        "$argon2id$v=19$m={},t={},p=1$c29tZXNhbHQxMjM0NTY3OA${output}",
        input.seconds % 300_000,
        input.bytes[0] % 12,
    );
    if let Ok(hash) = PasswordHash::parse(&phc) {
        let _ = yhotos_auth::needs_rehash(&hash);
    }
    // Exercise valid token parsing and hashes too, rather than only early rejections.
    let encoded = format!("ys_{}", URL_SAFE_NO_PAD.encode(input.bytes));
    if let Ok((kind, token)) = Token::decode(&encoded) {
        let _ = token.hash(kind);
        assert_eq!(encoded, token.encode(TokenKind::Session));
    }
    let secret_bytes = <[u8; 20]>::try_from(&input.bytes[..20]);
    if let Ok(bytes) = secret_bytes {
        let secret = totp::Secret::from_bytes(bytes);
        let _ = totp::verify(
            &secret,
            input.value,
            Duration::from_secs(input.seconds),
            input.last_used_step,
        );
    }
});
