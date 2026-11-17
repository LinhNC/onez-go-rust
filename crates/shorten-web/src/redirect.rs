//! The hot path: `/{slug}`.
//!
//! Only `GET` and `HEAD` are answered, the redirect cache absorbs repeat
//! lookups, and a successful `GET` hands the visit to the analytics collector.

use axum::extract::{Path, State};
use axum::http::header::HeaderMap;
use axum::http::header::{self, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use chrono::Utc;

use crate::context::{self, is_get_or_head};
use crate::state::{not_found, text_error, App};
use shorten::link::{self, DomainError};
use shorten::observability::RedirectOutcome;

/// Resolve one short code.
pub async fn handle(
    State(app): State<App>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    if !is_get_or_head(&method) {
        let mut response = text_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.");
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        app.metrics().redirect(RedirectOutcome::MethodNotAllowed);
        return response;
    }
    if !link::valid_slug(&slug) {
        app.metrics().redirect(RedirectOutcome::Missing);
        return not_found();
    }

    let now = Utc::now();
    let value = if let Some(value) = app.cache().get(&slug, now) {
        value
    } else {
        let store_slug = slug.clone();
        match app
            .db("redirect lookup", move |store| {
                store.get_by_slug(&store_slug)
            })
            .await
        {
            Ok(item) => {
                app.cache().put_found(&slug, item.clone(), now);
                shorten::redirectcache::Value {
                    found: true,
                    link: Some(item),
                }
            }
            Err(DomainError::NotFound) => {
                app.cache().put_missing(&slug, now);
                app.metrics().redirect(RedirectOutcome::Missing);
                return not_found();
            }
            Err(error) => {
                tracing::error!(slug = %slug, error = %error, "redirect lookup failed");
                app.metrics().redirect(RedirectOutcome::Error);
                return text_error(StatusCode::SERVICE_UNAVAILABLE, "Service unavailable.");
            }
        }
    };

    if !value.found {
        app.metrics().redirect(RedirectOutcome::Missing);
        return not_found();
    }
    let Some(item) = value.link else {
        app.metrics().redirect(RedirectOutcome::Missing);
        return not_found();
    };

    if !item.is_available(now) {
        let outcome = if item.is_expired(now) {
            RedirectOutcome::Expired
        } else {
            RedirectOutcome::Disabled
        };
        app.metrics().redirect(outcome);
        return text_error(StatusCode::GONE, "This short link is no longer available.");
    }

    if method == Method::GET && !app.analytics().record(item.id, now) {
        tracing::error!(link_id = item.id, "analytics backlog rejected visit");
    }
    app.metrics().redirect(RedirectOutcome::Success);

    let status =
        StatusCode::from_u16(u16::try_from(item.redirect_status.clamp(300, 399)).unwrap_or(302))
            .unwrap_or(StatusCode::FOUND);
    if HeaderValue::try_from(item.destination_url.as_str()).is_err() {
        // A destination that is not a legal header value is a data fault;
        // answer the same way the previous service answered a broken link.
        tracing::error!(link_id = item.id, "redirect destination is not a valid URL");
        return text_error(StatusCode::SERVICE_UNAVAILABLE, "Service unavailable.");
    }
    // `http.Redirect` wrote the status text body for a GET; the Go server
    // suppressed it for HEAD.
    context::go_redirect(
        status,
        &item.destination_url,
        &headers,
        method != Method::HEAD,
    )
}
