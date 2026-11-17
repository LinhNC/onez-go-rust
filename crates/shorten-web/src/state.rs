//! Shared application state, response helpers and health/metrics plumbing.

#![allow(clippy::needless_pass_by_value)] // the counter is consumed by the formatter.
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::http::HeaderValue;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Serialize;
use shorten::analytics::Collector;
use shorten::auth::{ApiKey, Limiter, Sessions};
use shorten::config::Config;
use shorten::link::DomainError;
use shorten::observability::Metrics;
use shorten::redirectcache::{Cache, Stats as CacheStats};
use shorten::store::Store;
use tokio::task::JoinError;

/// Login attempts allowed per client per minute.
const LOGIN_LIMIT: u32 = 8;
/// Management mutations allowed per client per minute.
const MUTATION_LIMIT: u32 = 120;
/// Number of rate-limit buckets kept per limiter.
const LIMITER_CAPACITY: usize = 4_096;
/// Rate-limit window.
const LIMITER_WINDOW: StdDuration = StdDuration::from_secs(60);
/// `/readyz` gives the database this much time to answer a ping.
const PING_TIMEOUT: StdDuration = StdDuration::from_millis(500);

/// Everything a request handler needs, cheap to clone and shared with Axum.
#[derive(Clone)]
pub struct App(Arc<Inner>);

struct Inner {
    config: Config,
    store: Arc<Store>,
    cache: Cache,
    analytics: Collector,
    api_key: ApiKey,
    sessions: Sessions,
    login_limiter: Limiter,
    mutation_limiter: Limiter,
    metrics: Metrics,
    ready: AtomicBool,
    sequence: AtomicU64,
}

impl App {
    /// Build the application state, mirroring the previous constructor:
    /// one API key digest, one session box, two fixed-window limiters,
    /// a redirect cache, an analytics collector and a metrics registry.
    #[must_use]
    pub fn new(config: Config, store: Arc<Store>, cache: Cache, analytics: Collector) -> Self {
        Self(Arc::new(Inner {
            login_limiter: Limiter::new(LOGIN_LIMIT, LIMITER_WINDOW, LIMITER_CAPACITY),
            mutation_limiter: Limiter::new(MUTATION_LIMIT, LIMITER_WINDOW, LIMITER_CAPACITY),
            api_key: ApiKey::new(&config.admin_api_key),
            sessions: Sessions::new(
                &config.session_secret,
                &config.previous_secret,
                config.session_ttl,
                config.secure_cookies,
            ),
            metrics: Metrics::new(),
            ready: AtomicBool::new(true),
            sequence: AtomicU64::new(0),
            config,
            store,
            cache,
            analytics,
        }))
    }

    /// Flip readiness; the shutdown sequence clears it before draining.
    pub fn set_ready(&self, value: bool) {
        self.0.ready.store(value, Ordering::Relaxed);
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.0.config
    }

    #[must_use]
    pub fn cache(&self) -> &Cache {
        &self.0.cache
    }

    #[must_use]
    pub fn analytics(&self) -> &Collector {
        &self.0.analytics
    }

    #[must_use]
    pub fn metrics(&self) -> &Metrics {
        &self.0.metrics
    }

    #[must_use]
    pub fn sessions(&self) -> &Sessions {
        &self.0.sessions
    }

    #[must_use]
    pub fn api_key(&self) -> &ApiKey {
        &self.0.api_key
    }

    #[must_use]
    pub fn login_limiter(&self) -> &Limiter {
        &self.0.login_limiter
    }

    #[must_use]
    pub fn mutation_limiter(&self) -> &Limiter {
        &self.0.mutation_limiter
    }

    /// The next request id: a base-36 counter, like the previous
    /// `strconv.FormatUint(seq, 36)` identifier.
    #[must_use]
    pub fn next_request_id(&self) -> String {
        let value = self.0.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        to_base36(value)
    }

    /// Run a blocking store call on the blocking pool.
    ///
    /// SQLite work never runs on the async runtime; a cancelled task is
    /// reported as a store failure so handlers answer 500.
    ///
    /// # Errors
    ///
    /// Propagates the domain error raised by the store call, or a wrapped
    /// failure when the blocking task itself failed.
    pub async fn db<T, F>(&self, context: &'static str, work: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Store>) -> Result<T, DomainError> + Send + 'static,
    {
        let store = self.0.store.clone();
        match tokio::task::spawn_blocking(move || work(store)).await {
            Ok(result) => result,
            Err(error) => Err(unwrap_join_error(context, error)),
        }
    }

    /// Extra gauge values rendered with the metrics registry.
    #[must_use]
    pub fn metric_values(&self) -> std::collections::BTreeMap<String, f64> {
        let cache: CacheStats = self.0.cache.stats();
        std::collections::BTreeMap::from([
            (
                "shorten_redirect_cache_hits_total".to_string(),
                f64::from(u32::try_from(cache.hits).unwrap_or(u32::MAX)),
            ),
            (
                "shorten_redirect_cache_misses_total".to_string(),
                f64::from(u32::try_from(cache.misses).unwrap_or(u32::MAX)),
            ),
            (
                "shorten_redirect_cache_evictions_total".to_string(),
                f64::from(u32::try_from(cache.evictions).unwrap_or(u32::MAX)),
            ),
            (
                "shorten_redirect_cache_size".to_string(),
                f64::from(u32::try_from(cache.size).unwrap_or(u32::MAX)),
            ),
            (
                "shorten_analytics_pending_links".to_string(),
                f64::from(u32::try_from(self.0.analytics.pending().max(0)).unwrap_or(u32::MAX)),
            ),
            (
                "shorten_analytics_dropped_total".to_string(),
                f64::from(u32::try_from(self.0.analytics.dropped().max(0)).unwrap_or(u32::MAX)),
            ),
        ])
    }

    /// Readiness is the combination of the ready flag, a live database ping
    /// within 500 ms, and a healthy analytics collector.
    ///
    /// # Errors
    ///
    /// Propagates a store failure, which is reported as not ready.
    pub async fn ready(&self) -> Result<bool, DomainError> {
        if !self.0.ready.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let pinged = tokio::time::timeout(PING_TIMEOUT, {
            let this = self.clone();
            async move {
                this.db("ping store", |store| {
                    store.ping().map_err(into_domain_error)
                })
                .await
            }
        })
        .await;
        let database_ok = matches!(pinged, Ok(Ok(())));
        let analytics_ok = self.0.analytics.healthy(Utc::now());
        Ok(database_ok && analytics_ok)
    }
}

/// Serialize a body exactly like the previous JSON helper: compact JSON with a
/// trailing newline and the `application/json; charset=utf-8` media type.
#[must_use]
pub fn write_json<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let mut body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    body.push(b'\n');
    (
        status,
        [(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// Plain-text error body, matching the previous `http.Error` shape:
/// `text/plain; charset=utf-8` and a trailing newline.
#[must_use]
pub fn text_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        format!("{message}\n"),
    )
        .into_response()
}

/// The previous `http.NotFound` body.
#[must_use]
pub fn not_found() -> Response {
    text_error(StatusCode::NOT_FOUND, "404 page not found")
}

/// Go's `ServeMux` answered a path whose registered patterns did not include
/// the request method with an `Allow` header listing the registered methods in
/// sorted order, and the plain-text body `http.Error` wrote.
#[must_use]
pub fn method_not_allowed(allow: &str) -> Response {
    let mut response = text_error(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    if let Ok(value) = HeaderValue::from_str(allow) {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
}

/// HTML body helper used by every management page.
#[must_use]
pub fn html(status: StatusCode, body: String) -> Response {
    (status, Html(body)).into_response()
}

fn to_base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while value > 0 {
        let index = usize::try_from(value % 36).unwrap_or(0);
        digits.push(DIGITS[index]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base 36 digits are ASCII")
}

fn unwrap_join_error(context: &str, error: JoinError) -> DomainError {
    tracing::error!(target: "shorten::store", context, error = %error, "blocking store task failed");
    DomainError::wrapped(context, "the operation could not be completed")
}

/// Store failures are surfaced through the same status mapping as domain
/// failures, so wrap them as a wrapped domain error.
fn into_domain_error(error: shorten::store::StoreError) -> DomainError {
    DomainError::wrapped("store", error)
}

/// A nonzero request identifier, exposed for tests and logging.
#[must_use]
pub fn request_id_of(value: NonZeroU64) -> String {
    to_base36(value.get())
}

/// Timestamp helper shared by handlers: whole seconds from an expiry claim.
#[must_use]
pub fn claims_time(seconds: i64) -> Option<DateTime<Utc>> {
    chrono::DateTime::from_timestamp(seconds, 0)
}
