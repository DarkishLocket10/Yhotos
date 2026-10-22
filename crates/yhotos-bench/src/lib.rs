#![forbid(unsafe_code)]

//! criterion benchmarks and load-test fixtures.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-bench");
    }
}
