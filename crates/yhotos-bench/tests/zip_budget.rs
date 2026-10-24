#![forbid(unsafe_code)]
//! Regression coverage for the production-router ZIP benchmark's hard limits.
#[path = "../benches/support/zip_budget.rs"]
mod zip_budget;
use std::time::Duration;

#[test]
fn zip_budget_rejects_cpu_or_throughput_regressions() {
    let wall = Duration::from_secs(1);
    assert!(zip_budget::within_budget(
        200_000_000,
        wall,
        Duration::from_millis(199)
    ));
    assert!(!zip_budget::within_budget(
        200_000_000,
        wall,
        Duration::from_millis(200)
    ));
    assert!(zip_budget::within_budget(
        203_300_000,
        wall,
        Duration::from_micros(177_700)
    ));
    assert!(!zip_budget::within_budget(
        199_999_999,
        wall,
        Duration::from_millis(50)
    ));
    assert!(!zip_budget::within_budget(
        0,
        Duration::ZERO,
        Duration::ZERO
    ));
}
