//! Visit analytics: a sharded aggregation buffer flushed to the store.
//!
//! A redirect is not willing to wait on a database write, so visits accumulate in
//! memory: a counter plus the newest timestamp per link. A background thread drains
//! the buffer every `interval`, and a record that pushes it past `threshold` asks
//! for an immediate drain. Sharding (`id % 32`, as before) keeps a hot link from
//! making every redirect contend on one lock. Nothing here is async:
//! [`Collector::start`] spawns a plain OS thread that uses a channel with a
//! timeout as its ticker; the HTTP layer owns async work.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::link::DomainError;

/// Visits accumulated for one link since the last successful flush.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisitDelta {
    /// How many visits are owed to this link.
    pub count: i64,
    /// The newest visit timestamp seen for this link.
    pub latest: DateTime<Utc>,
}

/// The store side of the collector, implemented by the SQLite store.
///
/// A failing flush is retried, so implementations must be safe to call repeatedly
/// with the same batch.
pub trait Flusher: Send + Sync + 'static {
    /// Apply every delta in one batch, as a single store transaction.
    ///
    /// # Errors
    ///
    /// Any store failure, which the collector treats as retriable.
    fn apply_visits(&self, deltas: &HashMap<i64, VisitDelta>) -> Result<(), DomainError>;
}

/// Shard count, kept at the previous value so aggregation matches.
const SHARDS: usize = 32;
/// Flush attempts per batch, matching the Go `for attempt := 0; attempt < 4`.
const ATTEMPTS: u32 = 4;
/// Unit of the exponential backoff, in milliseconds: `25ms << attempt`.
const BACKOFF_UNIT_MS: u64 = 25;
/// Floor of the health window: a success within 5s, or within four flush intervals
/// when the operator configured longer ones.
const MIN_HEALTH_WINDOW_MS: i64 = 5_000;
/// Backlog overflow message, byte-identical to the Go one.
const CAPACITY_MESSAGE: &str = "analytics backlog capacity exceeded";
const SHARD_LOCK: &str = "analytics shard lock poisoned";
const ERROR_LOCK: &str = "analytics error lock poisoned";

/// One shard of the aggregation buffer.
#[derive(Debug, Default)]
struct Shard {
    values: HashMap<i64, VisitDelta>,
}

/// Shard owning `id`, matching `uint64(id) % 32` of the Go version.
fn shard_index(id: i64) -> usize {
    usize::try_from(id.rem_euclid(i64::try_from(SHARDS).unwrap_or(1))).unwrap_or(0)
}

/// Whole milliseconds of a duration.
fn millis(value: Duration) -> i64 {
    i64::try_from(value.as_millis()).unwrap_or(i64::MAX)
}

/// Shared collector state; a [`Collector`] is a cheap handle to it.
struct Inner {
    store: Arc<dyn Flusher>,
    shards: Vec<Mutex<Shard>>,
    interval: Duration,
    threshold: i64,
    max_keys: i64,
    trigger: SyncSender<()>,
    /// In a mutex so [`Collector::start`] can move the receiver to its thread.
    trigger_rx: Mutex<Option<Receiver<()>>>,
    flushing: Mutex<()>,
    pending: AtomicI64,
    dropped: AtomicI64,
    last_success_ms: AtomicI64,
    last_error: Mutex<Option<String>>,
    stopping: AtomicBool,
}

impl Inner {
    fn record(&self, id: i64, at: DateTime<Utc>) -> bool {
        let mut shard = self.shard(id).lock().expect(SHARD_LOCK);
        let mut delta = shard.values.get(&id).copied().unwrap_or_default();
        let exists = shard.values.contains_key(&id);
        if !exists && self.pending.load(Ordering::Acquire) >= self.max_keys {
            drop(shard);
            self.dropped.fetch_add(1, Ordering::AcqRel);
            self.set_error(Some(CAPACITY_MESSAGE.to_string()));
            return false;
        }
        delta.count += 1;
        if delta.latest < at {
            delta.latest = at;
        }
        shard.values.insert(id, delta);
        if !exists {
            self.pending.fetch_add(1, Ordering::AcqRel);
        }
        let pending = self.pending.load(Ordering::Acquire);
        drop(shard);
        if pending >= self.threshold {
            // Non-blocking: a queued trigger already covers this request.
            let _ = self.trigger.try_send(());
        }
        true
    }

    /// Swap the whole buffer into one batch, leaving the shards empty.
    fn swap(&self) -> HashMap<i64, VisitDelta> {
        let mut batch = HashMap::new();
        for shard in &self.shards {
            let mut shard = shard.lock().expect(SHARD_LOCK);
            batch.extend(shard.values.iter().map(|(id, delta)| (*id, *delta)));
            shard.values.clear();
        }
        self.pending.fetch_add(
            -i64::try_from(batch.len()).unwrap_or(i64::MAX),
            Ordering::AcqRel,
        );
        batch
    }

    /// Fold a failed batch back into the shards so no visit is lost.
    fn merge(&self, batch: &HashMap<i64, VisitDelta>) {
        for (id, delta) in batch {
            let mut shard = self.shard(*id).lock().expect(SHARD_LOCK);
            let mut current = shard.values.get(id).copied().unwrap_or_default();
            let exists = shard.values.contains_key(id);
            current.count += delta.count;
            if current.latest < delta.latest {
                current.latest = delta.latest;
            }
            shard.values.insert(*id, current);
            if !exists {
                self.pending.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    /// Drain the buffer, retrying the flusher with exponential backoff.
    fn flush(&self) -> Result<(), DomainError> {
        // One flush at a time: the ticker and the trigger can both fire.
        let _serialise = self.flushing.lock().expect("analytics flush lock poisoned");
        let batch = self.swap();
        if batch.is_empty() {
            self.mark_success();
            return Ok(());
        }
        let mut failure = DomainError::Message("analytics flush failed".to_string());
        for attempt in 0..ATTEMPTS {
            match self.store.apply_visits(&batch) {
                Ok(()) => {
                    self.mark_success();
                    return Ok(());
                }
                Err(error) => failure = error,
            }
            std::thread::sleep(Duration::from_millis(BACKOFF_UNIT_MS << attempt));
        }
        self.merge(&batch);
        self.set_error(Some(failure.to_string()));
        Err(failure)
    }

    /// Background loop: the channel timeout doubles as the flush ticker.
    fn run(self: Arc<Self>) {
        let taken = self
            .trigger_rx
            .lock()
            .expect("analytics trigger lock poisoned")
            .take();
        let Some(receiver) = taken else { return };
        while !self.is_stopping() {
            match receiver.recv_timeout(self.interval) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {
                    let _ = self.flush();
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Record a successful drain: stamp the success instant, clear the failure.
    fn mark_success(&self) {
        self.last_success_ms
            .store(Utc::now().timestamp_millis(), Ordering::Release);
        self.set_error(None);
    }

    fn healthy(&self, now: DateTime<Utc>) -> bool {
        let window_ms = millis(self.interval)
            .saturating_mul(4)
            .max(MIN_HEALTH_WINDOW_MS);
        let elapsed = now
            .timestamp_millis()
            .saturating_sub(self.last_success_ms.load(Ordering::Acquire));
        self.dropped.load(Ordering::Acquire) == 0
            && self.pending.load(Ordering::Acquire) < self.max_keys
            && elapsed < window_ms
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Record the flush outcome; `None` clears the recorded failure.
    fn set_error(&self, message: Option<String>) {
        *self.last_error.lock().expect(ERROR_LOCK) = message;
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.lock().expect(ERROR_LOCK).clone()
    }

    fn shard(&self, id: i64) -> &Mutex<Shard> {
        &self.shards[shard_index(id)]
    }
}

/// Collects visits in memory and flushes them to the store in batches.
///
/// Cloning is cheap: every handle sees the same buffer, the same counters and the
/// same background thread.
#[derive(Clone)]
pub struct Collector {
    inner: Arc<Inner>,
}

impl Collector {
    /// Build a collector that flushes to `store` every `interval`.
    ///
    /// `threshold` is the number of pending links that triggers an immediate
    /// flush; `max_keys` caps the backlog, and a record of a *new* link beyond it
    /// is dropped.
    #[must_use]
    pub fn new(
        store: Arc<dyn Flusher>,
        interval: Duration,
        threshold: usize,
        max_keys: usize,
    ) -> Self {
        let (trigger, receiver) = sync_channel(1);
        let inner = Inner {
            store,
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            interval,
            threshold: i64::try_from(threshold).unwrap_or(i64::MAX),
            max_keys: i64::try_from(max_keys).unwrap_or(i64::MAX),
            trigger,
            trigger_rx: Mutex::new(Some(receiver)),
            flushing: Mutex::new(()),
            pending: AtomicI64::new(0),
            dropped: AtomicI64::new(0),
            last_success_ms: AtomicI64::new(Utc::now().timestamp_millis()),
            last_error: Mutex::new(None),
            stopping: AtomicBool::new(false),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Record one visit, returning false when the backlog is full.
    ///
    /// Visits of a link already in the buffer are always accepted; only a new key
    /// can be rejected by `max_keys`.
    #[must_use]
    pub fn record(&self, id: i64, at: DateTime<Utc>) -> bool {
        self.inner.record(id, at)
    }

    /// Drain the buffer into the store, retrying up to four times.
    ///
    /// On failure the whole batch is merged back so no visit is lost.
    ///
    /// # Errors
    ///
    /// The store failure of the last attempt.
    pub fn flush(&self) -> Result<(), DomainError> {
        self.inner.flush()
    }

    /// Spawn the background flush loop and return its handle.
    ///
    /// The loop ticks every `interval`, flushes on a trigger, and exits once
    /// [`Collector::stop`] has been called. Only the first call owns the receiver,
    /// so a later call spawns a thread that returns immediately.
    #[must_use]
    pub fn start(&self) -> JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        std::thread::spawn(move || inner.run())
    }

    /// Ask the background loop to exit.
    pub fn stop(&self) {
        self.inner.stopping.store(true, Ordering::Release);
        // Wake the loop instead of waiting out the interval.
        let _ = self.inner.trigger.try_send(());
    }

    /// Whether [`Collector::stop`] has been called.
    pub fn is_stopping(&self) -> bool {
        self.inner.is_stopping()
    }

    /// Whether visits are still being drained and the backlog is not full.
    #[must_use]
    pub fn healthy(&self, now: DateTime<Utc>) -> bool {
        self.inner.healthy(now)
    }

    /// Number of links waiting in the buffer.
    pub fn pending(&self) -> i64 {
        self.inner.pending.load(Ordering::Acquire)
    }

    /// Number of visits dropped because the backlog was full.
    pub fn dropped(&self) -> i64 {
        self.inner.dropped.load(Ordering::Acquire)
    }

    /// Text of the most recent flush failure, `None` after a success.
    pub fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as Span, TimeZone};
    use std::sync::MutexGuard;

    /// Test double: fails the first `fail` calls, like the Go `fakeFlusher`.
    #[derive(Debug, Default)]
    struct FakeFlusher {
        inner: Mutex<FakeState>,
        fail: i64,
    }

    #[derive(Debug, Default)]
    struct FakeState {
        calls: i64,
        totals: HashMap<i64, i64>,
    }

    impl FakeFlusher {
        fn new(fail: i64) -> Self {
            Self {
                inner: Mutex::new(FakeState::default()),
                fail,
            }
        }

        fn state(&self) -> MutexGuard<'_, FakeState> {
            self.inner.lock().expect("fake flusher lock")
        }

        fn total(&self, id: i64) -> i64 {
            self.state().totals.get(&id).copied().unwrap_or(0)
        }

        fn calls(&self) -> i64 {
            self.state().calls
        }
    }

    impl Flusher for FakeFlusher {
        fn apply_visits(&self, deltas: &HashMap<i64, VisitDelta>) -> Result<(), DomainError> {
            let mut state = self.state();
            state.calls += 1;
            if state.calls <= self.fail {
                return Err(DomainError::Message("busy".to_string()));
            }
            for (id, delta) in deltas {
                *state.totals.entry(*id).or_insert(0) += delta.count;
            }
            Ok(())
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .single()
            .expect("fixed date is valid")
    }

    fn collector(
        flusher: &Arc<FakeFlusher>,
        interval_ms: u64,
        threshold: usize,
        max_keys: usize,
    ) -> Collector {
        Collector::new(
            Arc::clone(flusher) as Arc<dyn Flusher>,
            Duration::from_millis(interval_ms),
            threshold,
            max_keys,
        )
    }

    #[test]
    fn aggregates_and_retries_two_failures() {
        let flusher = Arc::new(FakeFlusher::new(2));
        let collector = collector(&flusher, 1000, 100, 1000);
        let now = fixed_now();
        for step in 0..25u32 {
            let at = now + Span::try_milliseconds(i64::from(step)).unwrap_or_default();
            assert!(collector.record(7, at), "record rejected");
        }
        collector.flush().expect("flush should succeed");

        assert_eq!(flusher.total(7), 25, "visits were not aggregated");
        assert_eq!(collector.pending(), 0, "buffer not drained");
        assert_eq!(flusher.calls(), 3, "retry behaviour changed");
        assert_eq!(collector.dropped(), 0);
        assert!(collector.last_error().is_none(), "error not cleared");
        assert!(collector.healthy(Utc::now()), "success must restore health");
        assert!(
            !collector.healthy(Utc::now() + Span::seconds(6)),
            "success ages out"
        );
    }

    #[test]
    fn backlog_limit_drops_new_keys_only() {
        let flusher = Arc::new(FakeFlusher::default());
        let collector = collector(&flusher, 1000, 100, 1);
        let now = fixed_now();
        assert!(collector.record(1, now));
        assert!(!collector.record(2, now), "capacity behaviour failed");
        assert!(collector.record(1, now), "existing key must stay accepted");

        assert_eq!(collector.dropped(), 1, "drop metric missing");
        assert_eq!(collector.pending(), 1);
        assert_eq!(collector.last_error().as_deref(), Some(CAPACITY_MESSAGE));
        assert!(!collector.healthy(now), "full backlog must not be healthy");
    }

    #[test]
    fn idle_flush_keeps_the_collector_healthy() {
        let flusher = Arc::new(FakeFlusher::default());
        let collector = collector(&flusher, 250, 100, 1000);
        let now = fixed_now();
        collector
            .inner
            .last_success_ms
            .store(now.timestamp_millis() - 60_000, Ordering::SeqCst);

        collector.flush().expect("idle flush should succeed");

        assert!(collector.healthy(now), "idle collector became unhealthy");
        assert_eq!(flusher.calls(), 0, "idle flush must not call the store");
        assert!(collector.last_error().is_none());
    }

    #[test]
    fn failed_flush_is_merged_back_and_recorded() {
        let flusher = Arc::new(FakeFlusher::new(99));
        let collector = collector(&flusher, 1000, 100, 1000);
        let now = fixed_now();
        assert!(collector.record(11, now));
        assert!(collector.record(11, now + Span::seconds(1)));

        let error = collector.flush().expect_err("flush should fail");
        assert_eq!(error.to_string(), "busy");
        assert_eq!(flusher.calls(), 4, "attempt count changed");
        assert_eq!(collector.pending(), 1, "batch was not merged back");
        assert_eq!(collector.last_error().as_deref(), Some("busy"));
        assert!(
            collector.healthy(Utc::now()),
            "a fresh failure is inside the window"
        );
        assert!(
            !collector.healthy(Utc::now() + Span::seconds(6)),
            "failure ages out"
        );

        collector.flush().expect_err("still failing");
        assert_eq!(collector.pending(), 1, "merged visits were lost");
        assert_eq!(flusher.total(11), 0);
    }

    #[test]
    fn threshold_triggers_the_loop_and_stop_exits_it() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Collector>();
        assert_send_sync::<VisitDelta>();

        let flusher = Arc::new(FakeFlusher::default());
        let collector = collector(&flusher, 50, 1, 1000);
        let handle = collector.start();
        assert!(collector.record(3, fixed_now()));

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while collector.pending() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(collector.pending(), 0, "triggered flush never drained");
        assert_eq!(flusher.total(3), 1);
        collector.stop();
        assert!(collector.is_stopping());
        handle
            .join()
            .expect("collector thread must exit after stop");
    }

    #[test]
    fn clones_share_one_buffer_across_shards() {
        let collector = collector(&Arc::new(FakeFlusher::default()), 1000, 100, 1000);
        let clone = collector.clone();
        // Ids in the same shard (`id % 32`), including a negative one.
        assert!(clone.record(5, fixed_now()));
        assert!(clone.record(37, fixed_now()));
        assert!(collector.record(-27, fixed_now()));
        assert_eq!(collector.pending(), 3, "clone sees a different buffer");
        collector.flush().expect("flush should succeed");
        assert_eq!(clone.pending(), 0, "clone must see the drained buffer");
    }
}
