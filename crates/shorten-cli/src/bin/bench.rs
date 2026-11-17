//! `bench` — redirect throughput benchmark against a running service.
//!
//! Port of `cmd/bench/main.go`. Flags, defaults, the single report line and the
//! exit codes are reproduced from the Go program:
//!
//! ```text
//! -concurrency   concurrent keep-alive clients  (default 200)
//! -duration      test duration                  (default 1m0s)
//! -target        redirect URL                   (default http://127.0.0.1:8080/00000000)
//! ```
//!
//! Flags are parsed, and the usage is printed, exactly the way Go's `flag`
//! package did — see [`shorten_cli::cli`]. `-duration` accepts Go's
//! `time.ParseDuration` grammar, including a negative value, which Go's own
//! positivity check then rejected with exit 2.
//!
//! Parity notes:
//!
//! * Go built its transport with `DisableCompression: true`, so no
//!   `Accept-Encoding` is sent here either (the reqwest features in this
//!   workspace do not enable response decompression).
//! * Go fixed `MaxIdleConns` to `concurrency * 2` as well as
//!   `MaxIdleConnsPerHost` to `concurrency`; reqwest exposes only the per-host
//!   pool, which is the one that matters for a single target.
//! * Go's `CheckRedirect: http.ErrUseLastResponse` returns the first 3xx
//!   un-followed, so the client is built with `reqwest::redirect::Policy::none()`:
//!   a redirect counts as a success, the destination is never fetched, and the
//!   measured latency covers the redirect response only. Go records the latency
//!   when the response headers arrive and drains the body afterwards, which
//!   `send()` then `bytes()` reproduces.
//! * Two labels in the report line are host facts, not behaviour: `go=` names
//!   the Rust toolchain (Go printed `runtime.Version()`), and `os=` / `arch=`
//!   are mapped onto Go's spellings (`darwin`, `arm64`, `amd64`).

use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use shorten_cli::cli::{self, fatal, fatal_usage, Kind, Spec};
use shorten_cli::go_duration;
use tokio::task::JoinSet;

/// Go's client timeout for every request (`Timeout: 5 * time.Second`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Go's transport idle connection timeout (`IdleConnTimeout: 90 * time.Second`).
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Go's `p(.95) > 10*time.Millisecond` budget.
const P95_BUDGET: Duration = Duration::from_millis(10);
/// Go's `p(.99) > 25*time.Millisecond` budget.
const P99_BUDGET: Duration = Duration::from_millis(25);
/// Go's `rate < 5000` floor.
const MINIMUM_RATE: f64 = 5_000.0;
/// Go's `errorRate >= .1` ceiling, in percent.
const MAXIMUM_ERROR_RATE: f64 = 0.1;

/// The three flags `cmd/bench` registered.
const FLAGS: [Spec; 3] = [
    Spec {
        name: "concurrency",
        kind: Kind::Int,
        default: "200",
        usage: "concurrent keep-alive clients",
    },
    Spec {
        name: "duration",
        kind: Kind::Duration,
        default: "1m0s",
        usage: "test duration",
    },
    Spec {
        name: "target",
        kind: Kind::String,
        default: "http://127.0.0.1:8080/00000000",
        usage: "redirect URL",
    },
];

#[tokio::main]
async fn main() {
    let values = cli::parse(&FLAGS, "bench");
    let concurrency = values.int("concurrency");
    let duration = values.duration_nanos("duration");
    if concurrency < 1 || duration <= 0 {
        fatal_usage("duration and concurrency must be positive");
    }
    let target = values.string("target");
    let clients = usize::try_from(concurrency).unwrap_or(1);
    // The positivity check above already rejected the negative spellings Go
    // accepted, so the nanosecond count is a valid `Duration` here.
    let window = Duration::from_nanos(
        u64::try_from(u128::try_from(duration).unwrap_or(u128::from(u64::MAX))).unwrap_or(u64::MAX),
    );

    let client = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .pool_idle_timeout(IDLE_TIMEOUT)
        .pool_max_idle_per_host(clients)
        // Go set `CheckRedirect` to `http.ErrUseLastResponse`, which stops the
        // client following the first redirect and hands back the 3xx itself.
        // reqwest's equivalent is a policy that follows no redirects.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => fatal(error),
    };

    let deadline = Instant::now() + window;
    let total = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let target = Arc::new(target);
    let started = Instant::now();

    let mut workers = JoinSet::new();
    for _worker in 0..clients {
        let client = client.clone();
        let target = Arc::clone(&target);
        let total = Arc::clone(&total);
        let failures = Arc::clone(&failures);
        workers.spawn(async move {
            let mut latencies: Vec<Duration> = Vec::new();
            while Instant::now() < deadline {
                let start = Instant::now();
                // Go recorded the latency when `Get` returned (the headers had
                // arrived) and drained the body afterwards.
                if let Ok(response) = client.get(target.as_str()).send().await {
                    latencies.push(start.elapsed());
                    total.fetch_add(1, Ordering::Relaxed);
                    let status = response.status().as_u16();
                    if !(300..400).contains(&status) {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    let _result = response.bytes().await;
                } else {
                    latencies.push(start.elapsed());
                    total.fetch_add(1, Ordering::Relaxed);
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            latencies
        });
    }

    let mut latencies: Vec<Duration> = Vec::new();
    while let Some(result) = workers.join_next().await {
        if let Ok(values) = result {
            latencies.extend(values);
        }
    }
    latencies.sort_unstable();
    let elapsed = started.elapsed();

    let requests = total.load(Ordering::Relaxed);
    let errors = failures.load(Ordering::Relaxed);
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let p99 = percentile(&latencies, 0.99);
    let report = Report {
        target: Arc::try_unwrap(target).unwrap_or_else(|arc| (*arc).clone()),
        elapsed,
        concurrency,
        requests,
        errors,
        p50,
        p95,
        p99,
    };
    println!("{}", report.render());

    if report.rate() < MINIMUM_RATE
        || report.p95 > P95_BUDGET
        || report.p99 > P99_BUDGET
        || report.error_rate() >= MAXIMUM_ERROR_RATE
    {
        exit(1);
    }
}

/// `p(percent)` in the Go program: `int(float64(len(all)-1) * percent)`.
#[must_use]
fn percentile(latencies: &[Duration], percent: f64) -> Duration {
    if latencies.is_empty() {
        return Duration::ZERO;
    }
    let last = (latencies.len() - 1) as f64;
    latencies[(last * percent) as usize]
}

/// The measured run, so the report line and the pass/fail budgets are computed
/// from one source and can be tested without a server.
struct Report {
    target: String,
    elapsed: Duration,
    concurrency: i64,
    requests: u64,
    errors: u64,
    p50: Duration,
    p95: Duration,
    p99: Duration,
}

impl Report {
    /// `rate := float64(total) / elapsed.Seconds()`
    #[allow(
        clippy::cast_precision_loss,
        reason = "Go converted the same counters to float64"
    )]
    #[must_use]
    fn rate(&self) -> f64 {
        self.requests as f64 / self.elapsed.as_secs_f64()
    }

    /// `errorRate := float64(failures) / float64(max(total, 1)) * 100`
    #[allow(
        clippy::cast_precision_loss,
        reason = "Go converted the same counters to float64"
    )]
    #[must_use]
    fn error_rate(&self) -> f64 {
        self.errors as f64 / self.requests.max(1) as f64 * 100.0
    }

    /// The single `fmt.Printf` line `cmd/bench` printed.
    #[must_use]
    fn render(&self) -> String {
        let elapsed =
            go_duration::format_go_nanos(go_duration::round_milliseconds(self.elapsed).as_nanos());
        format!(
            "target={} duration={} concurrency={} requests={} rps={:.0} p50={} p95={} p99={} \
             errors={} error_rate={:.3}% go={} os={} arch={}",
            self.target,
            elapsed,
            self.concurrency,
            self.requests,
            self.rate(),
            go_duration::format_go_duration(self.p50),
            go_duration::format_go_duration(self.p95),
            go_duration::format_go_duration(self.p99),
            self.errors,
            self.error_rate(),
            toolchain(),
            go_os(),
            go_arch()
        )
    }
}

/// `runtime.Version()` has no Rust equivalent, so the report names the toolchain
/// that produced the numbers.
fn toolchain() -> String {
    format!("rustc{}", env!("CARGO_PKG_RUST_VERSION"))
}

/// `runtime.GOOS` with Go's spelling for macOS.
fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// `runtime.GOARCH` with Go's spelling for the common architectures.
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{percentile, Report, FLAGS};
    use std::time::Duration;

    fn report(requests: u64, errors: u64, elapsed: Duration) -> Report {
        Report {
            target: "http://127.0.0.1:8080/00000000".to_string(),
            elapsed,
            concurrency: 2,
            requests,
            errors,
            p50: Duration::from_millis(4),
            p95: Duration::from_millis(9),
            p99: Duration::from_millis(20),
        }
    }

    #[test]
    fn percentiles_use_go_indexing() {
        let latencies: Vec<Duration> = (0..100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&latencies, 0.50), Duration::from_millis(49));
        assert_eq!(percentile(&latencies, 0.95), Duration::from_millis(94));
        assert_eq!(percentile(&latencies, 0.99), Duration::from_millis(98));
        assert_eq!(percentile(&[], 0.95), Duration::ZERO);
        assert_eq!(
            percentile(&[Duration::from_millis(7)], 0.95),
            Duration::from_millis(7)
        );
    }

    #[test]
    fn rates_match_go_arithmetic() {
        let measured = report(1_000, 1, Duration::from_secs(2));
        assert_eq!(measured.rate(), 500.0);
        assert_eq!(measured.error_rate(), 0.1);
        // `max(total, 1)` keeps a zero-request run defined.
        let empty = report(0, 0, Duration::from_secs(1));
        assert_eq!(empty.rate(), 0.0);
        assert_eq!(empty.error_rate(), 0.0);
    }

    #[test]
    fn report_line_keeps_go_field_order_and_shapes() {
        let line = report(3, 3, Duration::from_millis(2_500)).render();
        let (fields, tail) = line.split_once(" concurrency=").expect("fields");
        assert!(
            fields.starts_with("target=http://127.0.0.1:8080/00000000 duration=2.5s"),
            "{line}"
        );
        let rest = tail.split(' ').skip(1).collect::<Vec<_>>();
        assert_eq!(rest[0], "requests=3", "{line}");
        assert_eq!(rest[2], "p50=4ms", "{line}");
        assert_eq!(rest[3], "p95=9ms", "{line}");
        assert_eq!(rest[4], "p99=20ms", "{line}");
        assert_eq!(rest[5], "errors=3", "{line}");
        assert_eq!(rest[6], "error_rate=100.000%", "{line}");
        assert_eq!(rest[7].split('=').next(), Some("go"), "{line}");
        assert!(
            line.ends_with(&format!(" os={} arch={}", super::go_os(), super::go_arch())),
            "{line}"
        );
        assert!(line.contains(" rps=1 p50="), "{line}");
    }

    #[test]
    fn flag_table_keeps_go_defaults() {
        assert_eq!(FLAGS[0].default, "200");
        assert_eq!(
            FLAGS[1].default, "1m0s",
            "Go printed the duration default as 1m0s"
        );
        assert_eq!(FLAGS[2].default, "http://127.0.0.1:8080/00000000");
    }
}
