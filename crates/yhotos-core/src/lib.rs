#![forbid(unsafe_code)]

//! Domain types, ids, errors, permissions, slugs, time. No IO.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-core");
    }
}
