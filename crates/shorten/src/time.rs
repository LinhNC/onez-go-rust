//! Time helpers.
//!
//! The database stores every timestamp as an integer number of milliseconds in
//! UTC. The helpers here centralise the conversions so the store, the session
//! tokens and the JSON envelope all agree.

use chrono::{DateTime, Duration, TimeZone, Utc};

/// Milliseconds since the Unix epoch for the current instant.
#[must_use]
pub fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// Current time truncated to whole milliseconds, matching the previous
/// implementation which truncated on write so stored values are exact.
#[must_use]
pub fn now_truncated() -> DateTime<Utc> {
    truncate_millis(Utc::now())
}

#[must_use]
pub fn truncate_millis(value: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(value.timestamp_millis()).unwrap_or(value)
}

/// Convert stored milliseconds into a UTC timestamp.
#[must_use]
pub fn from_millis(value: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(value).single().unwrap_or_else(|| {
        // Out-of-range values clamp to the epoch, which keeps a corrupt row
        // readable instead of aborting the request.
        Utc.timestamp_millis_opt(0)
            .single()
            .expect("zero is in range")
    })
}

/// Whole seconds between `now` and `target`, matching `time.Until` truncation.
#[must_use]
pub fn seconds_until(target: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    (target - now).num_seconds()
}

/// Duration in whole minutes, matching Go's `Duration.Minutes()` truncation.
#[must_use]
pub fn whole_minutes(value: Duration) -> i64 {
    value.num_minutes()
}

/// Duration in whole hours, matching Go's `Duration.Hours()` truncation.
#[must_use]
pub fn whole_hours(value: Duration) -> i64 {
    value.num_hours()
}
