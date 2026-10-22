use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A UTC instant expressed as a nonnegative duration since the Unix epoch.
/// Callers validate and convert wall-clock timestamps at their boundary.
pub type Timestamp = Duration;

/// Idle and absolute lifetimes; the absolute limit always takes precedence.
#[derive(Debug, Clone, Copy)]
pub struct SessionPolicy {
    /// Maximum time since the last observed request.
    pub idle: Duration,
    /// Maximum time since session creation, regardless of activity.
    pub absolute: Duration,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            idle: Duration::from_hours(14 * 24),
            absolute: Duration::from_hours(90 * 24),
        }
    }
}

impl SessionPolicy {
    /// Admin sessions expire after 12 idle hours or seven absolute days.
    #[must_use]
    pub const fn admin() -> Self {
        Self {
            idle: Duration::from_hours(12),
            absolute: Duration::from_hours(7 * 24),
        }
    }
}

/// Expiry decisions are deterministic and consider the exact boundary expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Both lifetimes are still valid.
    Active,
    /// The idle lifetime has elapsed.
    ExpiredIdle,
    /// The absolute lifetime elapsed or timestamps are inconsistent.
    ExpiredAbsolute,
}

/// Fails closed for future or out-of-order timestamps and never extends absolute expiry.
#[must_use]
pub fn session_state(
    created_at: Timestamp,
    last_seen_at: Timestamp,
    now: Timestamp,
    policy: SessionPolicy,
) -> SessionState {
    if created_at > last_seen_at
        || last_seen_at > now
        || now.saturating_sub(created_at) >= policy.absolute
    {
        SessionState::ExpiredAbsolute
    } else if now.saturating_sub(last_seen_at) >= policy.idle {
        SessionState::ExpiredIdle
    } else {
        SessionState::Active
    }
}

/// Exponential backoff after a fixed number of failures.
#[derive(Debug, Clone, Copy)]
pub struct LockoutPolicy {
    /// Failures permitted before the first delay.
    pub free_attempts: u32,
    /// Delay for the first failure after the free attempts.
    pub base_delay: Duration,
    /// Maximum delay for any failure.
    pub max_delay: Duration,
}

impl Default for LockoutPolicy {
    fn default() -> Self {
        Self {
            free_attempts: 5,
            base_delay: Duration::from_secs(30),
            max_delay: Duration::from_hours(1),
        }
    }
}

/// Persist per-username state server-side; update it atomically with authentication.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockoutState {
    failures: u32,
    locked_until: Option<Timestamp>,
}

/// Authentication cannot proceed before this instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("authentication temporarily locked")]
pub struct LockedUntil(pub Timestamp);

impl LockoutState {
    /// Rejects an attempt strictly before the current lockout deadline.
    ///
    /// # Errors
    /// Returns the deadline while a lockout is in force.
    pub fn check(&self, now: Timestamp) -> Result<(), LockedUntil> {
        if let Some(until) = self.locked_until.filter(|until| now < *until) {
            return Err(LockedUntil(until));
        }
        Ok(())
    }

    /// Records a failure with delay = base * 2^(failures - free - 1), capped at max.
    /// Failure counts and deadlines saturate instead of wrapping or panicking.
    pub fn record_failure(&mut self, now: Timestamp, policy: LockoutPolicy) {
        self.failures = self.failures.saturating_add(1);
        if self.failures <= policy.free_attempts {
            return;
        }
        let exponent = self.failures - policy.free_attempts - 1;
        let mut delay = policy.base_delay.min(policy.max_delay);
        if !delay.is_zero() {
            for _ in 0..exponent {
                if delay >= policy.max_delay {
                    break;
                }
                delay = delay.saturating_mul(2).min(policy.max_delay);
            }
        }
        let deadline = now.saturating_add(delay);
        // An out-of-order clock or overlapping attempt must not shorten a lock.
        self.locked_until = Some(self.locked_until.map_or(deadline, |old| old.max(deadline)));
    }

    /// Clears both the failure counter and lockout after successful authentication.
    pub fn record_success(&mut self) {
        *self = Self::default();
    }

    /// Returns the saturated number of failures since the last success.
    #[must_use]
    pub const fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> Timestamp {
        Duration::from_secs(seconds)
    }

    #[test]
    fn lockout_table_and_reset() {
        let mut state = LockoutState::default();
        let policy = LockoutPolicy::default();
        let mut now = at(100);
        for seconds in [0, 0, 0, 0, 0, 30, 60, 120, 240, 480, 960, 1920, 3600, 3600] {
            assert_eq!(state.check(now), Ok(()));
            state.record_failure(now, policy);
            if seconds == 0 {
                assert_eq!(state.check(now), Ok(()));
            } else {
                let until = now + at(seconds);
                assert_eq!(state.check(now), Err(LockedUntil(until)));
                assert_eq!(
                    state.check(until.saturating_sub(Duration::from_nanos(1))),
                    Err(LockedUntil(until))
                );
                assert_eq!(state.check(until), Ok(()));
                now = until;
            }
        }
        state.record_success();
        assert_eq!(state.failures(), 0);
        assert_eq!(state.check(Duration::ZERO), Ok(()));
        state.record_failure(now, policy);
        assert_eq!(state.check(now), Ok(()));
    }

    #[test]
    fn extreme_policies_and_counters_are_bounded() {
        let mut state = LockoutState {
            failures: u32::MAX,
            locked_until: None,
        };
        state.record_failure(Duration::MAX, LockoutPolicy::default());
        assert_eq!(state.failures(), u32::MAX);
        assert_eq!(state.check(Duration::ZERO), Err(LockedUntil(Duration::MAX)));
        let policy = LockoutPolicy {
            free_attempts: 0,
            base_delay: Duration::from_nanos(1),
            max_delay: at(3600),
        };
        state.record_success();
        state.record_failure(at(0), policy);
        assert_eq!(
            state.check(at(0)),
            Err(LockedUntil(Duration::from_nanos(1)))
        );
        state.failures = u32::MAX;
        state.record_failure(at(0), policy);
        assert_eq!(state.check(at(0)), Err(LockedUntil(at(3600))));
        state.record_success();
        state.record_failure(
            at(0),
            LockoutPolicy {
                base_delay: Duration::ZERO,
                ..policy
            },
        );
        assert_eq!(state.check(at(0)), Ok(()));
    }

    #[test]
    fn backwards_clock_cannot_shorten_lockout() {
        let mut state = LockoutState::default();
        let policy = LockoutPolicy {
            free_attempts: 0,
            ..LockoutPolicy::default()
        };
        state.record_failure(at(1000), policy);
        state.record_failure(at(0), policy);
        assert_eq!(state.check(at(1000)), Err(LockedUntil(at(1030))));
    }

    #[test]
    fn session_boundaries_and_absolute_precedence() {
        let policy = SessionPolicy {
            idle: at(10),
            absolute: at(100),
        };
        for (created, seen, now, expected) in [
            (0, 0, 9, SessionState::Active),
            (0, 0, 10, SessionState::ExpiredIdle),
            (0, 95, 99, SessionState::Active),
            (0, 100, 100, SessionState::ExpiredAbsolute),
            (0, 0, 100, SessionState::ExpiredAbsolute),
            (20, 10, 30, SessionState::ExpiredAbsolute),
            (0, 40, 30, SessionState::ExpiredAbsolute),
        ] {
            assert_eq!(
                session_state(at(created), at(seen), at(now), policy),
                expected
            );
        }
        for policy in [SessionPolicy::default(), SessionPolicy::admin()] {
            assert_eq!(
                session_state(at(0), at(0), policy.idle, policy),
                SessionState::ExpiredIdle
            );
            assert_eq!(
                session_state(at(0), policy.absolute, policy.absolute, policy),
                SessionState::ExpiredAbsolute
            );
        }
        assert_eq!(SessionPolicy::default().idle, at(14 * 86400));
        assert_eq!(SessionPolicy::default().absolute, at(90 * 86400));
        assert_eq!(SessionPolicy::admin().idle, at(12 * 3600));
        assert_eq!(SessionPolicy::admin().absolute, at(7 * 86400));
    }
}
