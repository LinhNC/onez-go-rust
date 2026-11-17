//! Shared fixtures for the `shorten-web` HTTP parity tests.
//!
//! Every fixture owns a private SQLite file, redirect cache, analytics
//! collector and `App`, so each test is independent and parallel-safe. No test
//! binds a socket: requests are handed straight to the router built by
//! `shorten_web::build` as a tower service, with a `ConnectInfo` extension that
//! stands in for the transport peer address the client-IP extractor reads.

// Helpers are shared by several test targets; not every target uses every one.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use chrono::{DateTime, Utc};
use serde_json::Value;
use shorten::analytics::Collector;
use shorten::config::Config;
use shorten::link::{CreateInput, Link, Status};
use shorten::redirectcache::Cache;
use shorten::store::Store;
use shorten_web::context::query_escape;
use shorten_web::{build, App};
use tempfile::TempDir;
use tower::Service;

/// Administrator API key used by the fixtures; at least 16 characters, as the
/// configuration validation requires.
pub const API_KEY: &str = "parity-admin-api-key-0001";

/// Session secret used by the fixtures; at least 32 characters.
pub const SESSION_SECRET: &str = "parity-session-secret-0123456789-abcdefgh";

/// Previous session secret kept empty: no rotation is live in tests.
pub const PREVIOUS_SECRET: &str = "";

/// Public base URL of every fixture; feeds `shortUrl` and the origin gate.
pub const BASE_URL: &str = "http://manage.example";

/// The exact CSP string the middleware stamps on every response.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Session cookie name shared with the previous service.
pub const SESSION_COOKIE: &str = "shorten_session";

/// Peer address used when a test does not care about the client IP.
pub const PEER: &str = "203.0.113.7:51234";

/// The `Host` header the harness sends with every request. The previous
/// service resolved relative redirects against it, exactly as Go's
/// `http.Redirect` did, so the harness has to carry one.
pub const HOST: &str = "example.com";

/// Analytics flush interval used by the fixtures: short enough that a visit is
/// visible in the store after one `flush_pause()`.
pub const ANALYTICS_INTERVAL: Duration = Duration::from_millis(40);

/// Pause long enough for the fixture collector's interval tick to land.
pub async fn flush_pause() {
    tokio::time::sleep(Duration::from_millis(220)).await;
}

/// Pause long enough for created timestamps to differ by a millisecond.
pub async fn tick() {
    tokio::time::sleep(Duration::from_millis(6)).await;
}

/// A test configuration with an explicit database path.
pub fn test_config(database_path: &str) -> Config {
    let env: BTreeMap<String, String> = BTreeMap::from([
        ("SHORTEN_ADDRESS".to_string(), ":0".to_string()),
        (
            "SHORTEN_DATABASE_PATH".to_string(),
            database_path.to_string(),
        ),
        ("SHORTEN_PUBLIC_BASE_URL".to_string(), BASE_URL.to_string()),
        ("SHORTEN_ADMIN_API_KEY".to_string(), API_KEY.to_string()),
        (
            "SHORTEN_SESSION_SECRET".to_string(),
            SESSION_SECRET.to_string(),
        ),
        (
            "SHORTEN_PREVIOUS_SESSION_SECRET".to_string(),
            PREVIOUS_SECRET.to_string(),
        ),
        ("SHORTEN_CACHE_CAPACITY".to_string(), "1000".to_string()),
        // Fixtures run over plain http, so the session cookie carries no
        // `Secure` flag, exactly as the previous local deployment ran with
        // `SHORTEN_INSECURE_COOKIES=true`.
        ("SHORTEN_INSECURE_COOKIES".to_string(), "true".to_string()),
        ("SHORTEN_ANALYTICS_INTERVAL".to_string(), "40ms".to_string()),
        ("SHORTEN_ANALYTICS_THRESHOLD".to_string(), "1".to_string()),
        ("SHORTEN_ANALYTICS_MAX_KEYS".to_string(), "1000".to_string()),
    ]);
    Config::load_from(move |key: &str| env.get(key).cloned()).expect("valid test configuration")
}

/// A built application plus the store and files behind it.
pub struct Fixture {
    app: App,
    store: Arc<Store>,
    database: PathBuf,
    directory: TempDir,
    collector: RefCell<Option<JoinHandle<()>>>,
}

impl Fixture {
    /// Open a private database and build the router state over it.
    #[must_use]
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("shorten.db");
        let path = database.to_string_lossy().to_string();
        let config = test_config(&path);
        let store = Arc::new(Store::open(&path, 2).expect("open test store"));
        let cache = Cache::new(
            config.cache_capacity,
            config.positive_cache_ttl,
            config.negative_cache_ttl,
        );
        let analytics = Collector::new(
            Arc::clone(&store) as Arc<dyn shorten::analytics::Flusher>,
            ANALYTICS_INTERVAL,
            1,
            1000,
        );
        let collector = analytics.start();
        let app = App::new(config, Arc::clone(&store), cache, analytics);
        Self {
            app,
            store,
            database,
            directory,
            collector: RefCell::new(Some(collector)),
        }
    }

    /// The router under test.
    pub fn router(&self) -> Router {
        build(self.app.clone())
    }

    /// Drive one request through the router and read the whole response.
    #[must_use]
    pub async fn request(&self, request: Request<Body>) -> Reply {
        let mut router = self.router();
        // A oneshot call of the router as a tower service: `call` consumes one
        // request and answers it, with no connection involved.
        let response = router
            .call(request)
            .await
            .expect("router answers the request");
        Reply::from_response(response).await
    }

    /// `GET` one path.
    #[must_use]
    pub async fn get(&self, uri: &str) -> Reply {
        self.request(Req::get(uri).build()).await
    }

    /// `POST` a urlencoded form, authenticated by the API key header.
    #[must_use]
    pub async fn post_json(&self, uri: &str, body: &str) -> Reply {
        self.request(Req::post(uri).json(body).api_key().build())
            .await
    }

    /// The application state.
    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    /// The shared store handle.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The database file behind this fixture; also keeps the temp directory in
    /// use by the compiler's eyes.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// The temporary directory holding the database.
    #[must_use]
    pub fn directory(&self) -> &TempDir {
        &self.directory
    }

    /// Write one link straight through the store.
    #[must_use]
    pub fn create(&self, input: CreateInput) -> Link {
        self.store.create(input).expect("create link")
    }

    /// Write one link with an explicit, caller-supplied record: the import path
    /// keeps the timestamps it is given, which the create path does not.
    pub fn import(&self, link: &Link) {
        self.store
            .import_batch(std::slice::from_ref(link), "abort")
            .expect("import link");
    }

    /// Read one link back out of the store.
    #[must_use]
    pub fn link(&self, slug: &str) -> Link {
        self.store.get_by_slug(slug).expect("read link")
    }

    /// Stored visit counter for one slug.
    #[must_use]
    pub fn visits(&self, slug: &str) -> i64 {
        self.link(slug).visit_count
    }

    /// Stop the analytics loop after flushing what is buffered, and join the
    /// flush thread so the fixture leaves no worker behind.
    pub fn stop_analytics(&self) {
        let _ = self.app.analytics().flush();
        self.app.analytics().stop();
        if let Some(handle) = self.collector.take() {
            let _ = handle.join();
        }
    }

    /// End the test: flush and stop analytics, close the store pools, release
    /// the router state, and remove the temporary directory for good.
    pub fn teardown(self) {
        self.stop_analytics();
        let Self {
            app,
            store,
            database,
            directory,
            collector,
        } = self;
        drop(collector);
        drop(app);
        let closed = store.close();
        drop(store);
        drop(database);
        let path = directory.path().to_path_buf();
        drop(directory);
        let removed = std::fs::remove_dir_all(&path);
        assert!(closed.is_ok(), "store pools close cleanly: {closed:?}");
        assert!(
            removed.is_ok() || !path.exists(),
            "the temporary directory is gone: {}",
            path.display()
        );
    }

    /// The rendered `/metrics` body.
    pub async fn metrics_body(&self) -> String {
        self.get("/metrics").await.text()
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Create input for a plain active link with a fixed slug.
#[must_use]
pub fn input(slug: &str, destination: &str) -> CreateInput {
    CreateInput {
        slug: slug.to_string(),
        destination_url: destination.to_string(),
        title: String::new(),
        notes: String::new(),
        status: Some(Status::Active),
        redirect_status: 302,
        expires_at: None,
    }
}

/// Create input with an explicit redirect status.
#[must_use]
pub fn input_status(slug: &str, destination: &str, redirect_status: i32) -> CreateInput {
    CreateInput {
        redirect_status,
        ..input(slug, destination)
    }
}

/// Create input for a disabled link.
#[must_use]
pub fn input_disabled(slug: &str, destination: &str) -> CreateInput {
    CreateInput {
        status: Some(Status::Disabled),
        ..input(slug, destination)
    }
}

/// Create input for a link that expired at `expires_at`.
#[must_use]
pub fn input_expired(slug: &str, destination: &str, expires_at: DateTime<Utc>) -> CreateInput {
    CreateInput {
        expires_at: Some(expires_at),
        ..input(slug, destination)
    }
}

/// A full stored record with an explicit `created_at`, for cleanup scenarios.
#[must_use]
pub fn aged_record(slug: &str, destination: &str, created_at: DateTime<Utc>) -> Link {
    Link {
        id: 0,
        slug: slug.to_string(),
        destination_url: destination.to_string(),
        title: String::new(),
        notes: String::new(),
        status: Status::Disabled,
        redirect_status: 302,
        expires_at: None,
        visit_count: 0,
        last_visited_at: None,
        created_at,
        updated_at: created_at,
    }
}

/// A request under construction.
#[derive(Debug)]
pub struct Req {
    method: Method,
    uri: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Body,
    peer: SocketAddr,
}

impl Req {
    /// A request with the given method and target.
    #[must_use]
    pub fn of(method: &str, uri: &str) -> Self {
        Self {
            method: Method::try_from(method).expect("test method is valid"),
            uri: uri.to_string(),
            headers: vec![(axum::http::header::HOST, HeaderValue::from_static(HOST))],
            body: Body::empty(),
            peer: PEER.parse().expect("test peer address"),
        }
    }

    /// `GET`.
    #[must_use]
    pub fn get(uri: &str) -> Self {
        Self::of("GET", uri)
    }

    /// `HEAD`.
    #[must_use]
    pub fn head(uri: &str) -> Self {
        Self::of("HEAD", uri)
    }

    /// `POST`.
    #[must_use]
    pub fn post(uri: &str) -> Self {
        Self::of("POST", uri)
    }

    /// `PATCH`.
    #[must_use]
    pub fn patch(uri: &str) -> Self {
        Self::of("PATCH", uri)
    }

    /// `DELETE`.
    #[must_use]
    pub fn delete(uri: &str) -> Self {
        Self::of("DELETE", uri)
    }

    /// Add one header.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((
            HeaderName::try_from(name).expect("test header name"),
            HeaderValue::try_from(value).expect("test header value"),
        ));
        self
    }

    /// Add a raw body with no content type.
    #[must_use]
    pub fn raw(mut self, body: &str) -> Self {
        self.body = Body::from(body.to_string());
        self
    }

    /// Add a JSON body with the `application/json` content type.
    #[must_use]
    pub fn json(self, body: &str) -> Self {
        self.header("content-type", "application/json").raw(body)
    }

    /// Add a JSON body with an explicit content type.
    #[must_use]
    pub fn json_with_type(self, body: &str, content_type: &str) -> Self {
        self.header("content-type", content_type).raw(body)
    }

    /// Add an `application/x-www-form-urlencoded` body.
    #[must_use]
    pub fn form(self, pairs: &[(&str, &str)]) -> Self {
        let body = pairs
            .iter()
            .map(|(key, value)| format!("{}={}", query_escape(key), query_escape(value)))
            .collect::<Vec<String>>()
            .join("&");
        self.header("content-type", "application/x-www-form-urlencoded")
            .raw(&body)
    }

    /// Add the administrator API key header.
    #[must_use]
    pub fn api_key(self) -> Self {
        self.header("x-api-key", API_KEY)
    }

    /// Add the HTMX request marker.
    #[must_use]
    pub fn htmx(self) -> Self {
        self.header("hx-request", "true")
    }

    /// Add one session cookie.
    #[must_use]
    pub fn cookie(self, value: &str) -> Self {
        self.header("cookie", &format!("{SESSION_COOKIE}={value}"))
    }

    /// Add a cookie with an arbitrary name and value.
    #[must_use]
    pub fn cookie_pair(self, name: &str, value: &str) -> Self {
        self.header("cookie", &format!("{name}={value}"))
    }

    /// Send the request from a different client address.
    #[must_use]
    pub fn peer(mut self, peer: &str) -> Self {
        self.peer = peer.parse().expect("test peer address");
        self
    }

    /// Finish the request, attaching the peer address the way a real listener
    /// would through `ConnectInfo`.
    #[must_use]
    pub fn build(self) -> Request<Body> {
        let mut builder = Request::builder().method(self.method).uri(self.uri);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder
            .extension(ConnectInfo(self.peer))
            .body(self.body)
            .expect("test request is valid")
    }
}

/// A fully read response.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    /// Read a response body to the end.
    #[must_use]
    pub async fn from_response(mut response: Response<Body>) -> Self {
        let body = response.body_mut();
        let bytes = to_bytes(std::mem::take(body), usize::MAX)
            .await
            .expect("response body is readable");
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            body: bytes.to_vec(),
        }
    }

    /// One header value.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = HeaderName::try_from(name).expect("test header name");
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Every `Set-Cookie` header.
    #[must_use]
    pub fn cookies(&self) -> Vec<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect()
    }

    /// The body decoded as UTF-8 text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body parsed as JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("response body is JSON")
    }

    /// The exact JSON wire body, including the trailing newline.
    #[must_use]
    pub fn json_line(&self) -> String {
        let text = self.text();
        assert!(
            text.ends_with('\n'),
            "JSON bodies end with a trailing newline, got {text:?}"
        );
        text
    }

    /// The status code as a number.
    #[must_use]
    pub fn code(&self) -> u16 {
        self.status.as_u16()
    }

    /// The content type.
    #[must_use]
    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }

    /// The `Location` header.
    #[must_use]
    pub fn location(&self) -> &str {
        self.header("location").unwrap_or("")
    }

    /// The `X-Request-ID` header.
    #[must_use]
    pub fn request_id(&self) -> &str {
        self.header("x-request-id").unwrap_or("")
    }

    /// Assert the middleware stamped every security header.
    pub fn assert_security_headers(&self) {
        assert_eq!(
            self.header("x-content-type-options"),
            Some("nosniff"),
            "X-Content-Type-Options: {self:?}"
        );
        assert_eq!(
            self.header("referrer-policy"),
            Some("no-referrer"),
            "Referrer-Policy: {self:?}"
        );
        assert_eq!(
            self.header("permissions-policy"),
            Some("camera=(), microphone=(), geolocation=()"),
            "Permissions-Policy: {self:?}"
        );
        assert_eq!(
            self.header("content-security-policy"),
            Some(CSP),
            "Content-Security-Policy: {self:?}"
        );
        assert!(
            !self.request_id().is_empty(),
            "X-Request-ID is present: {self:?}"
        );
    }

    /// Assert the middleware stamped the headers *and* the identifier matches
    /// the expected base-36 counter value.
    pub fn assert_security_headers_with_id(&self, id: &str) {
        self.assert_security_headers();
        assert_eq!(self.request_id(), id, "X-Request-ID counter: {self:?}");
    }

    /// Assert the response is not cacheable: the previous service set
    /// `Cache-Control` on embedded assets only, never on redirects, API
    /// responses, plain-text errors, or console pages.
    pub fn assert_no_cache_control(&self) {
        assert!(
            self.header("cache-control").is_none(),
            "no Cache-Control on this response: {self:?}"
        );
    }

    /// The response body as a `&str`, asserted to be valid UTF-8.
    #[must_use]
    pub fn utf8(&self) -> &str {
        std::str::from_utf8(&self.body).expect("response body is UTF-8")
    }

    /// The `shorten_session` value from the `Set-Cookie` header, if any.
    #[must_use]
    pub fn session_cookie_value(&self) -> Option<String> {
        self.cookies().iter().find_map(|cookie| {
            let pair = cookie.split(';').next()?;
            let (name, value) = pair.split_once('=')?;
            (name.trim() == SESSION_COOKIE).then(|| value.to_string())
        })
    }
}

/// The attributes of one `Set-Cookie` header, lowercased for tolerant matching.
#[must_use]
pub fn cookie_attributes(cookie: &str) -> Vec<String> {
    cookie
        .split(';')
        .skip(1)
        .map(|attribute| attribute.trim().to_string())
        .collect()
}

/// Index of `needle` in `haystack`, used to pin the attribute order of a cookie.
#[must_use]
pub fn attribute_order(cookie: &str, needles: &[&str]) -> Vec<usize> {
    needles
        .iter()
        .map(|needle| {
            cookie
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} missing from {cookie}"))
        })
        .collect()
}

/// Sign in through the console form and return the session cookie plus the
/// CSRF token of the create form.
#[must_use]
pub async fn login(fixture: &Fixture) -> (String, String) {
    let reply = fixture
        .request(
            Req::post("/manage/login")
                .form(&[("api_key", API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::SEE_OTHER,
        "login redirect: {reply:?}"
    );
    let token = reply
        .session_cookie_value()
        .expect("login sets the session cookie");
    let csrf = fixture_csrf(fixture, &token).await;
    (token, csrf)
}

/// Read the CSRF token out of one authenticated page.
#[must_use]
pub async fn fixture_csrf(fixture: &Fixture, token: &str) -> String {
    let reply = fixture
        .request(Req::get("/manage/links/new").cookie(token).build())
        .await;
    assert_eq!(reply.status, StatusCode::OK, "create page: {reply:?}");
    parse_csrf(&reply.text()).expect("create form carries a csrf field")
}

/// Pull the `name="csrf" value="…"` field out of rendered HTML.
#[must_use]
pub fn parse_csrf(html: &str) -> Option<String> {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The public base URL a fixture advertises.
#[must_use]
pub fn base_url_of(fixture: &Fixture) -> String {
    fixture.app().config().public_base_url.clone()
}

/// The base-36 spelling of a request sequence number.
#[must_use]
pub fn base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[usize::try_from(value % 36).expect("base 36 digit")]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base 36 digits are ASCII")
}

/// One instant from a Unix-millisecond value, the store's own encoding.
#[must_use]
pub fn at_millis(millis: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(millis).expect("test instant is in range")
}

/// The wire spelling of one timestamp, as the API prints it.
#[must_use]
pub fn go_time(value: DateTime<Utc>) -> String {
    shorten::json::format_go_time(value)
}

/// `POST` one authenticated console form: the session cookie plus a `csrf`
/// field carrying the session token.
#[must_use]
pub async fn mutation(
    fixture: &Fixture,
    uri: &str,
    session: &str,
    csrf: &str,
    pairs: &[(&str, &str)],
) -> Reply {
    let mut form: Vec<(&str, &str)> = pairs.to_vec();
    form.push(("csrf", csrf));
    fixture
        .request(Req::post(uri).cookie(session).form(&form).build())
        .await
}

/// The same mutation flagged as an HTMX request.
#[must_use]
pub async fn htmx_mutation(
    fixture: &Fixture,
    uri: &str,
    session: &str,
    csrf: &str,
    pairs: &[(&str, &str)],
) -> Reply {
    let mut form: Vec<(&str, &str)> = pairs.to_vec();
    form.push(("csrf", csrf));
    fixture
        .request(Req::post(uri).cookie(session).htmx().form(&form).build())
        .await
}

/// Assert every listed metric line is present in the exposition body.
pub fn assert_metric_lines(body: &str, lines: &[&str]) {
    for line in lines {
        assert!(body.contains(line), "metrics body lacks {line:?}:\n{body}");
    }
}

/// One named attribute of a `Set-Cookie` header, `Max-Age` and friends.
#[must_use]
pub fn cookie_attribute(cookie: &str, name: &str) -> Option<String> {
    cookie.split(';').skip(1).find_map(|attribute| {
        let attribute = attribute.trim();
        let (key, value) = attribute.split_once('=')?;
        (key.eq_ignore_ascii_case(name)).then(|| value.trim().to_string())
    })
}

/// Is one flag-style attribute (`HttpOnly`, `Secure`) present?
#[must_use]
pub fn cookie_flag(cookie: &str, name: &str) -> bool {
    cookie
        .split(';')
        .skip(1)
        .any(|attribute| attribute.trim().eq_ignore_ascii_case(name))
}

/// The attribute names of a `Set-Cookie` header, in wire order.
#[must_use]
pub fn cookie_attribute_names(cookie: &str) -> Vec<String> {
    cookie
        .split(';')
        .skip(1)
        .map(|attribute| {
            attribute
                .trim()
                .split('=')
                .next()
                .expect("attribute has a name")
                .trim()
                .to_string()
        })
        .collect()
}

/// Decode one base64url (unpadded) string, the encoding of a list cursor.
///
/// # Errors
///
/// Returns the offending byte or a trailing-bit description.
pub fn base64url_decode(input: &str) -> Result<Vec<u8>, String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lookup = [0xFF_u8; 256];
    for (index, symbol) in ALPHABET.iter().enumerate() {
        lookup[usize::from(*symbol)] = u8::try_from(index).expect("base64 index fits a byte");
    }
    let mut bits: u32 = 0;
    let mut pending: u8 = 0;
    let mut out: Vec<u8> = Vec::new();
    for byte in input.as_bytes() {
        let value = lookup[usize::from(*byte)];
        if value == 0xFF {
            return Err(format!("not a base64url byte: {byte:#04x}"));
        }
        bits = (bits << 6) | u32::from(value);
        pending += 6;
        if pending >= 8 {
            pending -= 8;
            out.push(((bits >> u32::from(pending)) & 0xFF) as u8);
            bits &= (1 << u32::from(pending)) - 1;
        }
    }
    if pending >= 4 {
        return Err("trailing base64url bits".to_string());
    }
    Ok(out)
}

/// The keys of the outermost JSON object, in wire order.
///
/// `serde_json::Map` sorts keys, so the order a body was written in is only
/// visible in the raw text: this walks the text and keeps every string that is
/// immediately followed by a `:` at the top level.
#[must_use]
pub fn json_top_level_keys(text: &str) -> Vec<String> {
    json_keys_at(text, 1)
}

/// The keys of every JSON object at `target` nesting depth, in wire order.
/// Depth 1 is the outermost object, depth 2 its direct children.
///
/// # Panics
///
/// When the body is not a JSON object.
#[must_use]
pub fn json_keys_at(text: &str, target: u32) -> Vec<String> {
    let chars: Vec<char> = text.trim().chars().collect();
    assert_eq!(
        chars.first().copied(),
        Some('{'),
        "the body is a JSON object: {text}"
    );
    let mut keys: Vec<String> = Vec::new();
    let mut depth: u32 = 0;
    let mut index: usize = 0;
    while index < chars.len() {
        match chars[index] {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            '"' => {
                let start = index + 1;
                let mut end = start;
                while end < chars.len() && chars[end] != '"' {
                    if chars[end] == '\\' {
                        end += 1;
                    }
                    end += 1;
                }
                let value: String = chars[start..end].iter().collect();
                // A key is a string followed by a colon, at the target depth.
                let mut after = end + 1;
                while after < chars.len() && chars[after].is_whitespace() {
                    after += 1;
                }
                if depth == target && chars.get(after).copied() == Some(':') {
                    keys.push(value);
                }
                index = end;
            }
            _ => {}
        }
        index += 1;
    }
    keys
}

/// The exact JSON envelope of an API error, with its trailing newline.
#[must_use]
pub fn error_line(message: &str) -> String {
    format!("{{\"error\":\"{message}\"}}\n")
}
