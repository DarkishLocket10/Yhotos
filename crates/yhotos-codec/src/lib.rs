#![deny(unsafe_op_in_unsafe_fn)]

//! The ONLY crate with unsafe: libvips, libraw/rawler, libheif FFI.
//!
//! Unsafe code is confined to this crate's codec FFI boundary. Each unsafe block
//! must document its safety invariant and have a test exercising that invariant;
//! unsafe operations inside unsafe functions still require explicit blocks.

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_has_expected_crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "yhotos-codec");
    }
}
