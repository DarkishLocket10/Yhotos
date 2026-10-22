#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/yhotos-http/src/files/range.rs"]
mod range;

fuzz_target!(|data: &[u8]| {
    let Some((size, header)) = data.split_first_chunk::<8>() else {
        return;
    };
    let Ok(header) = std::str::from_utf8(header) else {
        return;
    };
    for total in [u64::from_le_bytes(*size), 0, 1, 1024, u64::MAX] {
        if let Ok(result) = range::parse(header, total) {
            assert!(result.len > 0);
            assert!(result.start < total);
            assert!(result.len <= total - result.start);
            let end = result.start + (result.len - 1);
            assert_eq!(
                range::parse(&format!("bytes={}-{end}", result.start), total),
                Ok(result)
            );
        }
    }
});
