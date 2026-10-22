#![forbid(unsafe_code)]

//! Job queue client and worker loop (Postgres SKIP LOCKED + NOTIFY).

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-jobs");
    }
}
