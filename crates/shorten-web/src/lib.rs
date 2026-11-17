//! HTTP layer for Shorten: redirect path, JSON API and the HTMX console.
//!
//! The module layout mirrors the previous Go service:
//!
//! * [`state`] — shared application state and response helpers.
//! * [`context`] — request helpers: client IP, origin checks, CSRF, form and
//!   query reading, HTMX detection.
//! * [`models`] — view models and presentation formatting.
//! * [`redirect`] — the hot path (`/{slug}`).
//! * [`api`] — the versionless JSON API under `/api`.
//! * [`manage`] — the cookie-session management console.
//! * [`router`] — the route table and middleware stack.
//! * [`assets`] — embedded stylesheets, scripts and icons.
//! * [`templates`] — Askama render types for the console pages and fragments.

#![forbid(unsafe_code)]
#![warn(clippy::all, clippy::pedantic)]

pub mod api;
pub mod assets;
pub mod context;
pub mod manage;
pub mod models;
pub mod redirect;
pub mod router;
pub mod state;
pub mod templates;

pub use assets::Assets;
pub use models::{
    form_from_link, short_url, CleanupModel, ConfirmModel, LinkFormModel, LinksModel, LoginModel,
    PageMeta,
};
pub use router::build;
pub use state::write_json;
pub use state::App;
