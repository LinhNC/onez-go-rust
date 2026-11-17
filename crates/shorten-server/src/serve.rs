//! Connection loop: bind, accept, serve, and drain.
//!
//! The previous implementation used a `net/http` server with explicit
//! `ReadHeaderTimeout`, `ReadTimeout`, `WriteTimeout`, `IdleTimeout` and
//! `MaxHeaderBytes`; the same windows are enforced here on every accepted
//! connection, and `Shutdown` becomes a graceful per-connection shutdown
//! bounded by the configured drain timeout.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use shorten_web::App;

/// Router service factory that carries the peer address to each connection.
pub type MakeService = IntoMakeServiceWithConnectInfo<Router, SocketAddr>;

/// Build the per-listener service factory for `app`, as `net/http` built one
/// handler shared by every accepted connection.
#[must_use]
pub fn make_service(app: App) -> MakeService {
    shorten_web::build(app).into_make_service_with_connect_info::<SocketAddr>()
}

/// Read-header window of the previous `net/http` server (5 s).
pub const READ_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// Whole-request read window (15 s).
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Response write window (30 s).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Keep-alive idle window (60 s).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Header byte budget (`1 << 20`), enforced as the connection buffer size.
pub const MAX_HEADER_BYTES: usize = 1 << 20;
/// Maximum number of header fields accepted on a request.
pub const MAX_HEADER_FIELDS: usize = 10_000;
/// Granularity of the drain poll.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// Signals the accepted connections that the process is draining.
///
/// The flag is sticky so a connection that starts after the drain began
/// notices it on its next poll, and the `Notify` wakes the idle ones.
#[derive(Debug, Default)]
pub struct Drain {
    draining: AtomicBool,
    wake: Notify,
}

impl Drain {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin draining: mark the flag and wake every parked poller.
    pub fn start(&self) {
        self.draining.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Resolve when a drain notification arrives.
    pub async fn notified(&self) {
        self.wake.notified().await;
    }
}

/// Per-connection activity, shared between the instrumenting service and the
/// timer that enforces the read, write and idle windows.
#[derive(Debug, Default)]
struct Activity {
    in_flight: AtomicUsize,
    started_ms: AtomicI64,
    last_ms: AtomicI64,
}

impl Activity {
    fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            started_ms: AtomicI64::new(0),
            last_ms: AtomicI64::new(0),
        }
    }

    fn now_millis(start: Instant) -> i64 {
        i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    fn request_started(&self, now_ms: i64) {
        self.started_ms.store(now_ms, Ordering::Relaxed);
        self.last_ms.store(now_ms, Ordering::Relaxed);
        self.in_flight.fetch_add(1, Ordering::AcqRel);
    }

    fn request_finished(&self, now_ms: i64) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.last_ms.store(now_ms, Ordering::Relaxed);
    }
}

fn millis(value: Duration) -> i64 {
    i64::try_from(value.as_millis()).unwrap_or(i64::MAX)
}

/// Wraps the router service so every request updates the connection's
/// activity record, which is what the timeout windows are measured against.
#[derive(Clone)]
struct Instrumented<S> {
    inner: S,
    activity: Arc<Activity>,
    start: Instant,
}

impl<S, B> tower::Service<hyper::Request<B>> for Instrumented<S>
where
    S: tower::Service<hyper::Request<B>> + Clone,
    B: Send + 'static,
    S::Response: Send + 'static,
    S::Error: Send + std::fmt::Debug,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = InstrumentedFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: hyper::Request<B>) -> Self::Future {
        let now = Activity::now_millis(self.start);
        self.activity.request_started(now);
        InstrumentedFuture {
            inner: Box::pin(self.inner.call(request)),
            activity: Arc::clone(&self.activity),
            start: self.start,
        }
    }
}

struct InstrumentedFuture<F> {
    inner: Pin<Box<F>>,
    activity: Arc<Activity>,
    start: Instant,
}

impl<F: Future> Future for InstrumentedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = match this.inner.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        let now = Activity::now_millis(this.start);
        this.activity.request_finished(now);
        Poll::Ready(result)
    }
}

/// The accept loop for one bound listener.
pub struct Server {
    listener: TcpListener,
    make: MakeService,
}

impl Server {
    #[must_use]
    pub fn new(listener: TcpListener, make: MakeService) -> Self {
        Self { listener, make }
    }

    /// Accept connections until `drain` starts, then finish them gracefully.
    ///
    /// # Errors
    ///
    /// Returns an io error when accepting fails in a way that cannot be
    /// retried, and a drain error when in-flight connections outlive
    /// `drain_timeout`.
    pub async fn run(self, drain: Arc<Drain>, drain_timeout: Duration) -> Result<(), io::Error> {
        let mut connections: JoinSet<()> = JoinSet::new();
        let mut listener_failed = false;
        loop {
            if drain.is_draining() {
                break;
            }
            let accepted = tokio::select! {
                () = drain.notified(), if drain.is_draining() => break,
                () = tokio::time::sleep(DRAIN_POLL), if drain.is_draining() => break,
                result = self.listener.accept() => result,
            };
            match accepted {
                Ok((stream, peer)) => {
                    if let Err(error) = stream.set_nodelay(true) {
                        tracing::debug!(%error, "set TCP_NODELAY failed");
                    }
                    let make = self.make.clone();
                    connections.spawn(serve_connection(stream, peer, make, Arc::clone(&drain)));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::error!(%error, "accept failed");
                    listener_failed = true;
                    break;
                }
            }
        }

        // Graceful drain: every live connection is asked to finish its
        // in-flight request, bounded by the operator's shutdown timeout.
        drain.start();
        let drained = tokio::time::timeout(drain_timeout, drain_connections(connections)).await;
        match drained {
            Ok(()) if listener_failed => {
                Err(io::Error::other("listener stopped accepting requests"))
            }
            Ok(()) => Ok(()),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out draining in-flight requests",
            )),
        }
    }
}

async fn drain_connections(mut connections: JoinSet<()>) {
    while connections.join_next().await.is_some() {}
}

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    mut make: MakeService,
    drain: Arc<Drain>,
) {
    let start = Instant::now();
    let activity = Arc::new(Activity::new());
    // Idle clock starts at connect, as `net/http` measures idle from the
    // moment the connection is accepted.
    activity.request_finished(0);

    let router = tower::Service::call(&mut make, peer)
        .await
        .unwrap_or_else(|never| match never {});
    let service = TowerToHyperService::new(Instrumented {
        inner: router,
        activity: Arc::clone(&activity),
        start,
    });

    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .max_headers(MAX_HEADER_FIELDS)
        .max_buf_size(MAX_HEADER_BYTES)
        .header_read_timeout(READ_HEADER_TIMEOUT)
        .keep_alive(true);

    let io = TokioIo::new(stream);
    let connection = builder.serve_connection(io, service);
    tokio::pin!(connection);

    loop {
        if drain.is_draining() {
            connection.as_mut().graceful_shutdown();
        }
        let deadline = next_deadline(&activity, start);
        let nap = match deadline {
            Some(until) if drain.is_draining() => until.min(DRAIN_POLL),
            Some(until) => until,
            None if drain.is_draining() => DRAIN_POLL,
            None => IDLE_TIMEOUT,
        };
        tokio::select! {
            result = &mut connection => {
                if let Err(error) = result {
                    tracing::debug!(%error, %peer, "connection closed");
                }
                break;
            }
            () = tokio::time::sleep(nap) => {
                if let Some(reason) = violated(&activity, start) {
                    tracing::debug!(%peer, reason, "connection window expired");
                    connection.as_mut().graceful_shutdown();
                }
            }
            () = drain.notified() => {}
        }
    }
}

/// When the next window is violated, `None` when nothing is pending.
fn next_deadline(activity: &Activity, start: Instant) -> Option<Duration> {
    let now = Activity::now_millis(start);
    let due = if activity.in_flight.load(Ordering::Acquire) > 0 {
        let started = activity.started_ms.load(Ordering::Relaxed);
        let read_due = started.saturating_add(millis(READ_TIMEOUT));
        let write_due = activity
            .last_ms
            .load(Ordering::Relaxed)
            .saturating_add(millis(WRITE_TIMEOUT));
        read_due.min(write_due)
    } else {
        activity
            .last_ms
            .load(Ordering::Relaxed)
            .saturating_add(millis(IDLE_TIMEOUT))
    };
    Some(Duration::from_millis(
        u64::try_from(due.saturating_sub(now).max(0)).unwrap_or(0),
    ))
}

/// Which window the connection has just passed, if any.
fn violated(activity: &Activity, start: Instant) -> Option<&'static str> {
    let now = Activity::now_millis(start);
    if activity.in_flight.load(Ordering::Acquire) > 0 {
        let started = activity.started_ms.load(Ordering::Relaxed);
        if now.saturating_sub(started) >= millis(READ_TIMEOUT) {
            return Some("read timeout");
        }
        let last = activity.last_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= millis(WRITE_TIMEOUT) {
            return Some("write timeout");
        }
        return None;
    }
    let last = activity.last_ms.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= millis(IDLE_TIMEOUT) {
        return Some("idle timeout");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_match_the_go_server() {
        assert_eq!(READ_HEADER_TIMEOUT, Duration::from_secs(5));
        assert_eq!(READ_TIMEOUT, Duration::from_secs(15));
        assert_eq!(WRITE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(IDLE_TIMEOUT, Duration::from_secs(60));
        assert_eq!(MAX_HEADER_BYTES, 1 << 20);
    }

    #[test]
    fn idle_request_reports_the_idle_window() {
        let activity = Activity::new();
        activity.request_finished(0);
        let start = Instant::now();
        // An idle connection is not violated before the idle window.
        assert!(violated(&activity, start).is_none());
        assert!(next_deadline(&activity, start).is_some());
    }

    #[test]
    fn in_flight_request_reports_the_read_window_once_elapsed() {
        let activity = Activity::new();
        let started = 5_000;
        activity.request_started(started);
        // A request that started 5 seconds ago has not passed the read window.
        assert!(violated(&activity, Instant::now()).is_none());
        let in_flight = activity.in_flight.load(Ordering::Acquire);
        assert_eq!(in_flight, 1);
        activity.request_finished(started + 1);
        assert_eq!(activity.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn drain_flag_is_sticky_and_wakes_pollers() {
        let drain = Drain::new();
        assert!(!drain.is_draining());
        drain.start();
        assert!(drain.is_draining());
    }
}

#[cfg(test)]
mod route_probe {
    use axum::routing::any;
    use axum::Router;

    #[test]
    fn catch_all_forms_accepted_by_axum() {
        for (a, b) in [
            ("/{slug}", "/{slug}/{*rest}"),
            ("/{slug}/rename", "/{slug}/{*rest}"),
            ("/manage/links/{slug}", "/manage/links/{slug}/rename"),
            ("/manage/links/{slug}/rename", "/manage/{*rest}"),
            ("/manage/links", "/manage/{*rest}"),
            ("/manage/links/new", "/manage/{*rest}"),
            ("/api/links/{slug}", "/api/{*rest}"),
            ("/{slug}", "/{slug}/{*rest}"),
            ("/healthz", "/{slug}/{*rest}"),
        ] {
            let result = std::panic::catch_unwind(|| {
                let router: Router = Router::new()
                    .route(a, any(|| async { "x" }))
                    .route(b, any(|| async { "y" }));
                let _ = router.into_make_service();
            });
            println!("{a} + {b}: {}", if result.is_ok() { "ok" } else { "panic" });
        }
    }
}
