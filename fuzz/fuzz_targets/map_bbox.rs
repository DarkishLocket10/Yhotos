#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
#[path = "../../crates/yhotos-db/src/map_bounds.rs"]
mod bounds;
fuzz_target!(|data: &str| {
    if let Some(b) = bounds::Bounds::parse(data) {
        assert!(b.west < b.east && b.south < b.north);
        assert!(b.west >= -180.0 && b.east <= 180.0);
        assert!(b.south >= -85.051_129 && b.north <= 85.051_129);
        assert_eq!(bounds::Bounds::parse(&format!("{},{},{},{}",b.west,b.south,b.east,b.north)),Some(b));
    }
});
