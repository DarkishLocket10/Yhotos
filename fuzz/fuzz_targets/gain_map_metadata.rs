#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
#[path = "../../crates/yhotos-codec/test-support/gain_mutations.rs"]
mod gain_mutations;
use std::sync::OnceLock;
static MUTATIONS: OnceLock<Vec<gain_mutations::Case>> = OnceLock::new();
fuzz_target!(|bytes: &[u8]| {
    let _ = yhotos_codec::validate_gain_map_metadata(bytes);
    let cases = MUTATIONS.get_or_init(gain_mutations::cases);
    if let [lo, hi, rest @ ..] = bytes {
        let index = usize::from(u16::from_le_bytes([*lo, *hi])) % cases.len();
        if let Some(case) = cases.get(index) {
            let _ = yhotos_codec::validate_gain_map_metadata(&case.bytes);
            let mut mutated = case.bytes.clone();
            // Keep container/primary framing from the deterministic corpus and
            // explore nearby bytes, lengths and payload truncations as well.
            if let [a, b, value, mode, ..] = rest {
                let offset = usize::from(u16::from_le_bytes([*a, *b])) % mutated.len();
                if mode & 1 == 0 {
                    if let Some(byte) = mutated.get_mut(offset) {
                        *byte = *value;
                    }
                } else {
                    mutated.truncate(offset);
                }
                let _ = yhotos_codec::validate_gain_map_metadata(&mutated);
            }
        }
    }
});
