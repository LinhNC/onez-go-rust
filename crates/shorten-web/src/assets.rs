//! Embedded static assets: the design-system stylesheet, the small
//! application script, HTMX and the icons.
//!
//! Everything is compiled into the binary, so a deployment is a single file
//! with no external CDN and no filesystem dependency.

#![allow(clippy::missing_panics_doc)] // paths here are bracketed by a validated prefix.
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Files compiled into the binary from `assets/`.
#[derive(RustEmbed)]
#[folder = "assets/"]
pub struct Assets;

/// Serve one embedded asset by request path, `/assets/<file>`.
///
/// Answers `HEAD` with the same headers, `If-None-Match` with `304`, and
/// every other request with the immutable cache marker the previous file
/// server used.
#[must_use]
pub fn serve(path: &str, method: &Method, headers: &HeaderMap) -> Response {
    let Some(name) = asset_name(path) else {
        return crate::state::not_found();
    };
    let Some(asset) = Assets::get(name) else {
        return crate::state::not_found();
    };
    let data = asset.data.into_owned();
    let etag = etag_of(&data);
    if if_none_match_matches(headers, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [(
                header::ETAG,
                HeaderValue::from_str(&etag).expect("hex etag is a valid header"),
            )],
        )
            .into_response();
    }
    let builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type(name)).expect("media type is a valid header"),
        )
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        )
        .header(
            header::ETAG,
            HeaderValue::from_str(&etag).expect("hex etag is a valid header"),
        );
    if *method == Method::HEAD {
        return builder
            .header(header::CONTENT_LENGTH, HeaderValue::from(data.len()))
            .body(axum::body::Body::empty())
            .expect("static asset response is valid");
    }
    builder
        .body(axum::body::Body::from(data))
        .expect("static asset response is valid")
}

/// The `/assets/styles.css` entry point: the generated design-system
/// stylesheet, served with the same immutable cache marker.
#[must_use]
pub fn styles() -> Response {
    serve("/assets/styles.css", &Method::GET, &HeaderMap::new())
}

fn asset_name(path: &str) -> Option<&str> {
    let rest = path.trim_start_matches("/assets/").trim_start_matches('/');
    let name = rest.split(['?', '#']).next().unwrap_or(rest);
    if name.is_empty() || name.contains("..") || name.starts_with('/') || name.contains('/') {
        return None;
    }
    Some(name)
}

fn etag_of(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::from("W/\"");
    for byte in digest.iter().take(8) {
        let _ = write!(out, "{byte:02x}");
    }
    out.push('"');
    out
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(sent) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    // Weak and strong validators compare on the opaque value only.
    let ours = etag.strip_prefix("W/").unwrap_or(etag);
    sent.split(',').any(|candidate| {
        let candidate = candidate.trim();
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate == "*" || candidate == ours
    })
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Is the request path under `/assets/`?
#[must_use]
pub fn is_asset_path(uri: &Uri) -> bool {
    uri.path().starts_with("/assets/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_are_safe() {
        assert_eq!(asset_name("/assets/app.css"), Some("app.css"));
        assert_eq!(asset_name("/assets/app.css?v=1"), Some("app.css"));
        assert_eq!(asset_name("/assets/../secrets"), None);
        assert_eq!(asset_name("/assets/"), None);
        assert_eq!(asset_name("/assets/a/b.css"), None);
    }

    #[test]
    fn embedded_files_are_served() {
        let response = serve("/assets/app.css", &Method::GET, &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert!(response.headers().get(header::ETAG).is_some());
    }

    #[test]
    fn etag_round_trip() {
        let first = serve("/assets/app.js", &Method::GET, &HeaderMap::new());
        let etag = first.headers().get(header::ETAG).unwrap().clone();
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag);
        let second = serve("/assets/app.js", &Method::GET, &headers);
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn head_requests_have_no_body() {
        let response = serve("/assets/htmx.min.js", &Method::HEAD, &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::CONTENT_LENGTH).is_some());
    }

    #[test]
    fn media_types_match_the_previous_server() {
        assert_eq!(content_type("app.css"), "text/css; charset=utf-8");
        assert_eq!(
            content_type("htmx.min.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("favicon.ico"), "image/x-icon");
    }
}
