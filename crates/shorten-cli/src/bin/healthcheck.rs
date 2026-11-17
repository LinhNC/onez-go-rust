//! `healthcheck` — container readiness probe.
//!
//! Port of `cmd/healthcheck/main.go`. The Go program registered no flags and
//! never called `flag.Parse`, so command-line arguments are ignored here too:
//! `healthcheck --help` probes the service, it does not print usage. The target
//! comes from `SHORTEN_HEALTHCHECK_URL` — an empty value falls back to the
//! default, as Go's `if value := os.Getenv(...); value != ""` did — defaulting to
//! `http://127.0.0.1:8080/readyz`, and the probe uses a 2 second client timeout.
//! Success prints nothing on either stream and exits 0; a transport failure, an
//! unparsable URL, a redirect chain that ends in a non-200 response, or any
//! status other than 200 prints `not ready` on standard error and exits 1.

use std::env;
use std::process::exit;
use std::time::Duration;

/// Go's `http.Client{Timeout: 2 * time.Second}`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Go's default probe target.
const DEFAULT_TARGET: &str = "http://127.0.0.1:8080/readyz";
/// The environment variable the Go program read, and the value it accepted.
const TARGET_VARIABLE: &str = "SHORTEN_HEALTHCHECK_URL";

#[tokio::main]
async fn main() {
    let target = target_from_environment();

    // A client that cannot be built is a probe failure, exactly as a failed
    // request was: Go printed `not ready` and exited 1.
    let Ok(client) = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() else {
        eprintln!("not ready");
        exit(1);
    };

    if let Ok(response) = client.get(&target).send().await {
        let ready = response.status().as_u16() == 200;
        let _result = response.bytes().await;
        if ready {
            exit(0);
        }
    }
    eprintln!("not ready");
    exit(1);
}

/// `SHORTEN_HEALTHCHECK_URL`, taken verbatim, with Go's fallback for an unset or
/// empty value.
fn target_from_environment() -> String {
    let value = env::var(TARGET_VARIABLE).unwrap_or_default();
    if value.is_empty() {
        DEFAULT_TARGET.to_string()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_TARGET;

    #[test]
    fn default_target_matches_go() {
        assert_eq!(DEFAULT_TARGET, "http://127.0.0.1:8080/readyz");
    }
}
