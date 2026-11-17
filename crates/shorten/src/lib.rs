//! Shorten domain layer.
//!
//! This crate holds everything the Shorten service needs apart from the HTTP
//! transport: configuration, the link lifecycle rules, authentication
//! primitives, the redirect cache, the visit-analytics collector, operational
//! metrics and the SQLite store.
//!
//! The module boundaries intentionally mirror the previous Go implementation
//! (`internal/config`, `internal/link`, `internal/auth`, `internal/redirectcache`,
//! `internal/analytics`, `internal/observability`, `internal/store/sqlite`) so
//! that behaviour, wire formats and database contents stay compatible.

pub mod analytics;
pub mod auth;
pub mod config;
pub mod json;
pub mod link;
pub mod observability;
pub mod redirectcache;
pub mod store;
pub mod time;

pub use analytics::{Collector, Flusher, VisitDelta};
pub use auth::{ApiKey, Claims, Limiter, Sessions, COOKIE_NAME};
pub use config::Config;
pub use link::{CreateInput, DomainError, Link, Status, UpdateInput, ValidationErrors};
pub use observability::Metrics;
pub use redirectcache::{Cache, Stats as CacheStats, Value as CachedRedirect};
pub use store::{
    CleanupPreview, ImportOutcome, ListOptions, ListResult, Store, StoreError, Summary,
};
