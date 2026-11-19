//! Parity of the hot redirect path: `GET /{slug}` and `HEAD /{slug}`.
//!
//! Reference: Go `internal/web/redirect.go` (method gate, the plain-text 404,
//! the 410 of an unavailable link, `http.Redirect` with the stored status) and
//! `internal/web/server.go:97` (the slug route answers every method, so the
//! handler writes the 405 and its `Allow` header itself).

mod common;

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use common::{
    assert_metric_lines, input, input_disabled, input_expired, input_status, Fixture, Req,
};
use shorten::link::Status;

/// Every stored `redirect_status` is answered with that exact status and the
/// stored destination in `Location`, with no `Cache-Control` and the full
/// security header set.
#[tokio::test]
async fn every_stored_redirect_status_is_answered_verbatim() {
    let fixture = Fixture::new();

    let cases = [
        (301_i32, StatusCode::MOVED_PERMANENTLY),
        (302, StatusCode::FOUND),
        (307, StatusCode::TEMPORARY_REDIRECT),
        (308, StatusCode::PERMANENT_REDIRECT),
    ];
    for (stored, expected) in cases {
        let slug = format!("status{stored}");
        let destination = format!("https://redirect.example/{stored}/target?utm=1#fragment");
        let created = fixture.create(input_status(&slug, &destination, stored));
        assert_eq!(created.redirect_status, stored, "stored status for {slug}");

        let reply = fixture.get(&format!("/{slug}")).await;
        assert_eq!(
            reply.status, expected,
            "status {stored} for {slug}: {reply:?}"
        );
        assert_eq!(
            reply.header("location"),
            Some(created.destination_url.as_str()),
            "Location is the stored destination of {slug}"
        );
        assert_eq!(
            reply.header("location").map(str::to_string).unwrap(),
            destination
        );
        reply.assert_short_link_redirect_headers();
        reply.assert_no_cache_control();
    }

    fixture.teardown();
}

/// The success response shape: `Location` plus the security headers, and
/// nothing a cache may key on.
///
/// The success response is Go's `http.Redirect` response: the stored status,
/// the stored destination as `location`, the `text/html; charset=utf-8`
/// content type and the one-line body the standard library wrote, with the
/// status text Go used (`Found` for 302, `Moved Permanently` for 301,
/// `Temporary Redirect` for 307, `Permanent Redirect` for 308).
#[tokio::test]
async fn success_response_is_the_go_redirect_response() {
    let fixture = Fixture::new();
    let slug = "shape302xx";
    let created = fixture.create(input(slug, "https://shape.example/landing"));

    let reply = fixture.get(&format!("/{slug}")).await;
    assert_eq!(reply.status, StatusCode::FOUND, "redirect: {reply:?}");
    assert_eq!(
        reply.header("location"),
        Some(created.destination_url.as_str()),
        "Location header"
    );
    assert!(
        reply.header("cache-control").is_none(),
        "a redirect is never cacheable: {reply:?}"
    );
    assert_eq!(
        reply.content_type(),
        "text/html; charset=utf-8",
        "the redirect body's content type: {:?}",
        reply.headers
    );
    assert_eq!(
        reply.text(),
        "<a href=\"https://shape.example/landing\">Found</a>\n",
        "redirect body: {reply:?}"
    );
    reply.assert_short_link_redirect_headers();
    reply.assert_no_cache_control();

    fixture.teardown();
}

/// `HEAD /{slug}` answers the same status and `Location` with no body, and
/// never hands the visit to the analytics collector.
#[tokio::test]
async fn head_requests_answer_the_redirect_without_a_body() {
    let fixture = Fixture::new();
    let slug = "headmethod";
    let created = fixture.create(input(slug, "https://head.example/target"));

    let head = fixture
        .request(Req::head(&format!("/{slug}")).build())
        .await;
    assert_eq!(head.status, StatusCode::FOUND, "HEAD redirect: {head:?}");
    assert_eq!(
        head.header("location"),
        Some(created.destination_url.as_str()),
        "HEAD carries Location"
    );
    assert!(head.body.is_empty(), "HEAD carries no body: {head:?}");
    head.assert_short_link_redirect_headers();
    head.assert_no_cache_control();
    assert_eq!(fixture.visits(slug), 0, "HEAD never reaches the collector");

    // A `GET` of the same slug is a visit; the flush lands it in the store.
    let get = fixture.get(&format!("/{slug}")).await;
    assert_eq!(get.status, StatusCode::FOUND, "GET redirect: {get:?}");
    fixture.app().analytics().flush().expect("analytics flush");
    assert_eq!(fixture.visits(slug), 1, "the GET visit is recorded");

    fixture.teardown();
}

/// An unknown slug answers the previous `http.NotFound` body, twice: the
/// second lookup runs against the negative cache entry.
#[tokio::test]
async fn unknown_slug_answers_the_plain_text_not_found() {
    let fixture = Fixture::new();

    for attempt in 1..=2 {
        let reply = fixture.get("/absent-slug-9").await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "attempt {attempt}: {reply:?}"
        );
        assert_eq!(
            reply.content_type(),
            "text/plain; charset=utf-8",
            "attempt {attempt}: {reply:?}"
        );
        assert_eq!(reply.text(), "404 page not found\n", "attempt {attempt}");
        reply.assert_security_headers();
        reply.assert_no_cache_control();
    }

    let body = fixture.metrics_body().await;
    assert_metric_lines(
        &body,
        &[
            "shorten_redirects_total{outcome=\"missing\"} 2\n",
            "shorten_redirect_cache_misses_total 1\n",
            "shorten_redirect_cache_hits_total 1\n",
            "shorten_redirect_cache_size 1\n",
        ],
    );

    fixture.teardown();
}

/// The reserved names and the malformed codes are answered by the slug
/// handler's own 404, before any store lookup.
#[tokio::test]
async fn reserved_and_malformed_slugs_answer_not_found() {
    let fixture = Fixture::new();
    let reserved_probe =
        fixture.create(input("api-is-reserved", "https://reserved.example/target"));
    assert_eq!(reserved_probe.slug, "api-is-reserved", "probe link stored");

    for path in ["/api", "/manage", "/assets", "/favicon.ico", "/ab", "/to"] {
        let reply = fixture.get(path).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "reserved or short slug {path}: {reply:?}"
        );
        assert_eq!(reply.text(), "404 page not found\n", "body of {path}");
        reply.assert_security_headers();
    }

    fixture.teardown();
}

/// Any method other than `GET` and `HEAD` is rejected with 405 and the
/// `allow` header naming the two accepted methods (lower-cased on the wire by
/// HTTP/1.1), with the security headers stamped on it and no cache marker.
///
/// The slug route is registered for every method — the previous multiplexer
/// registered `/{slug}` without a method (`internal/web/server.go:97`), so
/// `redirect.go:13-18` wrote the answer itself: `Allow: GET, HEAD`, the
/// plain-text body `Method not allowed.\n`, and the `method_not_allowed`
/// outcome counter. The Rust router does the same (`any(redirect::handle)` in
/// `crates/shorten-web/src/router.rs`), which is why the body and the counter
/// below are part of the contract, not a fallback artifact.
#[tokio::test]
async fn other_methods_answer_method_not_allowed() {
    let fixture = Fixture::new();
    let slug = "methodgate";
    let gated = fixture.create(input(slug, "https://method.example/target"));
    assert_eq!(gated.slug, slug, "probe link stored");

    for method in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let reply = fixture
            .request(Req::of(method, &format!("/{slug}")).build())
            .await;
        assert_eq!(
            reply.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on a slug: {reply:?}"
        );
        assert_eq!(
            reply.header("allow"),
            Some("GET, HEAD"),
            "the `Allow` list of a {method} request"
        );
        assert_eq!(
            reply.content_type(),
            "text/plain; charset=utf-8",
            "the redirect handler answers with the plain-text body `http.Error` wrote: {reply:?}"
        );
        assert_eq!(
            reply.text(),
            "Method not allowed.\n",
            "{method} fallback body: {reply:?}"
        );
        reply.assert_security_headers();
        reply.assert_no_cache_control();
    }

    // Each rejected method is counted, as the Go handler counted it.
    let body = fixture.metrics_body().await;
    assert_metric_lines(
        &body,
        &[
            "shorten_redirect_requests_total 4\n",
            "shorten_redirects_total{outcome=\"method_not_allowed\"} 4\n",
        ],
    );

    fixture.teardown();
}

/// A disabled link is gone: 410 with the Go body text, `404`-style headers,
/// and the `disabled` outcome.
#[tokio::test]
async fn disabled_link_is_gone() {
    let fixture = Fixture::new();
    let slug = "offlinelink";
    let offline = fixture.create(input_disabled(slug, "https://gone.example/target"));
    assert_eq!(offline.status, Status::Disabled, "disabled link stored");

    let reply = fixture.get(&format!("/{slug}")).await;
    assert_eq!(reply.status, StatusCode::GONE, "disabled link: {reply:?}");
    assert_eq!(
        reply.content_type(),
        "text/plain; charset=utf-8",
        "disabled body type: {reply:?}"
    );
    assert_eq!(
        reply.text(),
        "This short link is no longer available.\n",
        "disabled body"
    );
    assert!(
        reply.header("location").is_none(),
        "a gone link never carries Location: {reply:?}"
    );
    reply.assert_security_headers();
    reply.assert_no_cache_control();

    // The disabled answer is cached, so the second lookup is a cache hit.
    let second = fixture.get(&format!("/{slug}")).await;
    assert_eq!(second.status, StatusCode::GONE, "second lookup: {second:?}");

    let body = fixture.metrics_body().await;
    assert_metric_lines(
        &body,
        &[
            "shorten_redirects_total{outcome=\"disabled\"} 2\n",
            "shorten_redirects_total{outcome=\"success\"} 0\n",
            "shorten_redirect_cache_misses_total 1\n",
            "shorten_redirect_cache_hits_total 1\n",
        ],
    );

    fixture.teardown();
}

/// An expired link is gone as well, with the `expired` outcome: the store
/// timestamp is a fixed instant in the past, so there is no clock race.
#[tokio::test]
async fn expired_link_is_gone() {
    let fixture = Fixture::new();
    let slug = "expiredlink";
    let past = Utc::now() - Duration::days(3);
    let gone = fixture.create(input_expired(
        slug,
        "https://expired.example/target",
        common::at_millis(past.timestamp_millis()),
    ));
    assert!(
        gone.is_expired(Utc::now()),
        "the stored link is expired on write"
    );

    let reply = fixture.get(&format!("/{slug}")).await;
    assert_eq!(reply.status, StatusCode::GONE, "expired link: {reply:?}");
    assert_eq!(
        reply.text(),
        "This short link is no longer available.\n",
        "expired body"
    );
    reply.assert_security_headers();
    reply.assert_no_cache_control();

    let body = fixture.metrics_body().await;
    assert_metric_lines(
        &body,
        &[
            "shorten_redirects_total{outcome=\"expired\"} 1\n",
            "shorten_redirects_total{outcome=\"disabled\"} 0\n",
        ],
    );

    fixture.teardown();
}

/// The outcome counters, the request counter, and the duration sum, in the
/// order the Go registry renders them.
#[tokio::test]
async fn redirect_outcome_counters_land_in_metrics() {
    let fixture = Fixture::new();
    let live = fixture.create(input("counter-live", "https://counter.example/live"));
    let off = fixture.create(input_disabled("counter-off", "https://counter.example/off"));
    let past = fixture.create(input_expired(
        "counter-past",
        "https://counter.example/past",
        common::at_millis((Utc::now() - Duration::days(2)).timestamp_millis()),
    ));
    assert_eq!(live.status, Status::Active, "live link stored");
    assert_eq!(off.status, Status::Disabled, "disabled link stored");
    assert!(past.is_expired(Utc::now()), "expired link stored");

    let live = fixture.get("/counter-live").await;
    assert_eq!(live.status, StatusCode::FOUND, "live: {live:?}");
    let repeat = fixture.get("/counter-live").await;
    assert_eq!(repeat.status, StatusCode::FOUND, "repeat: {repeat:?}");
    let absent = fixture.get("/counter-absent").await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND, "absent: {absent:?}");
    let off = fixture.get("/counter-off").await;
    assert_eq!(off.status, StatusCode::GONE, "off: {off:?}");
    let past = fixture.get("/counter-past").await;
    assert_eq!(past.status, StatusCode::GONE, "past: {past:?}");
    let posted = fixture.request(Req::post("/counter-live").build()).await;
    assert_eq!(
        posted.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "posted: {posted:?}"
    );

    let body = fixture.metrics_body().await;
    assert_metric_lines(
        &body,
        &[
            "shorten_redirect_requests_total 6\n",
            "shorten_redirects_total{outcome=\"success\"} 2\n",
            "shorten_redirects_total{outcome=\"missing\"} 1\n",
            "shorten_redirects_total{outcome=\"disabled\"} 1\n",
            "shorten_redirects_total{outcome=\"expired\"} 1\n",
            "shorten_redirects_total{outcome=\"method_not_allowed\"} 1\n",
            "shorten_redirects_total{outcome=\"error\"} 0\n",
            "shorten_redirect_cache_hits_total 1\n",
            "shorten_redirect_cache_misses_total 4\n",
        ],
    );

    // The Go registry renders the request counter, then the duration sum, then
    // the outcome lines: the order is the contract, not a comment block.
    let requests = body
        .find("shorten_redirect_requests_total ")
        .expect("request counter line");
    let duration = body
        .find("shorten_redirect_duration_seconds_sum ")
        .expect("duration sum line");
    let success = body
        .find("shorten_redirects_total{outcome=\"success\"}")
        .expect("success outcome line");
    assert!(
        requests < duration,
        "request counter precedes the duration sum"
    );
    assert!(
        duration < success,
        "duration sum precedes the outcome lines"
    );
    assert!(
        !body.contains('#'),
        "the Go metrics body carries no comment lines:\n{body}"
    );

    let duration_line = body
        .lines()
        .find(|line| line.starts_with("shorten_redirect_duration_seconds_sum "))
        .expect("duration line");
    let value = duration_line
        .split_whitespace()
        .nth(1)
        .expect("duration value");
    let parsed = value.parse::<f64>().expect("duration sum is a float");
    assert!(
        parsed > 0.0,
        "six requests accumulate time: {duration_line}"
    );
    assert_eq!(
        value.split('.').nth(1).expect("fraction").len(),
        9,
        "nine decimals, the Go fixed layout: {duration_line}"
    );

    fixture.teardown();
}

/// Every answer shape of the redirect path is uncacheable.
#[tokio::test]
async fn redirect_responses_are_not_cacheable() {
    let fixture = Fixture::new();
    let moved = fixture.create(input_status(
        "nocache301",
        "https://nocache.example/one",
        301,
    ));
    let offline = fixture.create(input_disabled("nocacheoff", "https://nocache.example/off"));
    assert_eq!(moved.redirect_status, 301, "301 stored");
    assert_eq!(offline.status, Status::Disabled, "disabled link stored");

    let paths = [
        ("/nocache301", StatusCode::MOVED_PERMANENTLY),
        ("/nocache-abs", StatusCode::NOT_FOUND),
        ("/nocacheoff", StatusCode::GONE),
    ];
    for (path, expected) in paths {
        let get = fixture.get(path).await;
        assert_eq!(get.status, expected, "GET {path}: {get:?}");
        get.assert_no_cache_control();

        let head = fixture.request(Req::head(path).build()).await;
        assert_eq!(head.status, expected, "HEAD {path}: {head:?}");
        head.assert_no_cache_control();
    }

    let post = fixture.request(Req::post("/nocache301").build()).await;
    assert_eq!(
        post.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "POST: {post:?}"
    );
    post.assert_no_cache_control();

    fixture.teardown();
}

/// A destination change is visible on the next lookup: the API patch
/// invalidates the cache key of the slug, exactly as Go's `apiPatch` calls
/// `s.cache.Invalidate` (`internal/web/api.go:150`).
#[tokio::test]
async fn an_update_replaces_the_cached_destination() {
    let fixture = Fixture::new();
    let slug = "renamedstxxx";
    let created = fixture.create(input_status(slug, "https://move.example/before", 301));
    let cached = fixture.get(&format!("/{slug}")).await;
    assert_eq!(cached.status, StatusCode::MOVED_PERMANENTLY, "{cached:?}");
    assert_eq!(
        cached.header("location"),
        Some(created.destination_url.as_str()),
        "first destination"
    );

    let patched = fixture
        .request(
            Req::patch(&format!("/api/links/{slug}"))
                .api_key()
                .json("{\"url\":\"https://move.example/after\",\"redirectStatus\":302}")
                .build(),
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "api patch: {patched:?}");

    let after = fixture.get(&format!("/{slug}")).await;
    assert_eq!(after.status, StatusCode::FOUND, "the new status: {after:?}");
    assert_eq!(
        after.header("location"),
        Some("https://move.example/after"),
        "exact Location of the moved link"
    );

    fixture.teardown();
}
