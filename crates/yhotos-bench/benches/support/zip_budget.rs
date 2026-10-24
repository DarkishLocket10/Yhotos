use std::time::Duration;

/// Enforces STORE throughput and the maintainer's under-20%-of-a-core CPU limit.
#[allow(clippy::cast_precision_loss)]
pub fn within_budget(bytes: u64, wall: Duration, cpu: Duration) -> bool {
    !wall.is_zero()
        && bytes as f64 / wall.as_secs_f64() >= 200_000_000.0
        && cpu.as_secs_f64() / wall.as_secs_f64() < 0.20
}
