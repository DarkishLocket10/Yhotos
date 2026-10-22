#![no_main]
#![forbid(unsafe_code)]
#[allow(dead_code, missing_docs)]
#[path = "../../crates/yhotos-http/src/admin/upload/headers.rs"]
mod headers;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|input: &[u8]| {
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = headers::number(text);
        let _ = headers::metadata(text);
        let name = headers::filename(text);
        assert!(name.len() <= 255);
        assert!(!name.contains(['/', '\\']));
    }
});
