#![forbid(unsafe_code)]

//! Measures password verification and the token/TOTP request-path primitives.

use std::{hint::black_box, time::Duration};

use criterion::Criterion;
use yhotos_auth::{Token, TokenKind, generate_temp_password, hash_password, totp, verify_password};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut criterion = Criterion::default().configure_from_args();
    let password = generate_temp_password();
    let hash = hash_password(&password)?;
    let mut group = criterion.benchmark_group("auth");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));
    group.bench_function("verify_password", |b| {
        b.iter(|| black_box(verify_password(black_box(&password), black_box(&hash))));
    });
    let encoded = Token::generate().encode(TokenKind::Session);
    group.bench_function("token_decode_hash", |b| {
        b.iter(|| {
            black_box(Token::decode(black_box(&encoded)).map(|(kind, token)| token.hash(kind)))
        });
    });
    // Public RFC 6238 Appendix B vector, adapted to six digits.
    let secret = totp::Secret::from_bytes(*b"12345678901234567890");
    let now = Duration::from_secs(59);
    totp::verify(&secret, "287082", now, None)?;
    group.bench_function("totp_verify", |b| {
        b.iter(|| {
            black_box(totp::verify(
                black_box(&secret),
                black_box("287082"),
                black_box(now),
                None,
            ))
        });
    });
    group.finish();
    criterion.final_summary();
    Ok(())
}
