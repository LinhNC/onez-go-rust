//! Configuration loaded from the environment.
//!
//! Variable names, defaults and validation rules are inherited from the Go
//! service unchanged: the same `.env` file drives both implementations.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::thread;
use std::time::Duration;

/// Validated service configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub address: String,
    pub database_path: String,
    pub public_base_url: String,
    pub admin_api_key: String,
    pub session_secret: String,
    pub previous_secret: String,
    pub session_ttl: Duration,
    pub secure_cookies: bool,
    pub allow_null_origin: bool,
    pub read_connections: i32,
    pub cache_capacity: usize,
    pub positive_cache_ttl: Duration,
    pub negative_cache_ttl: Duration,
    pub analytics_interval: Duration,
    pub analytics_threshold: usize,
    pub analytics_max_keys: usize,
    pub shutdown_timeout: Duration,
}

/// All configuration problems found in one pass, mirroring the previous
/// implementation which reported every problem at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub Vec<String>);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.join("\n"))
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Load and validate configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns every validation problem found, joined together.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(|key| env::var(key).ok())
    }

    /// Load configuration from an arbitrary lookup function, which keeps the
    /// validation rules unit-testable without touching the process environment.
    ///
    /// # Errors
    ///
    /// Returns every validation problem found, joined together.
    pub fn load_from<'a>(
        lookup: impl Fn(&str) -> Option<String> + 'a,
    ) -> Result<Self, ConfigError> {
        let lookup = |key: &str| lookup(key).map(|value| value.trim().to_string());
        let read = |key: &str, fallback: &str| {
            let value = lookup(key).unwrap_or_default();
            if value.is_empty() {
                String::from(fallback)
            } else {
                value
            }
        };
        let integer = |key: &str, fallback: i64| match lookup(key) {
            Some(value) if !value.is_empty() => value.parse::<i64>().unwrap_or(-1),
            _ => fallback,
        };
        let boolean = |key: &str, fallback: bool| match lookup(key) {
            Some(value) if !value.is_empty() => match parse_bool(&value) {
                Some(value) => value,
                None => fallback,
            },
            _ => fallback,
        };
        // An unparsable duration becomes `Duration::ZERO`, which the
        // positivity check below rejects — the same failure Go reported for
        // its `-1` sentinel.
        let duration = |key: &str, fallback: Duration| match lookup(key) {
            Some(value) if !value.is_empty() => parse_go_duration(&value).unwrap_or(Duration::ZERO),
            _ => fallback,
        };

        let processors = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);
        let read_connections = integer(
            "SHORTEN_READ_CONNECTIONS",
            i64::try_from(processors.clamp(2, 16)).unwrap_or(16),
        );

        let config = Self {
            address: read("SHORTEN_ADDRESS", ":8080"),
            database_path: read("SHORTEN_DATABASE_PATH", ".data/shorten.db"),
            public_base_url: read("SHORTEN_PUBLIC_BASE_URL", "http://localhost:8080")
                .trim_end_matches('/')
                .to_string(),
            admin_api_key: lookup("SHORTEN_ADMIN_API_KEY").unwrap_or_default(),
            session_secret: lookup("SHORTEN_SESSION_SECRET").unwrap_or_default(),
            previous_secret: lookup("SHORTEN_PREVIOUS_SESSION_SECRET").unwrap_or_default(),
            session_ttl: duration("SHORTEN_SESSION_TTL", Duration::from_secs(12 * 60 * 60)),
            secure_cookies: !boolean("SHORTEN_INSECURE_COOKIES", false),
            allow_null_origin: boolean("SHORTEN_ALLOW_NULL_ORIGIN", false),
            read_connections: read_connections as i32,
            cache_capacity: usize::try_from(integer("SHORTEN_CACHE_CAPACITY", 10_000)).unwrap_or(0),
            positive_cache_ttl: duration("SHORTEN_CACHE_POSITIVE_TTL", Duration::from_secs(5 * 60)),
            negative_cache_ttl: duration("SHORTEN_CACHE_NEGATIVE_TTL", Duration::from_secs(10)),
            analytics_interval: duration("SHORTEN_ANALYTICS_INTERVAL", Duration::from_millis(250)),
            analytics_threshold: usize::try_from(integer("SHORTEN_ANALYTICS_THRESHOLD", 1_000))
                .unwrap_or(0),
            analytics_max_keys: usize::try_from(integer("SHORTEN_ANALYTICS_MAX_KEYS", 50_000))
                .unwrap_or(0),
            shutdown_timeout: duration("SHORTEN_SHUTDOWN_TIMEOUT", Duration::from_secs(15)),
        };

        let mut problems = Vec::new();
        if config.admin_api_key.chars().count() < 16 {
            problems.push("SHORTEN_ADMIN_API_KEY must contain at least 16 characters".to_string());
        }
        if config.session_secret.chars().count() < 32 {
            problems.push("SHORTEN_SESSION_SECRET must contain at least 32 characters".to_string());
        }
        if config.database_path.is_empty() {
            problems.push("SHORTEN_DATABASE_PATH is required".to_string());
        }
        if !is_absolute_http_url(&config.public_base_url) {
            problems.push("SHORTEN_PUBLIC_BASE_URL must be an absolute http(s) URL".to_string());
        }
        if config.read_connections < 1
            || config.cache_capacity < 1
            || config.analytics_threshold < 1
            || config.analytics_max_keys < config.analytics_threshold
        {
            problems.push(
                "connection, cache, and analytics limits must be positive and internally consistent"
                    .to_string(),
            );
        }
        if is_non_positive(config.session_ttl)
            || is_non_positive(config.positive_cache_ttl)
            || is_non_positive(config.negative_cache_ttl)
            || is_non_positive(config.analytics_interval)
            || is_non_positive(config.shutdown_timeout)
        {
            problems.push("configured durations must be positive".to_string());
        }

        if problems.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError(problems))
        }
    }

    /// Positive SQLite reader connections, clamped to a usable range.
    #[must_use]
    pub fn reader_connections(&self) -> usize {
        usize::try_from(self.read_connections).unwrap_or(1).max(1)
    }

    /// Configuration as a sorted map, used by the CLI `config` style checks
    /// and by the health endpoint metadata.
    #[must_use]
    pub fn redacted_map(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("address".to_string(), self.address.clone()),
            ("database_path".to_string(), self.database_path.clone()),
            ("public_base_url".to_string(), self.public_base_url.clone()),
            (
                "read_connections".to_string(),
                self.read_connections.to_string(),
            ),
            (
                "cache_capacity".to_string(),
                self.cache_capacity.to_string(),
            ),
            (
                "secure_cookies".to_string(),
                self.secure_cookies.to_string(),
            ),
            (
                "allow_null_origin".to_string(),
                self.allow_null_origin.to_string(),
            ),
        ])
    }
}

impl fmt::Display for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "address={} database={} base_url={} read_connections={} cache_capacity={}",
            self.address,
            self.database_path,
            self.public_base_url,
            self.read_connections,
            self.cache_capacity
        )
    }
}

fn is_non_positive(value: Duration) -> bool {
    value == Duration::ZERO
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// Parse Go's `time.ParseDuration` grammar exactly: `1.5h`, `250ms`, `5m`,
/// `-30s`, `1h30m10s`, `1µs`. Unrecognised input is rejected so the caller can
/// surface the same validation failure the Go service produced.
pub fn parse_go_duration(value: &str) -> Option<Duration> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    let (sign, body) = match text.as_bytes().first() {
        Some(b'-') => (-1_i64, &text[1..]),
        Some(b'+') => (1, &text[1..]),
        _ => (1, text),
    };
    if body.is_empty() {
        return None;
    }

    const NANOS: i64 = 1;
    const MICRO: i64 = 1_000;
    const MILLI: i64 = 1_000_000;
    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;

    let mut total: i64 = 0;
    let mut index = 0_usize;
    let bytes: Vec<char> = body.chars().collect();
    let mut saw_unit = false;

    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == '.') {
            index += 1;
        }
        if index == number_start {
            return None;
        }
        let digits: String = bytes[number_start..index].iter().collect();
        let leading: f64 = digits.parse().ok()?;
        // Reject shapes Go rejects, such as "1." at the end of a component.
        if digits.ends_with('.') {
            return None;
        }
        let unit_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_digit() && bytes[index] != '.' {
            index += 1;
        }
        if index == unit_start {
            return None;
        }
        let unit: String = bytes[unit_start..index].iter().collect();
        let scale = match unit.as_str() {
            "ns" => NANOS,
            "us" | "\u{00b5}s" | "\u{03bc}s" => MICRO,
            "ms" => MILLI,
            "s" => SECOND,
            "m" => MINUTE,
            "h" => HOUR,
            _ => return None,
        };
        #[allow(clippy::cast_precision_loss)]
        let scaled = leading * (scale as f64);
        // Go rejects overflow; keep the same guard with a float pre-check.
        #[allow(clippy::cast_precision_loss)]
        if scaled > i64::MAX as f64 {
            return None;
        }
        total = total.checked_add(scaled.round() as i64)?;
        saw_unit = true;
    }

    if !saw_unit {
        return None;
    }
    let total = if sign < 0 {
        total.checked_neg()?
    } else {
        total
    };
    // Go's `time.Duration` is signed and the service rejects non-positive
    // durations, so negative input and unparsable input share one outcome.
    u64::try_from(total).map(Duration::from_nanos).ok()
}

/// A URL accepted by `SHORTEN_PUBLIC_BASE_URL`: absolute http(s) with a host.
#[must_use]
pub fn is_absolute_http_url(value: &str) -> bool {
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https") && !parsed.host_str().unwrap_or_default().is_empty()
}

/// Convert the configured address into something a TCP listener can bind.
/// Go's `":8080"` binds every interface dual-stack; Rust needs an explicit host.
#[must_use]
pub fn bind_addresses(address: &str) -> Vec<String> {
    if let Some(port) = address.strip_prefix(':') {
        // `net.JoinHostPort` brackets the wildcard address and keeps the colon.
        return vec![format!("[::]:{port}")];
    }
    vec![address.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_env() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "SHORTEN_ADMIN_API_KEY".to_string(),
                "correct-admin-api-key".to_string(),
            ),
            (
                "SHORTEN_SESSION_SECRET".to_string(),
                "12345678901234567890123456789012".to_string(),
            ),
        ])
    }

    fn load_with(extra: BTreeMap<String, String>) -> Result<Config, ConfigError> {
        let merged = extra;
        Config::load_from(move |key| merged.get(key).cloned())
    }

    #[test]
    fn accepts_defaults() {
        let config = load_with(base_env()).expect("valid defaults");
        assert_eq!(config.address, ":8080");
        assert_eq!(config.database_path, ".data/shorten.db");
        assert_eq!(config.public_base_url, "http://localhost:8080");
        assert!(config.secure_cookies);
        assert_eq!(config.cache_capacity, 10_000);
        assert_eq!(config.analytics_interval, Duration::from_millis(250));
        assert_eq!(config.session_ttl, Duration::from_secs(12 * 60 * 60));
        assert!(config.read_connections >= 2);
    }

    #[test]
    fn reports_every_problem_at_once() {
        let error = load_with(BTreeMap::new()).expect_err("missing secrets");
        let rendered = error.to_string();
        assert!(rendered.contains("SHORTEN_ADMIN_API_KEY must contain at least 16 characters"));
        assert!(rendered.contains("SHORTEN_SESSION_SECRET must contain at least 32 characters"));
        assert_eq!(error.0.len(), 2);
    }

    #[test]
    fn rejects_non_absolute_base_url() {
        let mut env = base_env();
        env.insert(
            "SHORTEN_PUBLIC_BASE_URL".to_string(),
            "go.example.com".to_string(),
        );
        let error = load_with(env).expect_err("invalid base url");
        assert!(error
            .to_string()
            .contains("SHORTEN_PUBLIC_BASE_URL must be an absolute http(s) URL"));
    }

    #[test]
    fn strips_trailing_slash_from_base_url() {
        let mut env = base_env();
        env.insert(
            "SHORTEN_PUBLIC_BASE_URL".to_string(),
            "https://go.example.com/".to_string(),
        );
        let config = load_with(env).expect("valid");
        assert_eq!(config.public_base_url, "https://go.example.com");
    }

    #[test]
    fn insecure_cookies_disable_secure_flag() {
        let mut env = base_env();
        env.insert("SHORTEN_INSECURE_COOKIES".to_string(), "true".to_string());
        let config = load_with(env).expect("valid");
        assert!(!config.secure_cookies);
    }

    #[test]
    fn parses_go_duration_grammar() {
        assert_eq!(parse_go_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_go_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(
            parse_go_duration("1h30m10s"),
            Some(Duration::from_secs(5410))
        );
        assert_eq!(parse_go_duration("1µs"), Some(Duration::from_micros(1)));
        assert_eq!(parse_go_duration("1us"), Some(Duration::from_micros(1)));
        assert_eq!(parse_go_duration("1ns"), Some(Duration::from_nanos(1)));
        assert_eq!(parse_go_duration("0"), None);
        assert_eq!(parse_go_duration("5"), None);
        assert_eq!(parse_go_duration("5x"), None);
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("1h30"), None);
    }

    #[test]
    fn rejects_inconsistent_limits() {
        let mut env = base_env();
        env.insert("SHORTEN_ANALYTICS_MAX_KEYS".to_string(), "10".to_string());
        env.insert("SHORTEN_ANALYTICS_THRESHOLD".to_string(), "100".to_string());
        let error = load_with(env).expect_err("inconsistent limits");
        assert!(error.to_string().contains(
            "connection, cache, and analytics limits must be positive and internally consistent"
        ));
    }

    #[test]
    fn maps_wildcard_address_to_dual_stack_bind() {
        assert_eq!(bind_addresses(":8080"), vec!["[::]:8080".to_string()]);
        assert_eq!(
            bind_addresses("127.0.0.1:9000"),
            vec!["127.0.0.1:9000".to_string()]
        );
    }

    #[test]
    fn display_matches_previous_format() {
        let config = load_with(base_env()).expect("valid");
        assert!(config
            .to_string()
            .starts_with("address=:8080 database=.data/shorten.db"));
    }
}
