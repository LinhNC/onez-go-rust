//! Redirect cache: a least-recently-used, time-to-live cache of redirect lookups.
//!
//! Positive entries (a slug that resolved) and negative entries (a slug that was
//! missing) expire independently, so a deleted link cannot resurrect through a
//! stale record while a typo costs nothing.
//!
//! Recency is a slab: nodes live in a `Vec`, threaded through a doubly linked list
//! by index, with freed slots recycled from a free list, plus a
//! `HashMap<String, usize>` from slug to slot. No external crate is needed, which
//! mirrors the Go implementation's `container/list` design. Counters are monotonic
//! and survive [`Cache::clear`]: the metrics endpoint reports lifetime values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::link::Link;

/// One cached lookup result: `found` distinguishes a resolved link from a
/// recorded miss, and a negative entry carries no link.
#[derive(Clone, Debug)]
pub struct Value {
    /// True when the slug resolved to a link.
    pub found: bool,
    /// The resolved link; `None` for a negative entry.
    pub link: Option<Link>,
}

/// A snapshot of the cache counters, matching the Go `redirectcache.Stats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Lifetime cache hits.
    pub hits: u64,
    /// Lifetime cache misses, including expired entries.
    pub misses: u64,
    /// Lifetime capacity evictions.
    pub evictions: u64,
    /// Live entries right now.
    pub size: usize,
}

/// Sentinel for "no node" in the recency list.
const NIL: usize = usize::MAX;

/// One cache entry plus its neighbours in the recency list.
#[derive(Debug)]
struct Node {
    slug: String,
    value: Value,
    expires: DateTime<Utc>,
    prev: usize,
    next: usize,
}

/// The recency list plus the slug index; all cache state lives here.
#[derive(Debug)]
struct Lru {
    nodes: Vec<Node>,
    free: Vec<usize>,
    index: HashMap<String, usize>,
    head: usize,
    tail: usize,
}

impl Default for Lru {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            free: Vec::new(),
            index: HashMap::new(),
            head: NIL,
            tail: NIL,
        }
    }
}

impl Lru {
    fn len(&self) -> usize {
        self.index.len()
    }

    /// Insert a fresh entry at the front of the recency list, recycling a freed
    /// slot when one is available and growing the slab otherwise.
    fn push_front(&mut self, slug: String, value: Value, expires: DateTime<Utc>) {
        let node = Node {
            slug,
            value,
            expires,
            prev: NIL,
            next: NIL,
        };
        let id = if let Some(id) = self.free.pop() {
            self.nodes[id] = node;
            id
        } else {
            self.nodes.push(node);
            self.nodes.len() - 1
        };
        self.index.insert(self.nodes[id].slug.clone(), id);
        self.attach_front(id);
    }

    /// Link `id` in front of the current head.
    fn attach_front(&mut self, id: usize) {
        self.nodes[id].prev = NIL;
        self.nodes[id].next = self.head;
        if self.head == NIL {
            self.tail = id;
        } else {
            self.nodes[self.head].prev = id;
        }
        self.head = id;
    }

    /// Detach `id` from the recency list, leaving its slot to the caller.
    fn unlink(&mut self, id: usize) {
        let (prev, next) = (self.nodes[id].prev, self.nodes[id].next);
        if prev == NIL {
            self.head = next;
        } else {
            self.nodes[prev].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.nodes[next].prev = prev;
        }
        self.nodes[id].prev = NIL;
        self.nodes[id].next = NIL;
    }

    /// Refresh an entry as most recently used.
    fn move_to_front(&mut self, id: usize) {
        if self.head != id {
            self.unlink(id);
            self.attach_front(id);
        }
    }

    /// Drop an entry entirely and recycle its slot.
    fn remove(&mut self, id: usize) {
        self.index.remove(&self.nodes[id].slug);
        self.unlink(id);
        self.free.push(id);
    }

    /// Forget every entry, keeping the slab allocated for reuse.
    fn clear(&mut self) {
        self.index.clear();
        self.head = NIL;
        self.tail = NIL;
        self.free.clear();
        self.free.extend(0..self.nodes.len());
    }
}

/// A time-to-live, least-recently-used cache of redirect lookups.
///
/// Shared behind a mutex, so it is `Send + Sync` and usable from many threads.
#[derive(Debug)]
pub struct Cache {
    capacity: usize,
    positive_ttl: Duration,
    negative_ttl: Duration,
    inner: Mutex<Lru>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl Cache {
    /// Build a cache holding `capacity` entries.
    ///
    /// `positive_ttl` bounds a resolved link and `negative_ttl` bounds a recorded
    /// miss, exactly as in the previous implementation.
    #[must_use]
    pub fn new(capacity: usize, positive_ttl: Duration, negative_ttl: Duration) -> Self {
        Self {
            capacity,
            positive_ttl,
            negative_ttl,
            inner: Mutex::new(Lru::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Look up a slug, counting a hit or a miss.
    ///
    /// An entry whose expiry is at or before `now` counts as a miss and is
    /// dropped; a hit refreshes the entry's recency.
    #[must_use]
    pub fn get(&self, slug: &str, now: DateTime<Utc>) -> Option<Value> {
        let mut lru = self.lock();
        let Some(&id) = lru.index.get(slug) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if lru.nodes[id].expires <= now {
            lru.remove(id);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let value = lru.nodes[id].value.clone();
        lru.move_to_front(id);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(value)
    }

    /// Cache a resolved link for `positive_ttl`.
    pub fn put_found(&self, slug: &str, link: Link, now: DateTime<Utc>) {
        self.put(
            slug,
            Value {
                found: true,
                link: Some(link),
            },
            self.positive_ttl,
            now,
        );
    }

    /// Cache a recorded miss for `negative_ttl`.
    pub fn put_missing(&self, slug: &str, now: DateTime<Utc>) {
        self.put(
            slug,
            Value {
                found: false,
                link: None,
            },
            self.negative_ttl,
            now,
        );
    }

    /// Drop the listed slugs, for example after an edit, a rename or a delete.
    pub fn invalidate(&self, slugs: &[&str]) {
        let mut lru = self.lock();
        for slug in slugs {
            if let Some(&id) = lru.index.get(*slug) {
                lru.remove(id);
            }
        }
    }

    /// Forget every entry. Counters keep counting.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Report the lifetime counters and the live entry count.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let lru = self.lock();
        Stats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            size: lru.len(),
        }
    }

    /// Insert or replace an entry with a fresh expiry, then enforce capacity by
    /// evicting from the back and counting one eviction per removed entry.
    fn put(&self, slug: &str, value: Value, ttl: Duration, now: DateTime<Utc>) {
        let expires = add_ttl(now, ttl);
        let mut lru = self.lock();
        if let Some(&id) = lru.index.get(slug) {
            lru.nodes[id].value = value;
            lru.nodes[id].expires = expires;
            lru.move_to_front(id);
            return;
        }
        lru.push_front(slug.to_string(), value, expires);
        while lru.len() > self.capacity {
            if lru.tail == NIL {
                break;
            }
            let tail = lru.tail;
            lru.remove(tail);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Lru> {
        self.inner.lock().expect("redirect cache lock poisoned")
    }
}

/// `now + ttl`, saturating at the representable timestamp range.
fn add_ttl(now: DateTime<Utc>, ttl: Duration) -> DateTime<Utc> {
    let span = chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX);
    now.checked_add_signed(span)
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Status;
    use chrono::{Duration as Span, TimeZone};

    const MINUTE: Duration = Duration::from_secs(60);
    const SECOND: Duration = Duration::from_secs(1);

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .single()
            .expect("fixed date is valid")
    }

    fn sample(slug: &str, now: DateTime<Utc>) -> Link {
        Link {
            id: 7,
            slug: slug.to_string(),
            destination_url: "https://example.com".to_string(),
            title: String::new(),
            notes: String::new(),
            status: Status::Active,
            redirect_status: 302,
            expires_at: None,
            visit_count: 0,
            last_visited_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn ttl_capacity_and_invalidation() {
        let now = fixed_now();
        let cache = Cache::new(2, MINUTE, SECOND);

        cache.put_found("a", sample("a", now), now);
        cache.put_found("b", sample("b", now), now);
        cache.put_found("c", sample("c", now), now);
        assert!(
            cache.get("a", now).is_none(),
            "oldest entry was not evicted"
        );

        cache.put_missing("missing", now);
        let value = cache.get("missing", now).expect("negative entry");
        assert!(!value.found, "negative cache failed");
        assert!(value.link.is_none(), "negative entry carried a link");
        assert!(
            cache.get("missing", now + Span::seconds(2)).is_none(),
            "expired negative cache remained"
        );

        cache.invalidate(&["c"]);
        assert!(cache.get("c", now).is_none(), "invalidation failed");

        cache.put_found("kept", sample("kept", now), now);
        // "c" was invalidated and the negative entry expired, so "kept" is the
        // only live entry.
        assert_eq!(cache.stats().size, 1);
        cache.clear();
        assert_eq!(cache.stats().size, 0, "clear left entries behind");
        assert!(cache.get("kept", now).is_none(), "cleared entry survived");
    }

    #[test]
    fn counters_survive_clear_and_expiry_counts_a_miss() {
        let now = fixed_now();
        let cache = Cache::new(4, MINUTE, SECOND);
        cache.put_found("a", sample("a", now), now);
        assert!(cache.get("a", now).is_some());
        cache.clear();

        let stats = cache.stats();
        assert_eq!(
            (stats.size, stats.hits, stats.misses, stats.evictions),
            (0, 1, 0, 0)
        );

        assert!(cache.get("a", now).is_none());
        assert!(cache.get("absent", now).is_none());
        let stats = cache.stats();
        assert_eq!(stats.hits, 1, "clear must not reset hits");
        assert_eq!(stats.misses, 2, "both lookups count as misses");
    }

    #[test]
    fn eviction_counts_one_per_removed_entry_and_expiry_is_inclusive() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Cache>();
        assert_send_sync::<Value>();
        assert_send_sync::<Stats>();
        let now = fixed_now();
        let cache = Cache::new(1, MINUTE, Duration::from_secs(10));
        cache.put_found("a", sample("a", now), now);
        cache.put_found("b", sample("b", now), now);

        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.stats().size, 1);
        assert!(cache.get("b", now).is_some());

        let at_expiry = now + Span::seconds(60);
        assert!(
            cache.get("b", at_expiry).is_none(),
            "expiry boundary is inclusive"
        );
        assert_eq!(cache.stats().size, 0);
    }

    #[test]
    fn hit_and_replace_refresh_recency_before_eviction() {
        let now = fixed_now();
        let cache = Cache::new(2, MINUTE, SECOND);
        cache.put_found("a", sample("a", now), now);
        cache.put_found("b", sample("b", now), now);
        assert!(cache.get("a", now).is_some(), "warm entry missing");
        cache.put_found("c", sample("c", now), now);
        assert!(
            cache.get("b", now).is_none(),
            "least recent entry was not evicted"
        );
        assert!(cache.get("a", now).is_some(), "hit entry was evicted");
        assert_eq!(cache.stats().hits, 2, "hits are counted per hit");

        cache.put_found("c", sample("c2", now), now);
        cache.put_found("a", sample("a2", now), now);
        assert_eq!(cache.stats().evictions, 1, "replace must not evict");
        assert_eq!(cache.stats().size, 2);
        let link = cache
            .get("a", now)
            .expect("replaced entry")
            .link
            .expect("link");
        assert_eq!(link.slug, "a2", "replace kept the old value");
    }
}
