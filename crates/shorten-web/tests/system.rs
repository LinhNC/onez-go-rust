//! Parity of the system routes: health, readiness, metrics, embedded assets,
//! the root redirect, the plain-text fallback, and the request middleware.
//!
//! Reference: Go `internal/web/server.go` (middleware, `healthz`, `readyz`,
//! `metrics`, `cacheAssets`, the `GET /{$}` redirect) and
//! `internal/observability` (metrics text exposition).

mod common;

use axum::http::StatusCode;
use common::{base36, Fixture, Req};

/// `GET /healthz` is the fixed JSON line of the previous service.
#[tokio::test]
async fn healthz_reports_ok() {
    let fixture = Fixture::new();
    let reply = fixture.get("/healthz").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.content_type(),
        "application/json; charset=utf-8",
        "writeJSON media type"
    );
    assert_eq!(reply.json_line(), "{\"status\":\"ok\"}\n");
    reply.assert_security_headers();
    fixture.teardown();
}

/// `GET /readyz` answers 200 while the flag, the store ping and the analytics
/// collector are healthy, and 503 once the shutdown sequence clears the flag.
#[tokio::test]
async fn readyz_tracks_the_ready_flag() {
    let fixture = Fixture::new();

    let ready = fixture.get("/readyz").await;
    assert_eq!(ready.status, StatusCode::OK);
    assert_eq!(
        ready.content_type(),
        "application/json; charset=utf-8",
        "writeJSON media type"
    );
    assert_eq!(ready.json_line(), "{\"status\":\"ready\"}\n");
    ready.assert_security_headers();

    fixture.app().set_ready(false);
    let drained = fixture.get("/readyz").await;
    assert_eq!(drained.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(drained.json_line(), "{\"status\":\"not_ready\"}\n");
    drained.assert_security_headers();

    fixture.app().set_ready(true);
    let restored = fixture.get("/readyz").await;
    assert_eq!(restored.status, StatusCode::OK);
    assert_eq!(restored.json_line(), "{\"status\":\"ready\"}\n");
    fixture.teardown();
}

/// PARITY GAP (reported to the lead): the readiness check folds in the
/// analytics collector's health window, which is five seconds wide
/// (`shorten/src/analytics.rs:52`), so an unhealthy collector cannot be
/// reached inside the test sleep budget. The flag path is covered by
/// `readyz_tracks_the_ready_flag`.
#[tokio::test]
async fn readyz_needs_a_healthy_analytics_window() {
    let fixture = Fixture::new();
    let ready = fixture.get("/readyz").await;
    assert_eq!(ready.status, StatusCode::OK, "healthy collector: {ready:?}");
    assert_eq!(ready.json_line(), "{\"status\":\"ready\"}\n");
    // Flushing keeps the collector healthy and leaves nothing pending.
    fixture.app().analytics().flush().expect("analytics flush");
    let after = fixture.get("/readyz").await;
    assert_eq!(after.status, StatusCode::OK, "after flush: {after:?}");
    fixture.app().analytics().stop();
}

/// `GET /metrics` is the Go text exposition: no `# HELP`/`# TYPE` comments, the
/// redirect counters, the duration sum and the six gauges, one value per line.
#[tokio::test]
async fn metrics_exposes_the_fixed_line_set() {
    let fixture = Fixture::new();
    let slug = "metricsprobe";
    let created = fixture.create(common::input(slug, "https://metrics.example/target"));
    assert_eq!(created.slug, slug, "the probe link is stored");

    // One miss, one hit: both cache counters and the redirect counters move.
    let first = fixture.get(&format!("/{slug}")).await;
    assert_eq!(first.status, StatusCode::FOUND, "first lookup: {first:?}");
    let second = fixture.get(&format!("/{slug}")).await;
    assert_eq!(
        second.status,
        StatusCode::FOUND,
        "second lookup: {second:?}"
    );
    fixture.app().analytics().flush().expect("analytics flush");

    let reply = fixture.get("/metrics").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.content_type(),
        "text/plain; version=0.0.4; charset=utf-8",
        "metrics media type"
    );
    let body = reply.text();
    assert!(
        body.ends_with('\n'),
        "every metric line is newline terminated: {body}"
    );
    assert!(
        !body.contains("# HELP") && !body.contains("# TYPE") && !body.contains('#'),
        "the Go handler emits no comment lines: {body}"
    );

    for line in [
        "shorten_redirect_requests_total 2\n",
        "shorten_redirects_total{outcome=\"success\"} 2\n",
        "shorten_redirects_total{outcome=\"missing\"} 0\n",
        "shorten_redirects_total{outcome=\"disabled\"} 0\n",
        "shorten_redirects_total{outcome=\"expired\"} 0\n",
        "shorten_redirects_total{outcome=\"method_not_allowed\"} 0\n",
        "shorten_redirects_total{outcome=\"error\"} 0\n",
        "shorten_redirect_cache_hits_total 1\n",
        "shorten_redirect_cache_misses_total 1\n",
        "shorten_redirect_cache_evictions_total 0\n",
        "shorten_redirect_cache_size 1\n",
        "shorten_analytics_pending_links 0\n",
        "shorten_analytics_dropped_total 0\n",
    ] {
        assert!(body.contains(line), "metrics body lacks {line:?}:\n{body}");
    }

    // The duration sum is the seconds form of the accumulated nanoseconds,
    // printed with Go's nine-place fixed layout.
    let duration = body
        .lines()
        .find(|line| line.starts_with("shorten_redirect_duration_seconds_sum "))
        .unwrap_or_else(|| panic!("duration line missing:\n{body}"));
    let value = duration.split_whitespace().nth(1).expect("duration value");
    assert!(
        value.parse::<f64>().is_ok() && value.contains('.'),
        "duration sum is a float: {duration}"
    );
    let decimals = value.split('.').nth(1).expect("fraction");
    assert_eq!(
        decimals.len(),
        9,
        "duration sum keeps nine decimals: {duration}"
    );
    assert!(
        !body.contains("shorten_http_requests_total{route=\"/{slug}\""),
        "the redirect route never enters the request map:\n{body}"
    );
    fixture.teardown();
}

/// Metrics counts console, API and asset traffic in the request map. The
/// single-segment routes (`/healthz`, `/readyz`, `/metrics`, `/{slug}`) all
/// normalize to `/{slug}` and go to the redirect counters, exactly as the
/// previous `normalizedRoute` helper routed them.
#[tokio::test]
async fn metrics_counts_console_and_api_routes() {
    let fixture = Fixture::new();
    let asset = fixture.get("/assets/app.css").await;
    assert_eq!(asset.status, StatusCode::OK, "asset request: {asset:?}");
    let console = fixture.get("/manage/links").await;
    assert_eq!(
        console.status,
        StatusCode::SEE_OTHER,
        "unauthenticated console request: {console:?}"
    );
    let health = fixture.get("/healthz").await;
    assert_eq!(health.status, StatusCode::OK, "health request: {health:?}");
    let denied = fixture
        .request(Req::get("/api/links").header("x-api-key", "wrong").build())
        .await;
    assert_eq!(
        denied.status,
        StatusCode::UNAUTHORIZED,
        "unauthenticated api request: {denied:?}"
    );

    let body = fixture.get("/metrics").await.text();
    for line in [
        "shorten_http_requests_total{route=\"/assets/app.css\",status=\"200\"} 1\n",
        "shorten_http_requests_total{route=\"/manage/links\",status=\"303\"} 1\n",
        "shorten_http_requests_total{route=\"/api/links\",status=\"401\"} 1\n",
    ] {
        assert!(body.contains(line), "metrics body lacks {line:?}:\n{body}");
    }
    assert!(
        !body.contains("route=\"/healthz\""),
        "single-segment routes are counted as the redirect route:\n{body}"
    );
    assert!(
        body.contains("shorten_redirect_requests_total 1\n"),
        "the /healthz observation is a redirect counter:\n{body}"
    );
    fixture.teardown();
}

/// Mutations reach the mutation counter keyed by operation.
#[tokio::test]
async fn metrics_counts_mutations() {
    let fixture = Fixture::new();
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://mutation.example/target\",\"slug\":\"mutate-me\"}",
        )
        .await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "api create: {created:?}"
    );
    let deleted = fixture
        .request(Req::delete("/api/links/mutate-me").api_key().build())
        .await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "api delete: {deleted:?}"
    );

    let body = fixture.get("/metrics").await.text();
    assert!(
        body.contains("shorten_link_mutations_total{operation=\"create\"} 1\n"),
        "create mutation line:\n{body}"
    );
    assert!(
        body.contains("shorten_link_mutations_total{operation=\"delete\"} 1\n"),
        "delete mutation line:\n{body}"
    );
    fixture.teardown();
}

/// Embedded assets carry the immutable cache marker, a weak `ETag`, and answer
/// `If-None-Match` with 304 and `HEAD` with the headers only.
#[tokio::test]
async fn assets_are_immutable_and_support_conditional_requests() {
    let fixture = Fixture::new();

    let first = fixture.get("/assets/app.css").await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.content_type(), "text/css; charset=utf-8");
    assert_eq!(
        first.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(!first.body.is_empty(), "stylesheet body is served");
    let etag = first
        .header("etag")
        .expect("asset carries an etag")
        .to_string();
    assert!(
        etag.starts_with("W/\"") && etag.ends_with('"'),
        "weak etag: {etag}"
    );
    first.assert_security_headers();

    let warm = fixture
        .request(
            Req::get("/assets/app.css")
                .header("if-none-match", &etag)
                .build(),
        )
        .await;
    assert_eq!(warm.status, StatusCode::NOT_MODIFIED);
    assert!(warm.body.is_empty(), "304 carries no body");
    assert_eq!(warm.header("etag"), Some(etag.as_str()));

    let strong = fixture
        .request(
            Req::get("/assets/app.css")
                .header("if-none-match", etag.trim_start_matches("W/"))
                .build(),
        )
        .await;
    assert_eq!(
        strong.status,
        StatusCode::NOT_MODIFIED,
        "strong validator matches"
    );

    let any = fixture
        .request(
            Req::get("/assets/app.css")
                .header("if-none-match", "*")
                .build(),
        )
        .await;
    assert_eq!(any.status, StatusCode::NOT_MODIFIED, "wildcard matches");

    let head = fixture.request(Req::head("/assets/app.css").build()).await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty(), "HEAD carries no body");
    assert_eq!(head.content_type(), "text/css; charset=utf-8");
    assert_eq!(
        head.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(
        head.header("content-length").is_some(),
        "HEAD advertises the length"
    );

    let script = fixture.get("/assets/app.js").await;
    assert_eq!(script.status, StatusCode::OK);
    assert_eq!(script.content_type(), "text/javascript; charset=utf-8");
    fixture.teardown();
}

/// Unknown asset names answer the plain-text 404.
#[tokio::test]
async fn unknown_assets_are_not_found() {
    let fixture = Fixture::new();

    let missing = fixture.get("/assets/nope.css").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.content_type(), "text/plain; charset=utf-8");
    assert_eq!(missing.text(), "404 page not found\n");
    missing.assert_security_headers();

    let nested = fixture.get("/assets/deep/app.css").await;
    assert_eq!(
        nested.status,
        StatusCode::NOT_FOUND,
        "nested names are rejected"
    );

    let empty = fixture.get("/assets/").await;
    assert_eq!(
        empty.status,
        StatusCode::NOT_FOUND,
        "empty asset name is rejected"
    );

    let traversal = fixture
        .request(Req::get("/assets/%2e%2e%2f%2e%2e/etc/passwd").build())
        .await;
    assert_eq!(
        traversal.status,
        StatusCode::NOT_FOUND,
        "traversal is rejected"
    );
    fixture.teardown();
}

/// The design-system stylesheet is served on the same asset route as the
/// console sheets: `GET /assets/styles.css` answers 200 with the immutable
/// cache marker, a weak `ETag`, `304` on `If-None-Match`, and headers only for
/// `HEAD`. Reference: Go `internal/web/server.go:64-68` served it from a
/// dedicated route; the Rust router folds it into `/assets/{*path}`.
#[tokio::test]
async fn styles_css_is_an_immutable_asset() {
    let fixture = Fixture::new();

    let first = fixture.get("/assets/styles.css").await;
    assert_eq!(first.status, StatusCode::OK, "styles.css: {first:?}");
    assert_eq!(first.content_type(), "text/css; charset=utf-8");
    assert_eq!(
        first.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(!first.body.is_empty(), "the stylesheet body is served");
    let etag = first
        .header("etag")
        .expect("styles.css carries an etag")
        .to_string();
    let payload = etag
        .strip_prefix("W/\"")
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("weak etag shape: {etag}"));
    assert_eq!(
        payload.len(),
        16,
        "the ETag payload is 16 hex characters: {etag}"
    );
    assert!(
        payload.chars().all(|ch| ch.is_ascii_hexdigit()),
        "the ETag payload is hexadecimal: {etag}"
    );
    first.assert_security_headers();

    let warm = fixture
        .request(
            Req::get("/assets/styles.css")
                .header("if-none-match", &etag)
                .build(),
        )
        .await;
    assert_eq!(
        warm.status,
        StatusCode::NOT_MODIFIED,
        "304 on ETag: {warm:?}"
    );
    assert!(warm.body.is_empty(), "304 carries no body");

    let head = fixture
        .request(Req::head("/assets/styles.css").build())
        .await;
    assert_eq!(head.status, StatusCode::OK, "HEAD styles.css: {head:?}");
    assert!(head.body.is_empty(), "HEAD carries no body");
    assert_eq!(head.content_type(), "text/css; charset=utf-8");
    assert_eq!(
        head.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    fixture.teardown();
}

/// `GET /` sends visitors to the console, with the body Go's `http.Redirect`
/// wrote: an absolute `location` built from the request host, the `text/html`
/// content type, and the `See Other` link.
#[tokio::test]
async fn root_redirects_to_the_console() {
    let fixture = Fixture::new();
    let reply = fixture.get("/").await;

    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "http://example.com/manage/links");
    assert_eq!(
        reply.content_type(),
        "text/html; charset=utf-8",
        "content type of the redirect body"
    );
    assert_eq!(
        reply.text(),
        "<a href=\"http://example.com/manage/links\">See Other</a>\n",
        "redirect body"
    );
    reply.assert_security_headers();
    fixture.teardown();
}

/// Unmatched paths answer the previous `http.NotFound` body.
#[tokio::test]
async fn unmatched_paths_answer_the_plain_text_not_found() {
    let fixture = Fixture::new();

    for path in [
        "/manage/unknown",
        "/api/links/x/y",
        "/a/b/c",
        "/deep/nested/path",
        "/manage/links/abc/extra",
    ] {
        let reply = fixture.get(path).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "path {path}: {reply:?}"
        );
        assert_eq!(
            reply.content_type(),
            "text/plain; charset=utf-8",
            "path {path}"
        );
        assert_eq!(reply.text(), "404 page not found\n", "path {path}");
        reply.assert_security_headers();
        assert!(
            !reply.request_id().is_empty(),
            "path {path} carries a request identifier: {reply:?}"
        );
    }

    // The Go multiplexer's 404 paths are registered as routes, so the request
    // counter labels them: a path that matches no pattern keeps its own text,
    // and a path that falls under a registered pattern is labelled with that
    // pattern (`internal/web/server.go:97-101`, the mux's 404 paths).
    let body = fixture.metrics_body().await;
    common::assert_metric_lines(
        &body,
        &[
            "shorten_http_requests_total{route=\"/manage/unknown\",status=\"404\"} 1\n",
            "shorten_http_requests_total{route=\"/a/b/c\",status=\"404\"} 1\n",
            "shorten_http_requests_total{route=\"/deep/nested/path\",status=\"404\"} 1\n",
            "shorten_http_requests_total{route=\"/api/links/{slug}\",status=\"404\"} 1\n",
            "shorten_http_requests_total{route=\"/manage/links/{slug}\",status=\"404\"} 1\n",
        ],
    );
    fixture.teardown();
}

/// A path that is registered for other methods answers 405 with the `allow`
/// header naming those methods.
///
/// Go's `ServeMux` answered this case itself: 405 with the plain-text body
/// `Method Not Allowed\n` and an `Allow` list sorted alphabetically, with
/// `HEAD` added for every `GET` pattern it registered for the path
/// (`internal/web/server.go:64-95`). The Rust router registers the same method
/// sets per route and answers with `state::method_not_allowed`, so the list
/// and the body below are Go's.
#[tokio::test]
async fn method_fallback_lists_the_registered_methods() {
    let fixture = Fixture::new();
    let slug = "methodgate-y";
    let created = fixture.create(common::input(slug, "https://methodgate.example/one"));
    assert_eq!(created.slug, slug, "the probe link is stored");

    let cases: [(&str, &str, &str); 5] = [
        ("POST", "/manage/links/new", "GET, HEAD"),
        ("POST", "/api/links", "GET, HEAD"),
        ("PUT", "/api/links/methodgate-y", "DELETE, GET, HEAD, PATCH"),
        ("POST", "/readyz", "GET, HEAD"),
        ("POST", "/healthz", "GET, HEAD"),
    ];
    for (method, path, allow) in cases {
        let reply = fixture.request(Req::of(method, path).build()).await;
        assert_eq!(
            reply.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}: {reply:?}"
        );
        assert_eq!(
            reply.header("allow"),
            Some(allow),
            "the `allow` list of {method} {path}, in Go's sorted order"
        );
        assert_eq!(
            reply.text(),
            "Method Not Allowed\n",
            "the mux-shaped body: {method} {path}: {reply:?}"
        );
        assert_eq!(
            reply.header("content-type"),
            Some("text/plain; charset=utf-8"),
            "media type of the 405 body: {method} {path}"
        );
        reply.assert_security_headers();
    }

    fixture.teardown();
}

/// The middleware stamps every security header on every response shape, and the
/// request identifier is the base-36 counter of the previous service.
#[tokio::test]
async fn request_ids_follow_the_base36_counter() {
    let fixture = Fixture::new();
    let slug = "idcounterx";
    let created = fixture.create(common::input(slug, "https://ids.example/target"));
    assert_eq!(created.slug, slug, "the probe link is stored");

    for expected in 1_u64..=4 {
        let reply = fixture.get("/healthz").await;
        reply.assert_security_headers_with_id(&base36(expected));
    }

    // Run the counter up to 36, the first two-digit base-36 identifier.
    let mut ids = Vec::new();
    for _ in 5_u64..=36 {
        let reply = fixture.get("/healthz").await;
        reply.assert_security_headers();
        ids.push(reply.request_id().to_string());
    }
    let expected: Vec<String> = (5_u64..=36).map(base36).collect();
    assert_eq!(ids, expected, "counter runs 5…36 in base 36");
    assert_eq!(
        ids.last().expect("counter reached 36"),
        "10",
        "36 is the first two-digit identifier"
    );

    // Every response shape carries the header set.
    for (method, path) in [
        ("GET", "/healthz"),
        ("GET", "/readyz"),
        ("GET", "/metrics"),
        ("GET", "/assets/app.css"),
        ("GET", "/"),
        ("GET", "/manage/links"),
        ("GET", &format!("/{slug}")),
        ("GET", "/nothing-here"),
        ("POST", &format!("/{slug}")),
        ("GET", "/api/links"),
    ] {
        let reply = fixture
            .request(Req::of(method, path).api_key().build())
            .await;
        reply.assert_security_headers();
    }
    fixture.teardown();
}
