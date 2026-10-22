#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|bytes: &[u8]| {
    let _ = yhotos_codec::validate_gain_map_metadata(bytes);
});
