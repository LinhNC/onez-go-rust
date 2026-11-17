//! JSON wire helpers.
//!
//! Timestamps on the wire use the shape the previous implementation produced:
//! RFC 3339 in UTC with a fractional part only when needed and trailing zeros
//! trimmed. Keeping that format means API clients, the KV exporter and the
//! console all see identical payloads from either implementation.

use chrono::{DateTime, Timelike, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

/// Format an instant exactly as Go's `time.Time` JSON encoding does.
#[must_use]
pub fn format_go_time(value: DateTime<Utc>) -> String {
    let base = value.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = value.nanosecond();
    if nanos == 0 {
        return format!("{base}Z");
    }
    let fraction = format!("{nanos:09}");
    let fraction = fraction.trim_end_matches('0');
    format!("{base}.{fraction}Z")
}

struct GoTime(DateTime<Utc>);

impl std::fmt::Display for GoTime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&format_go_time(self.0))
    }
}

/// Serialise an instant using the shared timestamp format.
pub mod time {
    use super::*;

    /// Serialize an instant using the shared timestamp format.
    pub fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&GoTime(*value))
    }

    /// Deserialize an RFC 3339 timestamp, accepting offsets as well as `Z`.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_rfc3339(&text).ok_or_else(|| D::Error::custom("expected an RFC3339 timestamp"))
    }
}

/// Serialise an optional instant, emitting `null` when absent.
pub mod nullable_time {
    use super::*;

    /// Serialize an optional instant, emitting `null` when absent.
    pub fn serialize<S: Serializer>(
        value: &Option<DateTime<Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.collect_str(&GoTime(*value)),
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize an optional instant; null and missing values are absent.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?;
        match text {
            None => Ok(None),
            Some(text) => parse_rfc3339(&text)
                .map(Some)
                .ok_or_else(|| D::Error::custom("expected an RFC3339 timestamp")),
        }
    }
}

/// Parse an RFC 3339 timestamp the way the API endpoints accepted them.
#[must_use]
pub fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    // Go parsed these with `time.Parse(time.RFC3339, …)`, whose layout is
    // `2006-01-02T15:04:05Z07:00`: a `T` separator and a zone that is exactly
    // `Z` or `±hh:mm`. Chrono also accepts a space separator and the compact
    // `+0700` offset, so the skeleton and the zone are checked here first.
    let skeleton_ok = value.len() >= 20
        && value.as_bytes()[10] == b'T'
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value.as_bytes()[13] == b':'
        && value.as_bytes()[16] == b':';
    let zone_ok = match value.as_bytes().last() {
        Some(b'Z') => value.len() >= 20,
        Some(last) if last.is_ascii_digit() => {
            let zone = value.get(value.len().saturating_sub(6)..)?;
            let bytes = zone.as_bytes();
            matches!(bytes.first(), Some(b'+' | b'-'))
                && bytes[1].is_ascii_digit()
                && bytes[2].is_ascii_digit()
                && bytes[3] == b':'
                && bytes[4].is_ascii_digit()
                && bytes[5].is_ascii_digit()
        }
        _ => false,
    };
    if !(skeleton_ok && zone_ok) {
        return None;
    }
    DateTime::parse_from_rfc3339(value).ok().map(Into::into)
}

/// Format a timestamp for the console's `datetime-local` inputs.
#[must_use]
pub fn format_local_input(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|value| value.format("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default()
}

/// Parse the `datetime-local` form value. The layout carries no zone, and the
/// previous service read it as UTC.
#[must_use]
pub fn parse_local_input(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
        .ok()
        .map(|value| value.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde::Serialize;

    #[test]
    fn trims_fractional_zeros_like_go() {
        let exact = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        assert_eq!(format_go_time(exact), "2026-08-01T10:00:00Z");
        let millis = exact + chrono::Duration::milliseconds(120);
        assert_eq!(format_go_time(millis), "2026-08-01T10:00:00.12Z");
        let precise = exact + chrono::Duration::milliseconds(5);
        assert_eq!(format_go_time(precise), "2026-08-01T10:00:00.005Z");
    }

    #[test]
    fn parses_offsets_and_fractional_seconds() {
        assert!(parse_rfc3339("2026-08-01T10:00:00Z").is_some());
        assert!(
            parse_rfc3339("2026-08-01T10:00:00+0700").is_none(),
            "Go's RFC3339 layout rejects the compact offset"
        );
        assert!(
            parse_rfc3339("2026-08-01 10:00:00Z").is_none(),
            "Go's RFC3339 layout requires the T separator"
        );
        assert!(
            parse_rfc3339("2026-08-01T10:00:00z").is_none(),
            "only the upper-case Z zone is accepted"
        );
        assert!(parse_rfc3339("2026-08-01T10:00:00.250+02:00").is_some());
        assert!(parse_rfc3339("2026-08-01T12:00:00+02:00").is_some());
        assert!(parse_rfc3339("2026-08-01T10:00:00.250Z").is_some());
        assert!(parse_rfc3339("not-a-time").is_none());
    }

    #[test]
    fn round_trips_nullable_time_through_json() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Sample {
            #[serde(with = "nullable_time")]
            expires_at: Option<DateTime<Utc>>,
        }
        let value = Sample { expires_at: None };
        assert_eq!(
            serde_json::to_string(&value).expect("json"),
            r#"{"expires_at":null}"#
        );
        let exact = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let value = Sample {
            expires_at: Some(exact),
        };
        let encoded = serde_json::to_string(&value).expect("json");
        assert_eq!(encoded, r#"{"expires_at":"2026-08-01T10:00:00Z"}"#);
        assert_eq!(
            serde_json::from_str::<Sample>(&encoded).expect("decode"),
            value
        );
    }

    #[test]
    fn formats_console_datetime_local_value() {
        let value = Utc.with_ymd_and_hms(2026, 8, 1, 10, 30, 0).unwrap();
        assert_eq!(format_local_input(Some(value)), "2026-08-01T10:30");
        assert_eq!(format_local_input(None), "");
        assert_eq!(parse_local_input("2026-08-01T10:30"), Some(value));
        assert_eq!(parse_local_input("2026-08-01T10:30:00"), None);
        assert_eq!(parse_local_input(""), None);
    }
}
