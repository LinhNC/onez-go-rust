//! `querybench` — list-query latency benchmark against the links API.
//!
//! Port of `cmd/querybench/main.go`. Flags, defaults, the report lines, the
//! stderr lines and the exit codes are reproduced from the Go program:
//!
//! ```text
//! -api-key      administrator API key     (no default: required)
//! -base-url     service base URL          (default http://127.0.0.1:8080)
//! -iterations   requests per scenario     (default 200)
//! ```
//!
//! Flags are parsed, and the usage is printed, exactly the way Go's `flag`
//! package did — see [`shorten_cli::cli`]. The missing-key / non-positive
//! iteration check is the program's own, so it exits 2 like Go's `os.Exit(2)`.
//!
//! Transport parity: Go fixed the client at 20 idle connections per host with a
//! 30 second idle timeout and a 2 second request timeout; the same settings are
//! applied to the client here. Go's `net/http` failure text (`Get "url": dial
//! tcp …: connect: connection refused`) cannot be reproduced from Rust's socket
//! layer, so the transport failure line carries the reqwest message after the
//! same `scenario request: ` prefix.

use std::process::exit;
use std::time::{Duration, Instant};

use shorten_cli::cli::{self, fatal, fatal_usage, Kind, Spec};
use shorten_cli::go_duration;

/// Go's client timeout for every request (`Timeout: 2 * time.Second`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Go's transport idle connection timeout (`IdleConnTimeout: 30 * time.Second`).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Go's transport pool size (`MaxIdleConns: 20, MaxIdleConnsPerHost: 20`).
const MAX_IDLE_CONNECTIONS: usize = 20;
/// The p95 budget every scenario must stay inside (`p95 > 50*time.Millisecond`).
const P95_BUDGET: Duration = Duration::from_millis(50);

/// The three flags `cmd/querybench` registered.
const FLAGS: [Spec; 3] = [
    Spec {
        name: "api-key",
        kind: Kind::String,
        default: "",
        usage: "administrator API key",
    },
    Spec {
        name: "base-url",
        kind: Kind::String,
        default: "http://127.0.0.1:8080",
        usage: "service base URL",
    },
    Spec {
        name: "iterations",
        kind: Kind::Int,
        default: "200",
        usage: "requests per query scenario",
    },
];

/// `scenario` in the Go program: a display name and the API path to drive.
struct Scenario {
    name: &'static str,
    path: &'static str,
}

/// The six scenarios Go drove, in the same order.
const SCENARIOS: [Scenario; 6] = [
    Scenario {
        name: "updated",
        path: "/api/links?limit=30&sort=updated&dir=desc",
    },
    Scenario {
        name: "active",
        path: "/api/links?limit=30&status=active&sort=updated&dir=desc",
    },
    Scenario {
        name: "expired",
        path: "/api/links?limit=30&status=expired&sort=updated&dir=desc",
    },
    Scenario {
        name: "visits",
        path: "/api/links?limit=30&sort=visits&dir=desc",
    },
    Scenario {
        name: "destination",
        path: "/api/links?limit=30&sort=destination&dir=asc",
    },
    Scenario {
        name: "fts",
        path: "/api/links?limit=30&q=Seed%20link%2000000000&sort=updated&dir=desc",
    },
];

/// Go's `int(float64(len(latencies)-1) * percent)` index.
#[must_use]
fn scaled_index(last: usize, percent: f64) -> usize {
    ((last as f64) * percent) as usize
}

/// The line Go printed for one completed scenario.
#[must_use]
fn scenario_line(name: &str, latencies: &[Duration]) -> String {
    let last = latencies.len() - 1;
    format!(
        "scenario={name} iterations={} p50={} p95={} p99={}",
        latencies.len(),
        go_duration::format_go_duration(latencies[latencies.len() / 2]),
        go_duration::format_go_duration(latencies[scaled_index(last, 0.95)]),
        go_duration::format_go_duration(latencies[scaled_index(last, 0.99)])
    )
}

#[tokio::main]
async fn main() {
    let values = cli::parse(&FLAGS, "querybench");
    let api_key = values.string("api-key");
    let iterations = values.int("iterations");
    if api_key.is_empty() || iterations < 1 {
        fatal_usage("api-key and positive iterations are required");
    }
    let base = values.string("base-url");
    let iterations = usize::try_from(iterations).unwrap_or(1);

    let client = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .pool_idle_timeout(IDLE_TIMEOUT)
        .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS)
        .build()
    {
        Ok(client) => client,
        Err(error) => fatal(error),
    };

    let mut failed = false;
    for scenario in &SCENARIOS {
        let url = format!("{base}{}", scenario.path);
        let mut latencies: Vec<Duration> = Vec::with_capacity(iterations);
        for _index in 0..iterations {
            let started = Instant::now();
            let result = client
                .get(&url)
                .header("X-API-Key", api_key.as_str())
                .send()
                .await;
            let elapsed = started.elapsed();
            match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let _result = response.bytes().await;
                    if status != 200 {
                        eprintln!("{} status={status}", scenario.name);
                        failed = true;
                        break;
                    }
                    latencies.push(elapsed);
                }
                Err(error) => {
                    eprintln!("{} request: {error}", scenario.name);
                    failed = true;
                    break;
                }
            }
        }
        if latencies.is_empty() {
            continue;
        }
        latencies.sort_unstable();
        let p95 = latencies[scaled_index(latencies.len() - 1, 0.95)];
        println!("{}", scenario_line(scenario.name, &latencies));
        if p95 > P95_BUDGET {
            failed = true;
        }
    }
    if failed {
        exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{scaled_index, scenario_line, Scenario, FLAGS, SCENARIOS};
    use std::time::Duration;

    #[test]
    fn percentiles_use_go_indexing() {
        assert_eq!(scaled_index(99, 0.95), 94);
        assert_eq!(scaled_index(99, 0.99), 98);
        assert_eq!(scaled_index(0, 0.95), 0);
        assert_eq!(scaled_index(3, 0.95), 2);
        assert_eq!(scaled_index(19, 0.99), 18);
    }

    #[test]
    fn scenario_line_keeps_go_shape() {
        let latencies: Vec<Duration> = (0..20)
            .map(|index| Duration::from_millis(index + 1))
            .collect();
        assert_eq!(
            scenario_line("updated", &latencies),
            "scenario=updated iterations=20 p50=11ms p95=19ms p99=19ms"
        );
        let single = [Duration::from_micros(1_500)];
        assert_eq!(
            scenario_line("fts", &single),
            "scenario=fts iterations=1 p50=1.5ms p95=1.5ms p99=1.5ms"
        );
    }

    #[test]
    fn scenarios_match_go_paths_and_order() {
        let names: Vec<&str> = SCENARIOS.iter().map(|s: &Scenario| s.name).collect();
        assert_eq!(
            names,
            [
                "updated",
                "active",
                "expired",
                "visits",
                "destination",
                "fts"
            ]
        );
        assert_eq!(
            SCENARIOS[0].path,
            "/api/links?limit=30&sort=updated&dir=desc"
        );
        assert_eq!(
            SCENARIOS[5].path,
            "/api/links?limit=30&q=Seed%20link%2000000000&sort=updated&dir=desc"
        );
        assert_eq!(
            SCENARIOS[4].path,
            "/api/links?limit=30&sort=destination&dir=asc"
        );
    }

    #[test]
    fn flag_table_keeps_go_defaults() {
        assert_eq!(
            FLAGS[0].default, "",
            "the key has no default, so it is required"
        );
        assert_eq!(FLAGS[1].default, "http://127.0.0.1:8080");
        assert_eq!(FLAGS[2].default, "200");
    }
}
