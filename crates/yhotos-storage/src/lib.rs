#![forbid(unsafe_code)]

//! Content-addressed file store, safe paths, streaming IO.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-storage");
    }
}
