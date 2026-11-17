//! Request-scoped helpers: client address, form and query reading, session
//! extraction, origin and CSRF checks, HTMX detection and route labels.

#![allow(clippy::unused_async_trait_impl)] // axum's extractor trait is async; these are synchronous by design.
#![allow(clippy::result_large_err)] // axum handlers use `Response` as the error type.
use std::collections::BTreeMap;
use std::fmt::Write;

use axum::extract::{FromRequestParts, Request};
use axum::http::header::{self, HeaderMap};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use http_body_util::BodyExt;
use shorten::auth::{Claims, CookieSpec, SameSite, COOKIE_NAME};

use crate::state::{self, App};

/// Field name of the session cookie.
pub const SESSION_COOKIE: &str = COOKIE_NAME;

/// Request peer address, used as the rate-limit key.
pub struct ClientIp(pub String);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // The previous implementation used the transport peer address only and
        // never a forwarded header, so the same behaviour is kept here.
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map_or_else(|| "unknown".to_string(), |info| info.0.ip().to_string());
        Ok(Self(peer))
    }
}

/// Validated session claims, extracted from the session cookie.
///
/// A missing token redirects to the sign-in page. A malformed, forged or
/// expired token additionally clears the cookie first, exactly like the
/// previous authentication middleware.
pub struct Session(pub Claims);

impl Session {
    /// The CSRF token bound to this session.
    #[must_use]
    pub fn csrf(&self) -> &str {
        &self.0.csrf
    }
}

impl FromRequestParts<App> for Session {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &App) -> Result<Self, Self::Rejection> {
        let Some(raw) = read_cookie(&parts.headers, SESSION_COOKIE) else {
            return Err(login_redirect(state, &parts.headers, false));
        };
        match state.sessions().parse(&raw, Utc::now()) {
            Ok(claims) => Ok(Self(claims)),
            Err(error) => {
                tracing::debug!(error = %error, "session token rejected");
                Err(login_redirect(state, &parts.headers, true))
            }
        }
    }
}

fn login_redirect(state: &App, headers: &HeaderMap, clear_cookie: bool) -> Response {
    let cookie = clear_cookie.then(|| build_cookie(&state.sessions().clear_cookie_attributes()));
    redirect_with_cookie(
        StatusCode::SEE_OTHER,
        "/manage/login",
        headers,
        cookie.as_deref(),
    )
}

/// Build a redirect response the way Go's `http.Redirect` did: the target is
/// resolved against the request scheme and `Host` header, so the `Location`
/// Go handed the client was absolute, and the body is the one-line HTML link
/// the standard library wrote. A `HEAD` request passes `include_body` false,
/// which is what the Go server put on the wire for it.
#[must_use]
pub fn go_redirect(
    status: StatusCode,
    target: &str,
    headers: &HeaderMap,
    include_body: bool,
) -> Response {
    let location = go_location(target, headers);
    let body = if include_body {
        go_redirect_body(&location, status)
    } else {
        String::new()
    };
    let builder = axum::response::Response::builder()
        .status(status)
        .header(header::LOCATION, location.as_str())
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8");
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "redirect response rejected");
            state::text_error(StatusCode::INTERNAL_SERVER_ERROR, "Unable to redirect.")
        })
}

/// Build a redirect response, optionally carrying a `Set-Cookie` header, as
/// the previous middleware did when it cleared a rejected session.
#[must_use]
pub fn redirect_with_cookie(
    status: StatusCode,
    target: &str,
    headers: &HeaderMap,
    cookie: Option<&str>,
) -> Response {
    let location = go_location(target, headers);
    let body = go_redirect_body(&location, status);
    let mut builder = axum::response::Response::builder()
        .status(status)
        .header(header::LOCATION, location.as_str())
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8");
    if let Some(value) = cookie {
        builder = builder.header(header::SET_COOKIE, value);
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "redirect response rejected");
            state::text_error(StatusCode::INTERNAL_SERVER_ERROR, "Unable to redirect.")
        })
}

/// The `Location` Go wrote: an absolute URL, built from `X-Forwarded-Proto`
/// (the scheme Go derived from TLS when the header was absent) and the request
/// `Host`. The targets this service redirects to are paths, so the host part
/// is all that is missing.
fn go_location(target: &str, headers: &HeaderMap) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    format!("{scheme}://{host}{target}")
}

/// The body Go's `http.Redirect` wrote: `<a href="…">Status text</a>` and a
/// newline, with the target escaped by the standard library's HTML escaper.
fn go_redirect_body(location: &str, status: StatusCode) -> String {
    let text = match status {
        StatusCode::MOVED_PERMANENTLY => "Moved Permanently",
        StatusCode::SEE_OTHER => "See Other",
        StatusCode::TEMPORARY_REDIRECT => "Temporary Redirect",
        StatusCode::PERMANENT_REDIRECT => "Permanent Redirect",
        // Go's `http.StatusText` for the status it wrote into the body; a
        // `302 Found` is the value it used for the codes not listed above.
        _ => "Found",
    };
    let escaped = location
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    format!("<a href=\"{escaped}\">{text}</a>\n")
}

/// Form and query values, resolved the way the previous request helper did:
/// a query value wins over a body value for the same field.
#[derive(Clone, Default)]
pub struct Values {
    query: BTreeMap<String, Vec<String>>,
    body: BTreeMap<String, Vec<String>>,
}

impl Values {
    /// Read the query string only, for `GET` handlers.
    #[must_use]
    pub fn from_query(uri: &Uri) -> Self {
        Self {
            query: parse_pairs(uri.query().unwrap_or("")),
            body: BTreeMap::new(),
        }
    }

    /// Layer a `application/x-www-form-urlencoded` body under the query
    /// values, matching the previous `ParseForm` precedence.
    #[must_use]
    pub fn with_body(mut self, body: &str) -> Self {
        self.body = parse_pairs(body);
        self
    }

    /// First value of a field, query first, exactly like `FormValue`.
    #[must_use]
    pub fn get(&self, key: &str) -> &str {
        self.query
            .get(key)
            .and_then(|values| values.first())
            .map(String::as_str)
            .or_else(|| {
                self.body
                    .get(key)
                    .and_then(|values| values.first())
                    .map(String::as_str)
            })
            .unwrap_or("")
    }
}

/// Percent-decode `application/x-www-form-urlencoded` pairs, keeping repeated
/// keys in order and mapping `+` to a space.
#[must_use]
pub fn parse_pairs(input: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        out.entry(decode_query(raw_key))
            .or_default()
            .push(decode_query(raw_value));
    }
    out
}

fn decode_query(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'+' {
            out.push(b' ');
            index += 1;
        } else if byte == b'%'
            && index + 2 < bytes.len()
            && is_hex(bytes[index + 1])
            && is_hex(bytes[index + 2])
        {
            out.push((hex_value(bytes[index + 1]) << 4) + hex_value(bytes[index + 2]));
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => byte - b'A' + 10,
    }
}

/// Encode a value the way `url.QueryEscape` does, used for pager links:
/// percent-encoding with spaces written as `+`.
#[must_use]
pub fn query_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'@' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                let _ = write!(out, "{byte:02X}");
            }
        }
    }
    out
}

/// Read the request body with a hard byte cap, matching the previous
/// `MaxBytesReader` limit.
///
/// # Errors
///
/// Returns a plain-text `413` when the declared or actual body exceeds `limit`.
pub async fn read_body(request: Request, limit: usize) -> Result<Vec<u8>, Response> {
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|size| size > limit) {
        return Err(crate::state::text_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body too large.",
        ));
    }
    let (_parts, body) = request.into_parts();
    match body.collect().await {
        Ok(collection) => {
            let bytes = collection.to_bytes();
            if bytes.len() > limit {
                return Err(crate::state::text_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request body too large.",
                ));
            }
            Ok(bytes.to_vec())
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to read request body");
            Err(crate::state::text_error(
                StatusCode::BAD_REQUEST,
                "Unable to read the request body.",
            ))
        }
    }
}

/// The value of one cookie in the request.
#[must_use]
pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for raw in headers.get_all(header::COOKIE) {
        let Ok(text) = raw.to_str() else { continue };
        for entry in text.split(';') {
            if let Some((key, value)) = entry.trim().split_once('=') {
                if key.trim() == name {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

/// Serialise one `Set-Cookie` header from a session cookie specification.
#[must_use]
pub fn build_cookie(spec: &CookieSpec) -> String {
    // The attribute order follows the previous cookie writer: name and value,
    // path, expiry, max-age, then the security flags.
    let mut out = format!("{}={}", spec.name, spec.value);
    if let Some(path) = &spec.path {
        let _ = write!(out, "; Path={path}");
    }
    if let Some(expires) = spec.expires {
        let _ = write!(
            out,
            "; Expires={}",
            expires.format("%a, %d %b %Y %H:%M:%S GMT")
        );
    }
    if let Some(max_age) = spec.max_age {
        let _ = write!(out, "; Max-Age={max_age}");
    }
    if spec.secure {
        out.push_str("; Secure");
    }
    if spec.http_only {
        out.push_str("; HttpOnly");
    }
    let same_site = match spec.same_site {
        SameSite::Strict => "Strict",
        SameSite::Lax => "Lax",
        SameSite::None => "None",
    };
    out.push_str("; SameSite=");
    out.push_str(same_site);
    out
}

/// Is this request from HTMX? Fragment handlers answer with a partial page.
#[must_use]
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .and_then(|value| value.to_str().ok())
        == Some("true")
}

/// The previous same-origin gate: an absent `Origin` is allowed, the opaque
/// `null` origin is allowed when cookies are not marked secure or when the
/// operator opted in, and any other origin must equal the public base URL.
#[must_use]
pub fn same_origin(headers: &HeaderMap, app: &App) -> bool {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if origin.is_empty() {
        return true;
    }
    let config = app.config();
    if origin == "null" && (!config.secure_cookies || config.allow_null_origin) {
        return true;
    }
    origin.trim_end_matches('/') == config.public_base_url
}

/// Does the submitted CSRF field match the session token?
#[must_use]
pub fn valid_csrf(claims: Option<&Claims>, submitted: &str) -> bool {
    !submitted.is_empty() && submitted == claims.map_or("", |claims| claims.csrf.as_str())
}

/// The metric label for a path: dynamic segments collapse to their route
/// template so label cardinality stays bounded.
#[must_use]
pub fn normalized_route(path: &str) -> String {
    if path.starts_with("/api/links/") {
        return "/api/links/{slug}".to_string();
    }
    if path.starts_with("/manage/links/") {
        return "/manage/links/{slug}".to_string();
    }
    if path == "/" || !path.trim_matches('/').contains('/') {
        return "/{slug}".to_string();
    }
    path.to_string()
}

/// Are these the only two methods the redirect path answers?
#[must_use]
pub fn is_get_or_head(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD
}

/// Build the post-mutation redirect: HTMX gets `204` plus `HX-Redirect`,
/// browsers get `303`.
#[must_use]
pub fn after_mutation(headers: &HeaderMap, destination: &str) -> Response {
    if is_htmx(headers) {
        let mut response = StatusCode::NO_CONTENT.into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(destination) {
            response.headers_mut().insert("HX-Redirect", value);
        }
        return response;
    }
    go_redirect(StatusCode::SEE_OTHER, destination, headers, true)
}
