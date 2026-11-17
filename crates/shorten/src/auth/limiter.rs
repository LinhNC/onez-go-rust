//! Bounded fixed-window rate limiter.
//!
//! Login and mutation endpoints are limited per IP with the same fixed window
//! the previous implementation used. The entry map is capped, and the oldest
//! entry is evicted when the cap is exceeded so an attacker cannot grow the
//! table by spraying source addresses.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

#[derive(Clone, Copy, Debug)]
struct Bucket {
    count: u32,
    reset_ms: i64,
    seen_ms: i64,
}

/// Fixed-window limiter keyed by an opaque bucket key, usually a client IP.
pub struct Limiter {
    inner: Mutex<Inner>,
    limit: u32,
    window_ms: i64,
    capacity: usize,
}

#[derive(Debug)]
struct Inner {
    entries: HashMap<String, Bucket>,
}

impl Limiter {
    #[must_use]
    pub fn new(limit: u32, window: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
            }),
            limit,
            window_ms: i64::try_from(window.as_millis()).unwrap_or(i64::MAX),
            capacity: capacity.max(1),
        }
    }

    /// Grant or deny one request for `key` at time `now`.
    #[must_use]
    pub fn allow(&self, key: &str, now: DateTime<Utc>) -> bool {
        let now_ms = now.timestamp_millis();
        let mut inner = self.inner.lock().expect("limiter state");
        let mut bucket = match inner.entries.get(key).copied() {
            Some(bucket) if now_ms < bucket.reset_ms => bucket,
            _ => Bucket {
                count: 0,
                reset_ms: now_ms.saturating_add(self.window_ms),
                seen_ms: now_ms,
            },
        };
        let allowed = bucket.count < self.limit;
        if allowed {
            bucket.count += 1;
        }
        bucket.seen_ms = now_ms;
        inner.entries.insert(key.to_string(), bucket);
        if inner.entries.len() > self.capacity {
            let oldest = inner
                .entries
                .iter()
                .min_by_key(|(_, bucket)| bucket.seen_ms)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                inner.entries.remove(&key);
            }
        }
        allowed
    }

    /// Number of tracked keys, exposed for the metrics endpoint and tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("limiter state").entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).expect("in range")
    }

    #[test]
    fn counts_requests_within_window() {
        let limiter = Limiter::new(3, Duration::from_secs(60), 16);
        for index in 0..3 {
            assert!(limiter.allow("ip", at(100 + index)));
        }
        assert!(!limiter.allow("ip", at(103)));
    }

    #[test]
    fn evicts_oldest_when_over_capacity() {
        let limiter = Limiter::new(1, Duration::from_secs(60), 2);
        let _ = limiter.allow("a", at(1_000));
        let _ = limiter.allow("b", at(2_000));
        let _ = limiter.allow("c", at(3_000));
        assert_eq!(limiter.len(), 2);
        // "a" was evicted, so it starts a fresh window.
        assert!(limiter.allow("a", at(4_000)));
    }

    #[test]
    fn keys_are_independent() {
        let limiter = Limiter::new(1, Duration::from_secs(60), 8);
        assert!(limiter.allow("one", at(0)));
        assert!(limiter.allow("two", at(0)));
        assert!(!limiter.allow("one", at(1)));
    }
}
