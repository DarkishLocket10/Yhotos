#![forbid(unsafe_code)]

//! Passwords, sessions, TOTP, `WebAuthn`, share tokens, rate limit keys.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-auth");
    }
}
