//! The text form of Go's `time.Duration`.
//!
//! The benchmark and seed reports print elapsed and percentile durations with
//! `%s`, which renders Go's `Duration.String()`. Rust's own duration display
//! differs ("1.234s" versus "1234ms" is the classic mismatch), so the reports
//! use this formatter to keep the output lines byte-identical.
//!
//! The rules reproduce Go's output:
//!
//! * `0s` for a zero duration.
//! * hours, minutes and whole seconds when the duration reaches a minute,
//!   with the hours part dropped when it is zero (`1m30s`, `1h30m0s`).
//! * a decimal fraction on the largest whole unit below a minute
//!   (`1.234s`, `1.5ms`, `1.5µs`) with trailing zeros trimmed.
//! * plain nanoseconds below a microsecond (`500ns`).

use std::time::Duration;

use crate::cli::go_time_quote;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const NANOS_PER_MINUTE: u128 = 60 * NANOS_PER_SECOND;
const NANOS_PER_HOUR: u128 = 60 * NANOS_PER_MINUTE;
const NANOS_PER_MILLISECOND: u128 = 1_000_000;
const NANOS_PER_MICROSECOND: u128 = 1_000;
const MAX_GO_DURATION: u128 = 1 << 63;

/// `time.ParseDuration`'s grammar: `[-+]?([0-9]*(\.[0-9]*)?[a-z]+)+` over the
/// units `ns`, `us` (or `µs` / `μs`), `ms`, `s`, `m`, `h`, with Go's overflow
/// limits and Go's error texts (`time: invalid duration "…"`,
/// `time: missing unit in duration "…"`, `time: unknown unit "…" in duration "…"`).
///
/// The result is signed nanoseconds, so `-1s` is `-1_000_000_000` and the
/// programs can apply Go's `*duration <= 0` check to it.
///
/// # Errors
///
/// Returns the message Go's `time.ParseDuration` returned for `text`.
pub fn parse_go_duration(text: &str) -> Result<i128, String> {
    let invalid = || format!("time: invalid duration {}", go_time_quote(text));
    let mut rest = text;
    let mut negative = false;
    match rest.as_bytes().first() {
        Some(b'-') => {
            negative = true;
            rest = &rest[1..];
        }
        Some(b'+') => rest = &rest[1..],
        _ => {}
    }
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err(invalid());
    }

    let mut total: u128 = 0;
    while !rest.is_empty() {
        let first = rest.as_bytes()[0];
        if first != b'.' && !first.is_ascii_digit() {
            return Err(invalid());
        }

        // Consume the whole part: [0-9]*
        let whole_at = rest.len();
        let mut whole: u128 = 0;
        while matches!(rest.as_bytes().first(), Some(byte) if byte.is_ascii_digit()) {
            whole = whole * 10 + u128::from(rest.as_bytes()[0] - b'0');
            rest = &rest[1..];
        }
        let has_whole = rest.len() != whole_at;

        // Consume the optional fraction: (\.[0-9]*)?
        let mut fraction: u128 = 0;
        let mut scale: f64 = 1.0;
        let mut has_fraction = false;
        if rest.starts_with('.') {
            rest = &rest[1..];
            let fraction_at = rest.len();
            while matches!(rest.as_bytes().first(), Some(byte) if byte.is_ascii_digit()) {
                // Go's `leadingFraction` counts the digits in the denominator.
                scale *= 10.0;
                fraction = fraction * 10 + u128::from(rest.as_bytes()[0] - b'0');
                rest = &rest[1..];
            }
            has_fraction = rest.len() != fraction_at;
        }
        if !has_whole && !has_fraction {
            // ".s" and "-.s" carry no digits at all.
            return Err(invalid());
        }

        // Consume the unit: everything up to the next digit or period.
        let unit_at = rest
            .as_bytes()
            .iter()
            .position(|byte| *byte == b'.' || byte.is_ascii_digit())
            .unwrap_or(rest.len());
        if unit_at == 0 {
            return Err(format!(
                "time: missing unit in duration {}",
                go_time_quote(text)
            ));
        }
        let unit = &rest[..unit_at];
        rest = &rest[unit_at..];
        let scale_nanos: u128 = match unit {
            "ns" => 1,
            "us" | "\u{00b5}s" | "\u{03bc}s" => NANOS_PER_MICROSECOND,
            "ms" => NANOS_PER_MILLISECOND,
            "s" => NANOS_PER_SECOND,
            "m" => NANOS_PER_MINUTE,
            "h" => NANOS_PER_HOUR,
            _ => {
                return Err(format!(
                    "time: unknown unit {} in duration {}",
                    go_time_quote(unit),
                    go_time_quote(text)
                ))
            }
        };

        if whole > MAX_GO_DURATION / scale_nanos {
            return Err(invalid());
        }
        let mut part = whole * scale_nanos;
        if fraction > 0 {
            // Go adds the fraction in floating point: fraction * unit / scale.
            #[allow(
                clippy::cast_precision_loss,
                reason = "Go's own formula is floating point"
            )]
            let added = (fraction as f64) * (scale_nanos as f64 / scale);
            part += added as u128;
            if part > MAX_GO_DURATION {
                return Err(invalid());
            }
        }
        total += part;
        if total > MAX_GO_DURATION {
            return Err(invalid());
        }
    }

    if negative {
        return Ok(-i128::try_from(total).unwrap_or(i128::MAX));
    }
    if total > MAX_GO_DURATION - 1 {
        return Err(invalid());
    }
    Ok(i128::try_from(total).unwrap_or(i128::MAX))
}

/// Format a signed nanosecond count with Go's `Duration.String()` layout,
/// prefixing `-` for a negative duration.
#[must_use]
pub fn format_go_nanos_signed(nanos: i128) -> String {
    if nanos < 0 {
        format!(
            "-{}",
            format_go_nanos(u128::try_from(-nanos).unwrap_or(u128::MAX))
        )
    } else {
        format_go_nanos(u128::try_from(nanos).unwrap_or(u128::MAX))
    }
}

/// Go's `Duration.Round(time.Millisecond)` on a signed nanosecond count,
/// reproducing Go's algorithm: halves round away from zero, and a result that
/// would overflow `Duration` clamps to Go's `maxDuration` / `minDuration`.
#[must_use]
pub fn round_milliseconds_nanos(nanos: i128) -> i128 {
    const MILLISECOND: i128 = 1_000_000;
    // Go's `maxDuration` and `minDuration`, spelled as the values themselves.
    const MAX_DURATION: i128 = 9_223_372_036_854_775_807;
    const MIN_DURATION: i128 = -9_223_372_036_854_775_808;
    let mut remainder = nanos % MILLISECOND;
    if nanos < 0 {
        remainder = -remainder;
        if 2 * remainder < MILLISECOND {
            return nanos + remainder;
        }
        let rounded = nanos - MILLISECOND + remainder;
        return if rounded < nanos {
            rounded
        } else {
            MIN_DURATION
        };
    }
    if 2 * remainder < MILLISECOND {
        return nanos - remainder;
    }
    let rounded = nanos + MILLISECOND - remainder;
    if rounded > nanos {
        rounded
    } else {
        MAX_DURATION
    }
}

/// Round to the nearest millisecond, rounding halves away from zero the way
/// Go's `Duration.Round(time.Millisecond)` does.
#[must_use]
pub fn round_milliseconds(value: Duration) -> Duration {
    let nanos = value.as_nanos();
    let remainder = nanos % NANOS_PER_MILLISECOND;
    let whole = nanos - remainder;
    let rounded = if 2 * remainder >= NANOS_PER_MILLISECOND {
        whole + NANOS_PER_MILLISECOND
    } else {
        whole
    };
    Duration::from_nanos(u64::try_from(rounded).unwrap_or(u64::MAX))
}

/// Format a duration with Go's `Duration.String()` layout.
#[must_use]
pub fn format_go_duration(value: Duration) -> String {
    format_go_nanos(value.as_nanos())
}

/// Format a nanosecond count with Go's `Duration.String()` layout.
#[must_use]
pub fn format_go_nanos(nanos: u128) -> String {
    if nanos == 0 {
        return "0s".to_string();
    }
    if nanos >= NANOS_PER_MINUTE {
        let hours = nanos / NANOS_PER_HOUR;
        let rest = nanos % NANOS_PER_HOUR;
        let minutes = rest / NANOS_PER_MINUTE;
        let rest = rest % NANOS_PER_MINUTE;
        let hours_part = if hours == 0 {
            String::new()
        } else {
            format!("{hours}h")
        };
        let seconds_part = decimal(
            rest / NANOS_PER_SECOND,
            rest % NANOS_PER_SECOND,
            "s",
            NINE_DIGITS,
        );
        return format!("{hours_part}{minutes}m{seconds_part}");
    }
    if nanos >= NANOS_PER_SECOND {
        return decimal(
            nanos / NANOS_PER_SECOND,
            nanos % NANOS_PER_SECOND,
            "s",
            NINE_DIGITS,
        );
    }
    if nanos >= NANOS_PER_MILLISECOND {
        return decimal(
            nanos / NANOS_PER_MILLISECOND,
            nanos % NANOS_PER_MILLISECOND,
            "ms",
            SIX_DIGITS,
        );
    }
    if nanos >= NANOS_PER_MICROSECOND {
        return decimal(
            nanos / NANOS_PER_MICROSECOND,
            nanos % NANOS_PER_MICROSECOND,
            "µs",
            THREE_DIGITS,
        );
    }
    format!("{nanos}ns")
}

const NINE_DIGITS: usize = 9;
const SIX_DIGITS: usize = 6;
const THREE_DIGITS: usize = 3;

/// Render `whole.fraction` plus the unit, printing the fraction with `digits`
/// digits and trimming trailing zeros the way Go's formatter does.
fn decimal(whole: u128, fraction: u128, unit: &str, digits: usize) -> String {
    if fraction == 0 {
        return format!("{whole}{unit}");
    }
    let text = format!("{fraction:0digits$}");
    format!("{whole}.{}{unit}", text.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::parse_go_duration as parse;
    use super::{format_go_duration, format_go_nanos, format_go_nanos_signed, round_milliseconds};
    use std::time::Duration;

    const SECOND: i128 = 1_000_000_000;
    const MINUTE: i128 = 60 * SECOND;
    const HOUR: i128 = 60 * MINUTE;

    #[test]
    fn parses_go_duration_grammar() {
        assert_eq!(parse("100ms"), Ok(100_000_000));
        assert_eq!(parse("250ms"), Ok(250_000_000));
        assert_eq!(parse("1.5s"), Ok(1_500_000_000));
        assert_eq!(parse("1.5h"), Ok(5_400_000_000_000));
        assert_eq!(parse("0.5s"), Ok(500_000_000));
        assert_eq!(parse(".5s"), Ok(500_000_000), "Go accepts a bare fraction");
        assert_eq!(parse("1h30m15s"), Ok(HOUR + 30 * MINUTE + 15 * SECOND));
        assert_eq!(parse("2h45m"), Ok(2 * HOUR + 45 * MINUTE));
        assert_eq!(parse("1h30m"), Ok(HOUR + 30 * MINUTE));
        assert_eq!(parse("300ms"), Ok(300_000_000));
        assert_eq!(parse("1ns"), Ok(1));
        assert_eq!(parse("1us"), Ok(1_000));
        assert_eq!(
            parse("1\u{00b5}s"),
            Ok(1_000),
            "the U+00B5 micro sign is a unit"
        );
        assert_eq!(parse("1\u{03bc}s"), Ok(1_000), "the Greek mu is a unit");
        assert_eq!(parse("0"), Ok(0));
        assert_eq!(parse("00s"), Ok(0));
        assert_eq!(parse("+2m"), Ok(2 * MINUTE), "Go accepts a leading plus");
    }

    #[test]
    fn parses_negative_durations() {
        assert_eq!(parse("-1s"), Ok(-SECOND));
        assert_eq!(parse("-1.5h"), Ok(-5_400_000_000_000));
        assert_eq!(parse("-.5s"), Ok(-500_000_000));
        assert_eq!(parse("-0"), Ok(0));
    }

    #[test]
    fn rejects_the_shapes_go_rejects() {
        assert_eq!(
            parse("1.5").err(),
            Some("time: missing unit in duration \"1.5\"".to_string())
        );
        assert_eq!(
            parse("1x").err(),
            Some("time: unknown unit \"x\" in duration \"1x\"".to_string())
        );
        assert_eq!(
            parse("").err(),
            Some("time: invalid duration \"\"".to_string())
        );
        assert_eq!(
            parse("5 minutes").err(),
            Some("time: unknown unit \" minutes\" in duration \"5 minutes\"".to_string()),
            "a space is not a unit"
        );
        assert_eq!(
            parse("s").err(),
            Some("time: invalid duration \"s\"".to_string())
        );
        assert_eq!(
            parse(".s").err(),
            Some("time: invalid duration \".s\"".to_string()),
            "no digits around the period"
        );
        assert_eq!(
            parse("1e3s").err(),
            Some("time: unknown unit \"e\" in duration \"1e3s\"".to_string())
        );
        assert_eq!(
            parse("1h x").err(),
            Some("time: unknown unit \"h x\" in duration \"1h x\"".to_string()),
            "Go scans every character up to the next digit as the unit"
        );
        assert_eq!(
            parse("100000000000h").err(),
            Some("time: invalid duration \"100000000000h\"".to_string()),
            "Go rejects durations above its int64 nanosecond limit"
        );
        assert_eq!(
            parse("2562047h47m16.854775808s").err(),
            Some("time: invalid duration \"2562047h47m16.854775808s\"".to_string()),
            "the overflow limit is the sum, not the parts"
        );
    }

    #[test]
    fn formats_zero() {
        assert_eq!(format_go_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn formats_signed_nanos() {
        assert_eq!(format_go_nanos_signed(0), "0s");
        assert_eq!(format_go_nanos_signed(-1_000_000), "-1ms");
        assert_eq!(format_go_nanos_signed(-(SECOND + SECOND / 2)), "-1.5s");
    }

    #[test]
    fn formats_nanoseconds_and_microseconds() {
        assert_eq!(format_go_nanos(1), "1ns");
        assert_eq!(format_go_nanos(500), "500ns");
        assert_eq!(format_go_nanos(1_000), "1µs");
        assert_eq!(format_go_nanos(1_500), "1.5µs");
        assert_eq!(format_go_nanos(125_000), "125µs");
    }

    #[test]
    fn formats_milliseconds() {
        assert_eq!(format_go_nanos(1_000_000), "1ms");
        assert_eq!(
            format_go_nanos(300_000_000 / 1_000_000 * 1_000_000),
            "300ms"
        );
        assert_eq!(format_go_nanos(1_500_000), "1.5ms");
        assert_eq!(format_go_nanos(1_234_000), "1.234ms");
    }

    #[test]
    fn formats_seconds_with_fraction() {
        assert_eq!(format_go_nanos(1_000_000_000), "1s");
        assert_eq!(format_go_nanos(10_000_000_000), "10s");
        assert_eq!(format_go_nanos(1_234_000_000), "1.234s");
        assert_eq!(format_go_nanos(1_001_000_000), "1.001s");
        assert_eq!(format_go_nanos(59_999_999_999), "59.999999999s");
    }

    #[test]
    fn formats_minutes_and_hours() {
        assert_eq!(format_go_nanos(90_000_000_000), "1m30s");
        assert_eq!(format_go_nanos(60_000_000_000), "1m0s");
        assert_eq!(format_go_nanos(61_000_000_000), "1m1s");
        assert_eq!(format_go_nanos(90_000_000_000 + 500_000_000), "1m30.5s");
        assert_eq!(format_go_nanos(5_400_000_000_000), "1h30m0s");
        assert_eq!(format_go_nanos(3_600_000_000_000), "1h0m0s");
        assert_eq!(format_go_nanos(86_400_000_000_000), "24h0m0s");
        assert_eq!(format_go_nanos(3_600_000_500_000_000), "1000h0m0.5s");
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(
            round_milliseconds(Duration::from_micros(1_500)),
            Duration::from_millis(2)
        );
        assert_eq!(
            round_milliseconds(Duration::from_nanos(1_499_999)),
            Duration::from_millis(1)
        );
        assert_eq!(round_milliseconds(Duration::ZERO), Duration::ZERO);
    }
}
