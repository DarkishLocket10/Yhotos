#![forbid(unsafe_code)]

//! Typed configuration loaded from env (and optional TOML).

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-config");
    }
}
