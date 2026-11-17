//! Route table and the application-wide middleware.
//!
//! The method-and-path patterns, security headers, request identifiers,
//! metrics recording, logging rules, panic handling, and static-asset caching
//! are the contract of the previous `internal/web` server.

#![allow(clippy::needless_pass_by_value)] // middleware forwards the request it owns.
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::header::{self, HeaderValue};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{any, get, post};
use axum::{extract, Router};
use tower_http::catch_panic::CatchPanicLayer;

use crate::context;
use crate::context::normalized_route;
use crate::state::App;
use crate::{api, assets, manage, redirect, state};
use axum::http::header::HeaderMap;

/// Build the complete application router: routes, panic handling, and the
/// security middleware.
pub fn build(app: App) -> Router {
    let middleware_state = app.clone();
    Router::new()
        .merge(routes())
        .with_state(app)
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(axum::middleware::from_fn_with_state(
            middleware_state,
            security_middleware,
        ))
}

/// The route table, without state.
fn routes() -> Router<App> {
    Router::new()
        .route(
            "/assets/{*path}",
            get(assets_handler)
                .head(assets_handler)
                .fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/healthz",
            get(health).fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/readyz",
            get(ready).fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/metrics",
            get(metrics).fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/",
            get(root).fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/manage/login",
            get(manage::login_page)
                .post(manage::login)
                .fallback(|| async { state::method_not_allowed("GET, HEAD, POST") }),
        )
        .route(
            "/manage/logout",
            post(manage::logout).fallback(|| async { state::method_not_allowed("POST") }),
        )
        .route(
            "/manage/links",
            get(manage::links_page)
                .post(manage::create_link)
                .fallback(|| async { state::method_not_allowed("GET, HEAD, POST") }),
        )
        .route(
            "/manage/links/new",
            get(manage::create_page).fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/manage/cleanup",
            get(manage::cleanup_page)
                .post(manage::cleanup_links)
                .fallback(|| async { state::method_not_allowed("GET, HEAD, POST") }),
        )
        .route(
            "/manage/links/{slug}",
            get(manage::edit_page)
                .post(manage::update_link)
                .fallback(|| async { state::method_not_allowed("GET, HEAD, POST") }),
        )
        .route(
            "/manage/links/{slug}/rename",
            get(manage::rename_page)
                .post(manage::rename_link)
                .fallback(|| async { state::method_not_allowed("GET, HEAD, POST") }),
        )
        .route(
            "/manage/links/{slug}/delete",
            get(manage::delete_page)
                .post(manage::delete_link)
                .fallback(|| async { state::method_not_allowed("GET, HEAD, POST") }),
        )
        .route(
            "/api/shorten",
            post(api::create).fallback(|| async { state::method_not_allowed("POST") }),
        )
        .route(
            "/api/links",
            get(api::list).fallback(|| async { state::method_not_allowed("GET, HEAD") }),
        )
        .route(
            "/api/links/{slug}",
            get(api::get)
                .patch(api::patch)
                .delete(api::remove)
                .fallback(|| async { state::method_not_allowed("DELETE, GET, HEAD, PATCH") }),
        )
        // The slug route takes every method: the previous multiplexer
        // registered `/{slug}` without a method, and the redirect handler
        // wrote its own `Allow: GET, HEAD` response for the rest.
        .route("/{slug}", any(redirect::handle))
        // Registered as routes, not `Router::fallback`, so that unmatched
        // paths still pass through the security middleware: the previous
        // server set the security headers on every response, including the
        // not-found ones. A root-level catch-all would collide with the
        // single-segment slug route, so the prefixes are registered
        // individually, exactly as the previous multiplexer's 404 paths.
        .route("/manage/{*rest}", any(fallback))
        .route("/api/{*rest}", any(fallback))
        .route("/{slug}/{*rest}", any(fallback))
}

/// Unmatched paths return the plain text 404 of the previous server.
async fn fallback() -> Response {
    state::not_found()
}

/// `GET /` redirects to the console.
async fn root(headers: HeaderMap) -> Response {
    context::go_redirect(StatusCode::SEE_OTHER, "/manage/links", &headers, true)
}

/// `GET /healthz`.
async fn health() -> Response {
    let mut body = serde_json::Map::new();
    body.insert("status".to_string(), serde_json::Value::from("ok"));
    state::write_json(StatusCode::OK, &serde_json::Value::Object(body))
}

/// `GET /readyz`: readiness flag, a 500 ms store ping, and a healthy analytics
/// collector.
async fn ready(State(app): State<App>) -> Response {
    let value = if app.ready().await.unwrap_or(false) {
        "ready"
    } else {
        "not_ready"
    };
    let status = if value == "ready" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let mut body = serde_json::Map::new();
    body.insert("status".to_string(), serde_json::Value::from(value));
    state::write_json(status, &serde_json::Value::Object(body))
}

/// `GET /metrics`.
async fn metrics(State(app): State<App>) -> Response {
    let body = app.metrics().render(&|| app.metric_values());
    let mut response = Response::new(body.into());
    if let Ok(value) = HeaderValue::from_str("text/plain; version=0.0.4; charset=utf-8") {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// Serve one embedded asset by path.
async fn assets_handler(
    State(app): State<App>,
    extract::Path(path): extract::Path<String>,
    request: Request,
) -> Response {
    let _ = app;
    assets::serve(&path, request.method(), request.headers())
}

/// Security headers, request identifiers, metrics, and the request log.
async fn security_middleware(State(app): State<App>, request: Request, next: Next) -> Response {
    let start = Instant::now();
    let id = app.next_request_id();
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let status = response.status().as_u16();
    let headers = response.headers_mut();
    insert(headers, "X-Request-ID", &id);
    insert(headers, "X-Content-Type-Options", "nosniff");
    insert(headers, "Referrer-Policy", "no-referrer");
    insert(
        headers,
        "Permissions-Policy",
        "camera=(), microphone=(), geolocation=()",
    );
    insert(
        headers,
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    );
    let route = normalized_route(&path);
    app.metrics()
        .observe_request(&route, status, start.elapsed());
    if status >= 400 || route.starts_with("/api") || route.starts_with("/manage") {
        tracing::info!(
            request_id = %id,
            method = %method,
            route = %route,
            status,
            duration_ms = start.elapsed().as_millis(),
            "request",
        );
    }
    response
}

fn insert(headers: &mut axum::http::HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        axum::http::HeaderName::try_from(name),
        HeaderValue::try_from(value),
    ) {
        headers.insert(name, value);
    }
}

/// A handler panic becomes the plain-text 500 of the previous server.
fn panic_response(error: Box<dyn std::any::Any + Send>) -> Response {
    let message = error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            error
                .downcast_ref::<&str>()
                .map(|value| (*value).to_string())
        })
        .unwrap_or_else(|| "panic".to_string());
    tracing::error!(error = %message, "panic recovered");
    state::text_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error.")
}
