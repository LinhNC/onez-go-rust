//! Operational metrics in the Prometheus text exposition format.
//!
//! Route and mutation counters are kept apart from the redirect counters because
//! the redirect handler is the hottest route and would otherwise dominate the
//! request map.
//!
//! [`Metrics::render`] reproduces the previous Go handler byte for byte, so an
//! existing scrape configuration, dashboard and alert rule keep working.

use std::collections::BTreeMap;
use std::fmt::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Route whose observations belong to the redirect counters, not the map.
const REDIRECT_ROUTE: &str = "/{slug}";
/// Nanoseconds per second, matching `float64(time.Second)` of the Go handler.
const NANOS_PER_SECOND: f64 = 1.0e9;
/// Outcome labels in the order the Go handler emits them.
const OUTCOMES: [(RedirectOutcome, &str); 6] = [
    (RedirectOutcome::Success, "success"),
    (RedirectOutcome::Missing, "missing"),
    (RedirectOutcome::Disabled, "disabled"),
    (RedirectOutcome::Expired, "expired"),
    (RedirectOutcome::MethodNotAllowed, "method_not_allowed"),
    (RedirectOutcome::Error, "error"),
];

/// How a redirect request ended, mirroring the Go outcome strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectOutcome {
    /// The visitor was sent on to the destination.
    Success,
    /// No link owns that short code.
    Missing,
    /// The link exists and is switched off.
    Disabled,
    /// The link exists and has passed its expiry.
    Expired,
    /// The request used a verb the redirect route does not serve.
    MethodNotAllowed,
    /// Anything else, matching the Go `default` branch.
    Error,
}

impl RedirectOutcome {
    /// The `outcome` label value the metrics endpoint reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Missing => "missing",
            Self::Disabled => "disabled",
            Self::Expired => "expired",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::Error => "error",
        }
    }
}

/// Monotonic counters for the metrics endpoint.
///
/// Route and mutation counters live behind a mutex because their keys arrive
/// dynamically; the redirect counters are atomics, since they are what the hot
/// path touches.
#[derive(Debug, Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<String, u64>>,
    mutations: Mutex<BTreeMap<String, u64>>,
    redirect_requests: AtomicU64,
    redirect_nanos: AtomicU64,
    redirect_success: AtomicU64,
    redirect_missing: AtomicU64,
    redirect_disabled: AtomicU64,
    redirect_expired: AtomicU64,
    redirect_method: AtomicU64,
    redirect_errors: AtomicU64,
}

impl Metrics {
    /// Build a counter set with nothing observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one request observation.
    ///
    /// `route` is the matched route pattern, not the request path. The redirect
    /// route goes to the redirect counters and never enters the request map,
    /// exactly as in the previous implementation.
    pub fn observe_request(&self, route: &str, status: u16, duration: Duration) {
        if route == REDIRECT_ROUTE {
            self.redirect_requests.fetch_add(1, Ordering::Relaxed);
            let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
            self.redirect_nanos.fetch_add(nanos, Ordering::Relaxed);
            return;
        }
        let key = format!("{route}|{status}");
        *self
            .requests
            .lock()
            .expect("metrics request lock poisoned")
            .entry(key)
            .or_insert(0) += 1;
    }

    /// Count one redirect outcome.
    pub fn redirect(&self, outcome: RedirectOutcome) {
        counter(self, outcome).fetch_add(1, Ordering::Relaxed);
    }

    /// Count one link mutation, keyed by operation name.
    pub fn mutation(&self, operation: &str) {
        *self
            .mutations
            .lock()
            .expect("metrics mutation lock poisoned")
            .entry(operation.to_string())
            .or_insert(0) += 1;
    }

    /// Render the whole counter set as Prometheus text.
    ///
    /// Line order is fixed: request lines sorted by route and status, mutation
    /// lines sorted by operation, the redirect totals, the duration sum, the six
    /// outcome lines in the Go order, then the `extra` gauges in the map's own
    /// ordering. The caller sets the `text/plain; version=0.0.4; charset=utf-8`
    /// content type; the Go body carries no `# HELP` or `# TYPE` comments.
    ///
    /// Route patterns never contain quotes or backslashes, so the label quoting
    /// matches the Go handler's `%q` output.
    #[allow(
        clippy::cast_precision_loss,
        reason = "nanoseconds → seconds, as the Go handler does"
    )]
    #[must_use]
    pub fn render(&self, extra: &(dyn Fn() -> BTreeMap<String, f64> + Send + Sync)) -> String {
        let mut body = String::new();
        {
            let requests = self.requests.lock().expect("metrics request lock poisoned");
            for (key, count) in requests.iter() {
                let (route, status) = split_key(key);
                push_line(
                    &mut body,
                    format_args!(
                        "shorten_http_requests_total{{route=\"{route}\",status=\"{status}\"}} {count}\n"
                    ),
                );
            }
        }
        {
            let mutations = self
                .mutations
                .lock()
                .expect("metrics mutation lock poisoned");
            for (operation, count) in mutations.iter() {
                push_line(
                    &mut body,
                    format_args!(
                        "shorten_link_mutations_total{{operation=\"{operation}\"}} {count}\n"
                    ),
                );
            }
        }
        push_line(
            &mut body,
            format_args!(
                "shorten_redirect_requests_total {}\n",
                self.redirect_requests.load(Ordering::Relaxed)
            ),
        );
        // `f64` cannot hold every `u64` exactly; the Go handler rounds the same
        // way, so a plain cast reproduces its output.
        let nanos = self.redirect_nanos.load(Ordering::Relaxed) as f64;
        push_line(
            &mut body,
            format_args!(
                "shorten_redirect_duration_seconds_sum {:.9}\n",
                nanos / NANOS_PER_SECOND
            ),
        );
        for (outcome, label) in OUTCOMES {
            push_line(
                &mut body,
                format_args!(
                    "shorten_redirects_total{{outcome=\"{label}\"}} {}\n",
                    counter(self, outcome).load(Ordering::Relaxed)
                ),
            );
        }
        for (key, value) in extra() {
            push_line(&mut body, format_args!("{key} {}\n", format_g(&value)));
        }
        body
    }
}

/// Append one formatted metrics line.
///
/// Writing into a `String` cannot fail, so the result is only asserted on.
fn push_line(body: &mut String, line: fmt::Arguments<'_>) {
    debug_assert!(
        body.write_fmt(line).is_ok(),
        "appending to a String is infallible"
    );
}

/// Outcome counter, in the same order as the rendered outcome lines.
fn counter(metrics: &Metrics, outcome: RedirectOutcome) -> &AtomicU64 {
    match outcome {
        RedirectOutcome::Success => &metrics.redirect_success,
        RedirectOutcome::Missing => &metrics.redirect_missing,
        RedirectOutcome::Disabled => &metrics.redirect_disabled,
        RedirectOutcome::Expired => &metrics.redirect_expired,
        RedirectOutcome::MethodNotAllowed => &metrics.redirect_method,
        RedirectOutcome::Error => &metrics.redirect_errors,
    }
}

/// Split the internal `route|status` key back into its label halves.
fn split_key(key: &str) -> (&str, &str) {
    match key.rsplit_once('|') {
        Some((route, status)) => (route, status),
        None => (key, ""),
    }
}

/// Format a float exactly as Go's `%g` verb does.
///
/// Go prints the shortest decimal form that round-trips, uses fixed notation when
/// the exponent lies in `[-4, 6)` and scientific notation otherwise, and writes
/// the exponent with a sign and at least two digits.
fn format_g(value: &f64) -> String {
    let value = *value;
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_string();
    }

    // Rust's LowerExp yields the same shortest digit string Go's strconv uses.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa
        .chars()
        .filter(char::is_ascii_digit)
        .skip_while(|c| *c == '0')
        .collect();
    let digits = digits.trim_end_matches('0');
    if digits.is_empty() {
        return "0".to_string();
    }
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let sign = if negative { "-" } else { "" };

    if (-4..6).contains(&exponent) {
        return format!("{sign}{}", fixed_notation(digits, exponent));
    }
    let tail = if digits.len() > 1 {
        format!(".{}", &digits[1..])
    } else {
        String::new()
    };
    let exponent_sign = if exponent < 0 { "-" } else { "+" };
    let magnitude = u64::try_from(exponent.abs()).unwrap_or(0);
    format!("{sign}{}{tail}e{exponent_sign}{magnitude:02}", &digits[..1])
}

/// Fixed notation for the shortest digits of `0.<digits> * 10^(exponent + 1)`.
fn fixed_notation(digits: &str, exponent: i64) -> String {
    let width = usize::try_from(exponent + 1).unwrap_or(digits.len());
    if exponent >= 0 {
        // The digits run out before the decimal point does, so the integer part
        // is zero-padded, exactly as Go pads it.
        if width >= digits.len() {
            return format!("{}{}", digits, "0".repeat(width - digits.len()));
        }
        let (integer, fraction) = digits.split_at(width);
        return if fraction.is_empty() {
            integer.to_string()
        } else {
            format!("{integer}.{fraction}")
        };
    }
    let zeros = usize::try_from(-(exponent + 1)).unwrap_or(0);
    format!("0.{}{digits}", "0".repeat(zeros))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extras() -> BTreeMap<String, f64> {
        let mut values = BTreeMap::new();
        values.insert("shorten_analytics_dropped_total".to_string(), 1.5);
        values.insert("shorten_analytics_pending_links".to_string(), 7.0);
        values.insert("shorten_redirect_cache_hits_total".to_string(), 1.0e6);
        values.insert("shorten_redirect_cache_size".to_string(), 3.0);
        values
    }

    #[test]
    fn redirect_route_stays_out_of_the_request_map() {
        let metrics = Metrics::new();
        metrics.observe_request("/healthz", 200, Duration::from_millis(5));
        metrics.observe_request("/healthz", 200, Duration::from_millis(5));
        metrics.observe_request("/healthz", 500, Duration::from_millis(7));
        metrics.observe_request("/{slug}", 302, Duration::from_millis(10));
        metrics.observe_request("/{slug}", 404, Duration::from_millis(20));

        let body = metrics.render(&extras);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some("shorten_http_requests_total{route=\"/healthz\",status=\"200\"} 2")
        );
        assert_eq!(
            lines.get(1).copied(),
            Some("shorten_http_requests_total{route=\"/healthz\",status=\"500\"} 1")
        );
        assert_eq!(
            lines.get(2).copied(),
            Some("shorten_redirect_requests_total 2"),
            "totals not third: {body}"
        );
        assert!(
            body.contains("shorten_redirect_duration_seconds_sum 0.030000000\n"),
            "duration sum mismatch"
        );
        assert!(
            !body.contains("\"/{slug}\""),
            "redirect route entered the map"
        );
        assert_eq!(lines.len(), 2 + 8 + 4);
        assert_eq!(lines.last().copied(), Some("shorten_redirect_cache_size 3"));
    }

    #[test]
    fn counters_and_line_order_match_the_go_handler() {
        let metrics = Metrics::new();
        metrics.observe_request("/api/links", 200, Duration::from_micros(1_500));
        metrics.observe_request("/manage/links/{slug}", 302, Duration::from_secs(1));
        metrics.mutation("create");
        metrics.mutation("create");
        metrics.mutation("delete");
        let labels: Vec<&str> = OUTCOMES.iter().map(|(_, label)| *label).collect();
        assert_eq!(
            labels,
            [
                "success",
                "missing",
                "disabled",
                "expired",
                "method_not_allowed",
                "error"
            ]
        );
        for (outcome, label) in OUTCOMES {
            assert_eq!(outcome.as_str(), label, "outcome label changed");
            metrics.redirect(outcome);
        }

        let body = metrics.render(&extras);
        let order = [
            "shorten_http_requests_total{route=\"/api/links\",status=\"200\"} 1",
            "shorten_link_mutations_total{operation=\"create\"} 2",
            "shorten_redirect_requests_total 0",
            "shorten_redirect_duration_seconds_sum 0.000000000",
            "shorten_redirects_total{outcome=\"success\"} 1",
            "shorten_redirects_total{outcome=\"method_not_allowed\"} 1",
            "shorten_redirects_total{outcome=\"error\"} 1",
            "shorten_analytics_dropped_total 1.5",
            "shorten_redirect_cache_size 3",
        ]
        .map(|line| body.find(line).expect("line present"));
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "line order changed:\n{body}"
        );
        assert!(body.contains("shorten_redirects_total{outcome=\"missing\"} 1\n"));
        assert!(body.contains("shorten_redirects_total{outcome=\"disabled\"} 1\n"));
        assert!(body.contains("shorten_redirects_total{outcome=\"expired\"} 1\n"));
        assert!(body.contains("shorten_link_mutations_total{operation=\"delete\"} 1\n"));
        assert!(body.contains("shorten_analytics_pending_links 7\n"));
        assert!(body.contains("shorten_redirect_cache_hits_total 1e+06\n"));
        assert!(body.ends_with('\n'));
        assert_eq!(body.lines().count(), 2 + 2 + 1 + 1 + 6 + 4);
    }

    #[test]
    fn fresh_counters_render_only_the_fixed_lines() {
        let body = Metrics::new().render(&BTreeMap::new);
        let expected = concat!(
            "shorten_redirect_requests_total 0\n",
            "shorten_redirect_duration_seconds_sum 0.000000000\n",
            "shorten_redirects_total{outcome=\"success\"} 0\n",
            "shorten_redirects_total{outcome=\"missing\"} 0\n",
            "shorten_redirects_total{outcome=\"disabled\"} 0\n",
            "shorten_redirects_total{outcome=\"expired\"} 0\n",
            "shorten_redirects_total{outcome=\"method_not_allowed\"} 0\n",
            "shorten_redirects_total{outcome=\"error\"} 0\n",
        );
        assert_eq!(body, expected);
    }

    #[test]
    fn format_g_matches_go_g_formatting() {
        let cases = [
            (0.0_f64, "0"),
            (3.0, "3"),
            (2.5, "2.5"),
            (100.0, "100"),
            (100_000.0, "100000"),
            (1.0e6, "1e+06"),
            (1.0e-4, "0.0001"),
            (1.0e-5, "1e-05"),
            (123_456_789.0, "1.23456789e+08"),
            (-2.5, "-2.5"),
            (-1.0e-5, "-1e-05"),
            (1.0 / 3.0, "0.3333333333333333"),
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_g(&value), expected, "format_g of {value}");
        }
    }

    #[test]
    fn metrics_are_send_and_sync_and_counters_accumulate_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Metrics>();
        assert_send_sync::<RedirectOutcome>();

        let metrics = std::sync::Arc::new(Metrics::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let metrics = std::sync::Arc::clone(&metrics);
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    metrics.observe_request("/{slug}", 302, Duration::from_micros(1));
                    metrics.redirect(RedirectOutcome::Success);
                }
                metrics.observe_request("/healthz", 200, Duration::from_micros(1));
            }));
        }
        for handle in handles {
            handle.join().expect("thread closed cleanly");
        }
        let body = metrics.render(&BTreeMap::new);
        assert!(body.contains("shorten_redirect_requests_total 100\n"));
        assert!(body.contains("shorten_redirects_total{outcome=\"success\"} 100\n"));
        assert!(body.contains("shorten_redirect_duration_seconds_sum 0.000100000\n"));
        assert!(body.contains("shorten_http_requests_total{route=\"/healthz\",status=\"200\"} 4\n"));
    }
}
