#![forbid(unsafe_code)]

//! Ingest orchestration: validate, hash, metadata, variant plan, enqueue.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-pipeline");
    }
}
