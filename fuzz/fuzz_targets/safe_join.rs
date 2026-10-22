#![no_main]
#![forbid(unsafe_code)]

use std::str::FromStr;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
// Minimal boundary types used by the included production modules. Parsing and
// path construction themselves are compiled from the storage sources below.
#[derive(Debug)]
pub enum StorageError {
    InvalidHash,
    InvalidName,
}

#[allow(dead_code)]
pub enum Area {
    Originals,
    Derivatives,
    Video,
}

#[path = "../../crates/yhotos-storage/src/hash.rs"]
mod hash;
use hash::ContentHash;

// Compile the private production path implementation directly: fuzzing does not
// require making arbitrary path construction part of the storage public API.
#[path = "../../crates/yhotos-storage/src/paths.rs"]
#[allow(dead_code)]
mod paths;

fuzz_target!(|data: &[u8]| {
    static ROOT: OnceLock<std::path::PathBuf> = OnceLock::new();
    let root = ROOT.get_or_init(|| std::env::temp_dir().canonicalize().unwrap());
    let mut arbitrary = arbitrary::Unstructured::new(data);
    let inputs = arbitrary.arbitrary::<Vec<&[u8]>>().unwrap_or_default();
    check_components(root, inputs.into_iter());
    check_components(root, data.split(|byte| *byte == 0));
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(hash) = ContentHash::from_str(text) {
            assert_eq!(hash.to_string(), text);
            assert_eq!(text.len(), 64);
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
        }
    }
});

fn check_components<'a>(root: &std::path::Path, inputs: impl Iterator<Item = &'a [u8]>) {
    let components: Result<Vec<_>, _> = inputs
        .map(|bytes| {
            let component = std::str::from_utf8(bytes).map_err(|_| StorageError::InvalidName)?;
            paths::Component::parse(component)
        })
        .collect();
    if let Ok(components) = components {
        let joined = paths::safe_join(root, components);
        assert!(joined.starts_with(root));
        assert!(
            joined
                .components()
                .all(|part| !matches!(part, std::path::Component::ParentDir))
        );
    }
}
