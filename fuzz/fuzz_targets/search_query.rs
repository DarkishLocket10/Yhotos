#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
#[path = "../../crates/yhotos-http/src/search/query.rs"]
mod query;
fuzz_target!(|data: &str| {
    if let Ok(query) = query::parse(data) {
        assert!(data.chars().count() <= 256);
        assert!(query.filters.len() <= 8);
        assert!(!query.text.chars().any(char::is_control));
        assert!(query.filters.iter().all(|f| f.key != "person"));
    }
});
