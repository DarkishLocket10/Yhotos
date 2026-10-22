//! Independent token buckets keyed only by validated client IP and route class.

use crate::error::HttpError;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dashmap::{DashMap, mapref::entry::Entry};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
use yhotos_config::RateLimits;

// Shared by all route classes. New keys fail closed until idle entries expire.
const MAX_BUCKETS: usize = 16_384;
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_TTL: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Class {
    Cheap,
    Expensive,
    Auth,
}
fn class(path: &str) -> Class {
    if path.starts_with("/login")
        || path.starts_with("/u/security")
        || path.starts_with("/admin/login")
        || path.starts_with("/admin/u/security")
        || matches!(
            path,
            "/u/password" | "/admin/u/password" | "/logout" | "/admin/logout"
        )
        || (path.starts_with("/s/") && path.ends_with("/unlock"))
    {
        Class::Auth
    } else if matches!(path, "/search" | "/zip")
        || path.starts_with("/o/")
        || path.starts_with("/v/")
    {
        Class::Expensive
    } else {
        Class::Cheap
    }
}
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// Bounds per-router buckets across all IPs and classes; idle storage is reclaimed.
pub struct RateLimiter {
    buckets: DashMap<(IpAddr, Class), Bucket>,
    // Counts live entries plus slots reserved by pending insertions.
    entries: AtomicUsize,
    budgets: RateLimits,
    trust_proxy: bool,
}
impl RateLimiter {
    pub(crate) fn new(budgets: RateLimits, trust_proxy: bool) -> Arc<Self> {
        let state = Arc::new(Self {
            buckets: DashMap::new(),
            entries: AtomicUsize::new(0),
            budgets,
            trust_proxy,
        });
        let weak = Arc::downgrade(&state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                // Scanning and allocator work stay off the request executor.
                if let Err(error) = tokio::task::spawn_blocking(move || state.sweep()).await {
                    tracing::error!(%error, "rate-limit storage sweep failed");
                }
            }
        });
        state
    }
    fn sweep(&self) {
        let now = Instant::now();
        let mut removed = 0;
        self.buckets.retain(|_, bucket| {
            let keep = now.saturating_duration_since(bucket.updated) < IDLE_TTL;
            if !keep {
                removed += 1;
            }
            keep
        });
        // Release reservations only after removal. Concurrent insertions may
        // briefly see a full table, but can never overfill it.
        self.entries.fetch_sub(removed, Ordering::Relaxed);
        self.buckets.shrink_to_fit();
    }
    fn check(&self, ip: IpAddr, class: Class) -> Option<u64> {
        self.check_with_clock(ip, class, Instant::now)
    }
    fn check_with_clock(
        &self,
        ip: IpAddr,
        class: Class,
        clock: impl FnOnce() -> Instant,
    ) -> Option<u64> {
        let capacity = match class {
            Class::Cheap => self.budgets.cheap_per_min,
            Class::Expensive => self.budgets.expensive_per_min,
            Class::Auth => self.budgets.auth_per_min,
        };
        let (mut bucket, now) = if let Some(bucket) = self.buckets.get_mut(&(ip, class)) {
            (bucket, clock())
        } else {
            // DashMap::entry may allocate even before insertion. Reserve a slot
            // first, so rejected keys cannot grow the underlying hash tables.
            if self
                .entries
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < MAX_BUCKETS).then_some(count + 1)
                })
                .is_err()
            {
                return Some(SWEEP_INTERVAL.as_secs());
            }
            match self.buckets.entry((ip, class)) {
                Entry::Occupied(entry) => {
                    // Another request inserted this key after the lookup.
                    self.entries.fetch_sub(1, Ordering::Relaxed);
                    (entry.into_ref(), clock())
                }
                Entry::Vacant(entry) => {
                    let now = clock();
                    (
                        entry.insert(Bucket {
                            tokens: f64::from(capacity),
                            updated: now,
                        }),
                        now,
                    )
                }
            }
        };
        // Read the refill clock only under the entry lock. Never let a stale
        // timestamp rewind accounting and credit the same interval twice.
        let now = now.max(bucket.updated);
        bucket.tokens = (bucket.tokens
            + now.duration_since(bucket.updated).as_secs_f64() * f64::from(capacity) / 60.0)
            .min(f64::from(capacity));
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            // Capacity is positive after configuration validation; bound the
            // duration without a lossy float-to-integer cast.
            Some(
                Duration::from_secs_f64((1.0 - bucket.tokens) * 60.0 / f64::from(capacity))
                    .as_secs()
                    + 1,
            )
        }
    }
}
pub(crate) fn client_ip(request: &Request, trust_proxy: bool) -> Result<IpAddr, HttpError> {
    if trust_proxy {
        let mut values = request.headers().get_all("cf-connecting-ip").iter();
        let ip = values
            .next()
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .ok_or(HttpError::BadRequest)?;
        if values.next().is_some() {
            return Err(HttpError::BadRequest);
        }
        Ok(ip)
    } else {
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.ip())
            .ok_or(HttpError::BadRequest)
    }
}
#[derive(Clone, Copy)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

pub(crate) async fn limit(
    State(state): State<Arc<RateLimiter>>,
    mut request: Request,
    next: Next,
) -> Response {
    let ip = match client_ip(&request, state.trust_proxy) {
        Ok(ip) => ip,
        Err(error) => return error.into_response(),
    };
    let route_class = class(request.uri().path());
    let route_class = if route_class == Class::Auth && request.method() != axum::http::Method::POST
    {
        Class::Cheap
    } else {
        route_class
    };
    if let Some(seconds) = state.check(ip, route_class) {
        let mut response = HttpError::RateLimited.into_response();
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    // Both original and video bodies use the validated IP for stream accounting.
    if request.uri().path().starts_with("/o/") || request.uri().path().starts_with("/v/") {
        request.extensions_mut().insert(ClientIp(ip));
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Barrier;

    fn test_limiter() -> Arc<RateLimiter> {
        Arc::new(RateLimiter {
            buckets: DashMap::new(),
            entries: AtomicUsize::new(0),
            budgets: RateLimits {
                cheap_per_min: 2,
                expensive_per_min: 2,
                auth_per_min: 2,
            },
            trust_proxy: false,
        })
    }
    fn ip(index: usize) -> IpAddr {
        std::net::Ipv6Addr::from(u128::try_from(index).unwrap() + 1).into()
    }

    #[tokio::test(start_paused = true)]
    async fn refills_sweeps_and_separates_all_budgets() {
        let state = RateLimiter::new(
            RateLimits {
                cheap_per_min: 1,
                expensive_per_min: 2,
                auth_per_min: 3,
            },
            false,
        );
        let ip = "127.0.0.1".parse().unwrap();
        for (class, count) in [(Class::Cheap, 1), (Class::Expensive, 2), (Class::Auth, 3)] {
            for _ in 0..count {
                assert_eq!(state.check(ip, class), None);
            }
            assert!(state.check(ip, class).is_some());
        }
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(state.check(ip, Class::Cheap), None);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(181)).await;
        tokio::task::yield_now().await;
        state.sweep();
        assert!(state.buckets.is_empty());
        assert_eq!(state.entries.load(Ordering::Relaxed), 0);
        assert_eq!(state.buckets.capacity(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn rotating_ipv6_clients_are_bounded_across_all_classes_and_fail_closed() {
        use axum::{Router, body::Body, http::StatusCode, routing::get};
        use tower::ServiceExt;

        let state = test_limiter();
        for index in 0..MAX_BUCKETS {
            let class = match index % 3 {
                0 => Class::Cheap,
                1 => Class::Expensive,
                _ => Class::Auth,
            };
            assert_eq!(state.check(ip(index), class), None);
        }
        let capacity = state.buckets.capacity();
        for index in MAX_BUCKETS..MAX_BUCKETS * 2 {
            for class in [Class::Cheap, Class::Expensive, Class::Auth] {
                assert_eq!(state.check(ip(index), class), Some(60));
            }
        }
        assert_eq!(state.buckets.len(), MAX_BUCKETS);
        assert_eq!(state.entries.load(Ordering::Relaxed), MAX_BUCKETS);
        assert_eq!(state.buckets.capacity(), capacity);
        // Existing keys retain their budgets even when no new key can enter.
        assert_eq!(state.check(ip(0), Class::Cheap), None);
        assert!(state.check(ip(0), Class::Cheap).is_some());
        assert_eq!(state.check(ip(0), Class::Auth), Some(60));

        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, limit));
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = axum::http::Uri::from_static("/healthz");
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip(MAX_BUCKETS), 1)));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "60");
    }

    #[test]
    fn simultaneous_new_clients_cannot_overfill_the_last_slots() {
        let state = test_limiter();
        for index in 0..MAX_BUCKETS - 4 {
            assert_eq!(state.check(ip(index), Class::Cheap), None);
        }
        let barrier = Barrier::new(16);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|index| {
                    let state = &state;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        state.check(ip(MAX_BUCKETS + index), Class::Cheap)
                    })
                })
                .collect();
            let admitted = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(Option::is_none)
                .count();
            assert_eq!(admitted, 4);
        });
        assert_eq!(state.buckets.len(), MAX_BUCKETS);
        assert_eq!(state.entries.load(Ordering::Relaxed), MAX_BUCKETS);
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_reclaims_capacity_preserves_active_budgets_and_reuses_slots() {
        let state = test_limiter();
        for index in 0..MAX_BUCKETS {
            assert_eq!(state.check(ip(index), Class::Cheap), None);
        }
        let full_capacity = state.buckets.capacity();
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(state.check(ip(0), Class::Cheap), None);
        assert_eq!(state.check(ip(0), Class::Cheap), None);
        assert!(state.check(ip(0), Class::Cheap).is_some());
        tokio::time::advance(Duration::from_secs(60)).await;
        state.sweep();
        assert_eq!(state.buckets.len(), 1);
        assert_eq!(state.entries.load(Ordering::Relaxed), 1);
        assert!(state.buckets.capacity() < full_capacity);
        let survivor = state.buckets.get(&(ip(0), Class::Cheap)).unwrap();
        assert_eq!(survivor.tokens, 0.0);
        drop(survivor);
        for index in MAX_BUCKETS..MAX_BUCKETS * 2 - 1 {
            assert_eq!(state.check(ip(index), Class::Cheap), None);
        }
        assert_eq!(state.buckets.len(), MAX_BUCKETS);
        assert_eq!(state.entries.load(Ordering::Relaxed), MAX_BUCKETS);
        assert_eq!(state.check(ip(MAX_BUCKETS * 2), Class::Cheap), Some(60));
        tokio::time::advance(Duration::from_secs(120)).await;
        state.sweep();
        assert!(state.buckets.is_empty());
        assert_eq!(state.entries.load(Ordering::Relaxed), 0);
        assert_eq!(state.buckets.capacity(), 0);
        assert_eq!(state.check(ip(MAX_BUCKETS * 2), Class::Cheap), None);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_request_cannot_rewind_refill_time_or_credit_it_twice() {
        let state = test_limiter();
        let ip = ip(0);
        let arrived = Instant::now();
        assert_eq!(state.check_with_clock(ip, Class::Cheap, || arrived), None);
        assert_eq!(state.check_with_clock(ip, Class::Cheap, || arrived), None);

        let ready = Barrier::new(2);
        let resume = Barrier::new(2);
        std::thread::scope(|scope| {
            let delayed = scope.spawn(|| {
                ready.wait();
                resume.wait();
                state.check_with_clock(ip, Class::Cheap, || {
                    // The clock must be sampled while the shard is locked.
                    assert!(state.buckets.try_get_mut(&(ip, Class::Cheap)).is_locked());
                    // Force the old arrival timestamp to exercise the monotonic
                    // guard even under the reviewer's original interleaving.
                    arrived
                })
            });
            ready.wait();
            // A later arrival spends the token refilled over the first 30 s.
            assert_eq!(
                state.check_with_clock(ip, Class::Cheap, || arrived + Duration::from_secs(30)),
                None
            );
            resume.wait();
            assert!(delayed.join().unwrap().is_some());
        });
        assert_eq!(
            state.buckets.get(&(ip, Class::Cheap)).unwrap().updated,
            arrived + Duration::from_secs(30)
        );
        // Only half a token has refilled at 45 s. Rewinding to the old arrival
        // would credit 45 s again and incorrectly admit this request.
        assert!(
            state
                .check_with_clock(ip, Class::Cheap, || arrived + Duration::from_secs(45))
                .is_some()
        );
        assert_eq!(
            state.check_with_clock(ip, Class::Cheap, || arrived + Duration::from_secs(60)),
            None
        );
    }
}
