//! Parity of the management console under `/manage`.
//!
//! Reference: Go `internal/web/manage.go` (the `requireSession` gate, the
//! login/logout cookie writer, the CSRF and origin guard inside `allowMutation`,
//! the redirect targets and notices, the list controls and the cleanup preview),
//! `internal/web/server.go:78-91` (route table), `internal/auth` (cookie
//! attributes) and `internal/web/templates_templ.go` (the markup markers).
//!
//! Askama escapes `<` inside text nodes as `&#60;` where the Go templates emit
//! `&lt;`, so every HTML assertion here is a marker string or substring, never a
//! byte-for-byte comparison of markup.

mod common;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use common::{
    aged_record, at_millis, base_url_of, cookie_attribute, cookie_attribute_names, cookie_flag,
    htmx_mutation, login, mutation, parse_csrf, Fixture, Req, SESSION_COOKIE,
};
use shorten::link::{Link, Status};

/// `POST /manage/login` with the fixture key: the session token and its csrf
/// token, the same pair the browser carries.
async fn session(fixture: &Fixture) -> (String, String) {
    login(fixture).await
}

/// Every console route answers an anonymous visitor with 303 to the sign-in
/// page, for `GET` as well as for the mutating `POST`s.
///
/// Go's `http.Redirect` wrote an absolute `location` — resolved against the
/// request `Host` — plus the `<a href=…>See Other</a>` body and its
/// `text/html` content type. The Rust handlers reproduce that shape, so the
/// target here is absolute and the body is the standard library's one-liner.
#[tokio::test]
async fn console_routes_redirect_anonymous_visitors_to_sign_in() {
    let fixture = Fixture::new();

    let routes: [(Method, &str); 8] = [
        (Method::GET, "/manage/links"),
        (Method::GET, "/manage/links/new"),
        (Method::GET, "/manage/links/some-slug"),
        (Method::GET, "/manage/links/some-slug/delete"),
        (Method::GET, "/manage/cleanup"),
        (Method::POST, "/manage/links"),
        (Method::POST, "/manage/links/some-slug/delete"),
        (Method::POST, "/manage/logout"),
    ];
    for (method, path) in routes {
        let reply = fixture
            .request(Req::of(method.as_str(), path).build())
            .await;
        assert_eq!(
            reply.status,
            StatusCode::SEE_OTHER,
            "{method} {path}: {reply:?}"
        );
        assert_eq!(
            reply.header("location"),
            Some("http://example.com/manage/login"),
            "sign-in target of {method} {path}"
        );
        assert_eq!(
            reply.text(),
            "<a href=\"http://example.com/manage/login\">See Other</a>\n",
            "{method} {path} body of the redirect: {reply:?}"
        );
        assert_eq!(
            reply.header("content-type"),
            Some("text/html; charset=utf-8"),
            "{method} {path} content type of the redirect"
        );
        reply.assert_security_headers();
        reply.assert_no_cache_control();
    }

    // A valid session opens the same routes.
    let (token, _csrf) = session(&fixture).await;
    let links = fixture
        .request(Req::get("/manage/links").cookie(&token).build())
        .await;
    assert_eq!(links.status, StatusCode::OK, "signed-in list: {links:?}");

    fixture.teardown();
}

/// The sign-in page carries the Go markup markers and the CSRF field.
#[tokio::test]
async fn sign_in_page_carries_the_go_markers() {
    let fixture = Fixture::new();

    let reply = fixture.get("/manage/login").await;
    assert_eq!(reply.status, StatusCode::OK, "login page: {reply:?}");
    assert_eq!(
        reply.content_type(),
        "text/html; charset=utf-8",
        "the console media type"
    );
    let page = reply.text();
    for marker in [
        "Enter your administrator API key to start a secure session.",
        "id=\"login-title\"",
        "name=\"api_key\"",
        "id=\"api-key\"",
        "action=\"/manage/login\"",
        "method=\"post\"",
    ] {
        assert!(
            page.contains(marker),
            "login page marker {marker:?}:\n{page}"
        );
    }
    assert!(
        parse_csrf(&page).is_none(),
        "the sign-in form has no csrf field, as in the Go template"
    );
    reply.assert_security_headers();
    reply.assert_no_cache_control();

    // A signed-in visitor is bounced from the sign-in page to the list.
    let (token, _csrf) = session(&fixture).await;
    let signed_in = fixture
        .request(Req::get("/manage/login").cookie(&token).build())
        .await;
    assert_eq!(
        signed_in.status,
        StatusCode::SEE_OTHER,
        "sign-in page while signed in: {signed_in:?}"
    );
    assert_eq!(
        signed_in.header("location"),
        Some("http://example.com/manage/links"),
        "the list is the target"
    );

    fixture.teardown();
}

/// A successful sign-in answers 303 to the list and sets the session cookie
/// with the previous cookie attributes: `Path=/`, an RFC 1123 `Expires`, the
/// session `Max-Age`, `HttpOnly`, `SameSite=Strict`, and no `Secure` flag while
/// cookies are served over plain HTTP.
#[tokio::test]
async fn login_issues_the_session_cookie() {
    let fixture = Fixture::new();

    let reply = fixture
        .request(
            Req::post("/manage/login")
                .form(&[("api_key", common::API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "login: {reply:?}");
    assert_eq!(
        reply.header("location"),
        Some("http://example.com/manage/links"),
        "the list is the redirect target"
    );
    reply.assert_security_headers();

    let cookie = reply
        .header("set-cookie")
        .expect("login sets the session cookie")
        .to_string();
    assert!(
        cookie.starts_with(&format!("{SESSION_COOKIE}=")),
        "cookie name and value: {cookie}"
    );
    assert_eq!(
        cookie_attribute(&cookie, "Path").as_deref(),
        Some("/"),
        "cookie path: {cookie}"
    );
    // `Max-Age` is the whole-second truncation of the remaining lifetime, so a
    // run inside the final second of the twelfth hour reads 43199.
    let max_age: i64 = cookie_attribute(&cookie, "Max-Age")
        .expect("Max-Age attribute")
        .parse()
        .expect("Max-Age is a number");
    assert!(
        (43_199..=43_200).contains(&max_age),
        "the default session lifetime is twelve hours: {cookie}"
    );
    assert!(cookie_flag(&cookie, "HttpOnly"), "HttpOnly flag: {cookie}");
    assert_eq!(
        cookie_attribute(&cookie, "SameSite").as_deref(),
        Some("Strict"),
        "SameSite attribute: {cookie}"
    );
    assert!(
        !cookie_flag(&cookie, "Secure"),
        "no Secure flag with `SHORTEN_INSECURE_COOKIES=true`: {cookie}"
    );

    // Attribute order follows the previous writer: name=value, Path, Expires,
    // Max-Age, then the flags.
    let names = cookie_attribute_names(&cookie);
    let expected = [
        "Path".to_string(),
        "Expires".to_string(),
        "Max-Age".to_string(),
        "HttpOnly".to_string(),
        "SameSite".to_string(),
    ];
    assert_eq!(
        names, expected,
        "cookie attribute order, as Go's cookie writer emits them: {cookie}"
    );

    // `Expires` is RFC 1123 GMT and agrees with `Max-Age`.
    let expires = cookie_attribute(&cookie, "Expires").expect("Expires attribute");
    assert!(expires.ends_with(" GMT"), "GMT zone in {expires}");
    let parsed = chrono::DateTime::parse_from_rfc2822(&expires)
        .expect("Expires is an RFC 1123 timestamp")
        .with_timezone(&Utc);
    let max_age: i64 = cookie_attribute(&cookie, "Max-Age")
        .expect("Max-Age attribute")
        .parse()
        .expect("Max-Age is a number");
    let drift = (parsed - Utc::now()).num_seconds() - max_age;
    assert!(
        drift.abs() <= 5,
        "Expires and Max-Age describe the same instant, drift {drift}s: {cookie}"
    );

    // The issued token opens the console.
    let token = reply.session_cookie_value().expect("session cookie value");
    let links = fixture
        .request(Req::get("/manage/links").cookie(&token).build())
        .await;
    assert_eq!(links.status, StatusCode::OK, "signed-in list: {links:?}");

    fixture.teardown();
}

/// A wrong key re-renders the sign-in page with the Go copy and 401, and a form
/// posted from another origin is refused the same way, before the key is ever
/// compared. Neither sets a cookie.
#[tokio::test]
async fn login_rejects_the_wrong_key_and_foreign_origins() {
    let fixture = Fixture::new();
    let base_url = base_url_of(&fixture);

    let wrong = fixture
        .request(
            Req::post("/manage/login")
                .form(&[("api_key", "not-the-admin-key")])
                .build(),
        )
        .await;
    assert_eq!(
        wrong.status,
        StatusCode::UNAUTHORIZED,
        "wrong key: {wrong:?}"
    );
    let page = wrong.text();
    assert!(
        page.contains("That API key is not valid."),
        "the Go copy (manage.go:38):\n{page}"
    );
    assert!(
        wrong.header("set-cookie").is_none(),
        "a rejected login sets no session: {:?}",
        wrong.headers
    );
    assert!(
        page.contains("name=\"api_key\"") && page.contains("role=\"alert\""),
        "the sign-in form is re-rendered with the failure banner:\n{page}"
    );
    wrong.assert_security_headers();

    // A same-origin login is accepted, and a cross-origin one is refused with
    // the identical copy (`manage.go:35-40`).
    let cross = fixture
        .request(
            Req::post("/manage/login")
                .header("origin", "https://evil.example")
                .form(&[("api_key", common::API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(
        cross.status,
        StatusCode::UNAUTHORIZED,
        "cross-origin: {cross:?}"
    );
    assert!(
        cross.text().contains("That API key is not valid."),
        "the origin guard reuses the credential copy"
    );
    assert!(
        cross.header("set-cookie").is_none(),
        "a cross-origin login sets no session"
    );

    let same_origin = fixture
        .request(
            Req::post("/manage/login")
                .header("origin", &base_url)
                .form(&[("api_key", common::API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(
        same_origin.status,
        StatusCode::SEE_OTHER,
        "origin equal to the public base URL: {same_origin:?}"
    );

    // A trailing slash on the origin is the same origin, as Go trims it.
    let trailing = fixture
        .request(
            Req::post("/manage/login")
                .header("origin", &format!("{base_url}/"))
                .form(&[("api_key", common::API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(
        trailing.status,
        StatusCode::SEE_OTHER,
        "origin with a trailing slash: {trailing:?}"
    );

    fixture.teardown();
}

/// The sign-in limiter allows eight attempts per minute from one client address
/// and then re-renders with the rate-limit copy.
#[tokio::test]
async fn login_is_rate_limited_per_client() {
    let fixture = Fixture::new();
    let peer = "198.51.100.24:41000";

    for attempt in 1..=8 {
        let reply = fixture
            .request(
                Req::post("/manage/login")
                    .peer(peer)
                    .form(&[("api_key", "wrong-key")])
                    .build(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} from {peer}: {reply:?}"
        );
    }

    let blocked = fixture
        .request(
            Req::post("/manage/login")
                .peer(peer)
                .form(&[("api_key", common::API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(
        blocked.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the ninth attempt: {blocked:?}"
    );
    assert!(
        blocked
            .text()
            .contains("Too many attempts. Wait a minute and try again."),
        "the limiter copy (manage.go:28)"
    );
    assert!(
        blocked.header("set-cookie").is_none(),
        "the limiter runs before the credential check"
    );

    // A different client address has its own allowance.
    let other = fixture
        .request(
            Req::post("/manage/login")
                .peer("198.51.100.25:41000")
                .form(&[("api_key", common::API_KEY)])
                .build(),
        )
        .await;
    assert_eq!(
        other.status,
        StatusCode::SEE_OTHER,
        "the second address signs in: {other:?}"
    );

    fixture.teardown();
}

/// Signing out clears the cookie with the removal attributes and answers 303 to
/// the sign-in page; the cleared token no longer opens the console.
#[tokio::test]
async fn logout_clears_the_session_cookie() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;

    let reply = mutation(&fixture, "/manage/logout", &token, &csrf, &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "logout: {reply:?}");
    assert_eq!(
        reply.header("location"),
        Some("http://example.com/manage/login"),
        "the sign-in page is the target"
    );

    let cookie = reply
        .header("set-cookie")
        .expect("logout rewrites the session cookie")
        .to_string();
    assert!(
        cookie.starts_with(&format!("{SESSION_COOKIE}=;")),
        "the value is cleared: {cookie}"
    );
    assert_eq!(
        cookie_attribute(&cookie, "Max-Age").as_deref(),
        Some("-1"),
        "removal marker: {cookie}"
    );
    assert_eq!(
        cookie_attribute(&cookie, "Path").as_deref(),
        Some("/"),
        "the clearing cookie keeps the path: {cookie}"
    );
    assert!(
        cookie_flag(&cookie, "HttpOnly"),
        "HttpOnly is preserved: {cookie}"
    );
    assert_eq!(
        cookie_attribute(&cookie, "SameSite").as_deref(),
        Some("Strict"),
        "SameSite is preserved: {cookie}"
    );

    // Signing out requires a valid token: without the csrf field it is the 403.
    let without_token = fixture
        .request(Req::post("/manage/logout").cookie(&token).form(&[]).build())
        .await;
    assert_eq!(
        without_token.status,
        StatusCode::FORBIDDEN,
        "logout without the csrf field: {without_token:?}"
    );
    assert_eq!(
        without_token.text(),
        "Invalid security token.\n",
        "the Go copy (manage.go:56)"
    );

    fixture.teardown();
}

/// Every mutation form needs the session's csrf token and a same-origin
/// `Origin`; a missing, stale, or foreign submission is the 403 plain-text copy.
#[tokio::test]
async fn mutations_require_the_csrf_token_and_the_origin() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;
    let pairs = [
        ("destination", "https://csrf.example/target"),
        ("slug", "csrf-gate"),
        ("title", ""),
        ("notes", ""),
        ("status", "active"),
        ("redirect_status", "302"),
        ("expires_at", ""),
    ];

    // No csrf field at all.
    let missing = fixture
        .request(
            Req::post("/manage/links")
                .cookie(&token)
                .form(&pairs)
                .build(),
        )
        .await;
    assert_eq!(
        missing.status,
        StatusCode::FORBIDDEN,
        "create without csrf: {missing:?}"
    );
    assert_eq!(missing.text(), "Invalid security token.\n");
    assert_eq!(
        missing.content_type(),
        "text/plain; charset=utf-8",
        "the http.Error media type"
    );
    missing.assert_security_headers();

    // A wrong token.
    let mut stale: Vec<(&str, &str)> = pairs.to_vec();
    stale.push(("csrf", "the-wrong-csrf-token"));
    let wrong = fixture
        .request(
            Req::post("/manage/links")
                .cookie(&token)
                .form(&stale)
                .build(),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN, "stale csrf: {wrong:?}");
    assert_eq!(wrong.text(), "Invalid security token.\n");

    // The right token, from a foreign origin.
    let foreign = fixture
        .request(
            Req::post("/manage/links")
                .cookie(&token)
                .header("origin", "https://evil.example")
                .form(&[
                    ("destination", "https://csrf.example/two"),
                    ("slug", "csrf-gate-two"),
                    ("csrf", &csrf),
                ])
                .build(),
        )
        .await;
    assert_eq!(
        foreign.status,
        StatusCode::FORBIDDEN,
        "foreign origin: {foreign:?}"
    );
    assert_eq!(
        foreign.text(),
        "Invalid security token.\n",
        "the origin guard shares the csrf copy (manage.go:327)"
    );

    // The same form with the token and no `Origin` header is accepted, which is
    // how a plain browser form posts.
    let accepted = mutation(&fixture, "/manage/links", &token, &csrf, &pairs).await;
    assert_eq!(
        accepted.status,
        StatusCode::SEE_OTHER,
        "same-origin create: {accepted:?}"
    );

    fixture.teardown();
}

/// A create through the console answers 204 with an `hx-redirect` for HTMX and
/// 303 with `Location` for a plain form post, both naming the edit page and the
/// `created` notice.
#[tokio::test]
async fn creating_a_link_reports_created() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;
    let target = "/manage/links/console-one?notice=created";
    let pairs = [
        ("destination", "https://console.example/one"),
        ("slug", "console-one"),
        ("title", "Console one"),
        ("notes", "private"),
        ("status", "active"),
        ("redirect_status", "301"),
        ("expires_at", ""),
    ];

    let fragment = htmx_mutation(&fixture, "/manage/links", &token, &csrf, &pairs).await;
    assert_eq!(
        fragment.status,
        StatusCode::NO_CONTENT,
        "htmx create: {fragment:?}"
    );
    assert_eq!(
        fragment.header("hx-redirect"),
        Some(target),
        "the HTMX redirect instruction"
    );
    assert!(fragment.body.is_empty(), "204 has no body: {fragment:?}");
    fragment.assert_security_headers();
    fragment.assert_no_cache_control();

    // The plain form post answers 303 to the very same target.
    let second = mutation(
        &fixture,
        "/manage/links",
        &token,
        &csrf,
        &[
            ("destination", "https://console.example/two"),
            ("slug", "console-two"),
            ("title", "Console two"),
            ("notes", ""),
            ("status", "active"),
            ("redirect_status", "302"),
            ("expires_at", ""),
        ],
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::SEE_OTHER,
        "plain create: {second:?}"
    );
    assert_eq!(
        second.header("location"),
        Some("http://example.com/manage/links/console-two?notice=created"),
        "the plain redirect target"
    );

    // The stored row carries the form values, and the notice is rendered on the
    // edit page the redirect lands on.
    let stored = fixture.link("console-one");
    assert_eq!(
        stored.destination_url, "https://console.example/one",
        "the form destination is stored"
    );
    assert_eq!(
        stored.redirect_status, 301,
        "the form redirect type is stored"
    );
    assert_eq!(stored.notes, "private", "the form note is stored");

    let page = fixture
        .request(
            Req::get("/manage/links/console-one?notice=created")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "edit page: {page:?}");
    let html = page.text();
    for marker in [
        "Link created.",
        "id=\"link-form\"",
        "name=\"destination\"",
        "value=\"console-one\"",
        "https://console.example/one",
    ] {
        assert!(
            html.contains(marker),
            "edit page marker {marker:?}:\n{html}"
        );
    }

    // The new link redirects on its short path.
    let short = fixture.get("/console-one").await;
    assert_eq!(
        short.status,
        StatusCode::MOVED_PERMANENTLY,
        "the stored redirect type is the form's 301: {short:?}"
    );
    assert_eq!(
        short.header("location"),
        Some("https://console.example/one"),
        "the console create takes effect immediately"
    );

    let body = fixture.metrics_body().await;
    assert!(
        body.contains("shorten_link_mutations_total{operation=\"create\"} 2\n"),
        "both console creates are counted:\n{body}"
    );

    fixture.teardown();
}

/// A form that fails validation is rendered again: 422 for a plain post, 200
/// with the form fragment for HTMX, both carrying the field messages.
#[tokio::test]
async fn create_errors_render_the_form_again() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;

    let plain = mutation(
        &fixture,
        "/manage/links",
        &token,
        &csrf,
        &[
            ("destination", "not-a-url"),
            ("slug", "api"),
            ("title", ""),
            ("notes", ""),
            ("status", "active"),
            ("redirect_status", "302"),
            ("expires_at", "not-a-time"),
        ],
    )
    .await;
    assert_eq!(
        plain.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "422 form: {plain:?}"
    );
    assert_eq!(
        plain.content_type(),
        "text/html; charset=utf-8",
        "the form is HTML"
    );
    let page = plain.text();
    for marker in [
        "Enter a complete URL starting with http:// or https://.",
        "This short code is reserved by the application.",
        "Enter a valid UTC date and time.",
        "id=\"link-form\"",
        "A good short link",
        "name=\"destination\"",
    ] {
        assert!(
            page.contains(marker),
            "form error marker {marker:?}:\n{page}"
        );
    }
    plain.assert_security_headers();

    // The same submission under HTMX answers the fragment with 200.
    let fragment = htmx_mutation(
        &fixture,
        "/manage/links",
        &token,
        &csrf,
        &[
            ("destination", ""),
            ("slug", "console-two"),
            ("title", ""),
            ("notes", ""),
            ("status", "active"),
            ("redirect_status", "302"),
            ("expires_at", ""),
        ],
    )
    .await;
    assert_eq!(
        fragment.status,
        StatusCode::OK,
        "htmx form error: {fragment:?}"
    );
    let fragment_html = fragment.text();
    assert!(
        fragment_html.contains("id=\"link-form\""),
        "the fragment re-renders the form surface:\n{fragment_html}"
    );
    assert!(
        fragment_html.contains("Enter an HTTP or HTTPS destination up to 8,192 characters."),
        "the empty-destination message is in the fragment:\n{}",
        snippet(&fragment_html)
    );
    assert!(
        fragment_html.contains("aria-invalid=\"true\""),
        "the invalid field is marked:\n{}",
        snippet(&fragment_html)
    );

    // A duplicate slug is a conflict, reported on the form.
    let seeded = mutation(
        &fixture,
        "/manage/links",
        &token,
        &csrf,
        &[
            ("destination", "https://dupe-console.example/first"),
            ("slug", "console-dupe"),
            ("title", ""),
            ("notes", ""),
            ("status", "active"),
            ("redirect_status", "302"),
            ("expires_at", ""),
        ],
    )
    .await;
    assert_eq!(
        seeded.status,
        StatusCode::SEE_OTHER,
        "seed create: {seeded:?}"
    );
    let clash = mutation(
        &fixture,
        "/manage/links",
        &token,
        &csrf,
        &[
            ("destination", "https://dupe-console.example/second"),
            ("slug", "console-dupe"),
            ("title", ""),
            ("notes", ""),
            ("status", "active"),
            ("redirect_status", "302"),
            ("expires_at", ""),
        ],
    )
    .await;
    assert_eq!(
        clash.status,
        StatusCode::CONFLICT,
        "duplicate slug on the form: {clash:?}"
    );
    assert!(
        clash.text().contains("This short code is already in use."),
        "the form shows the console conflict copy (manage.go:395)"
    );
    assert_eq!(
        clash.content_type(),
        "text/html; charset=utf-8",
        "the conflict is a re-rendered form, not a JSON envelope"
    );

    fixture.teardown();
}

/// An update answers with the `saved` notice and writes the edited fields; the
/// rename route answers with the `renamed` notice and moves the short code.
#[tokio::test]
async fn updating_and_renaming_report_their_notices() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;
    let seeded_edit = fixture.create(common::input("console-edit", "https://edit.example/before"));
    assert_eq!(seeded_edit.slug, "console-edit", "the edit row is stored");

    let saved = mutation(
        &fixture,
        "/manage/links/console-edit",
        &token,
        &csrf,
        &[
            ("destination", "https://edit.example/after"),
            ("slug", "console-edit"),
            ("title", "Edited title"),
            ("notes", "Edited note"),
            ("status", "disabled"),
            ("redirect_status", "308"),
            ("expires_at", ""),
        ],
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "update: {saved:?}");
    assert_eq!(
        saved.header("location"),
        Some("http://example.com/manage/links/console-edit?notice=saved"),
        "the saved target"
    );
    let stored = fixture.link("console-edit");
    assert_eq!(
        stored.destination_url, "https://edit.example/after",
        "the destination is written"
    );
    assert_eq!(stored.title, "Edited title", "the title is written");
    assert_eq!(stored.notes, "Edited note", "the note is written");
    assert_eq!(stored.status, Status::Disabled, "the status is written");
    assert_eq!(stored.redirect_status, 308, "the redirect type is written");

    // HTMX answers 204 with the same instruction.
    let fragment = htmx_mutation(
        &fixture,
        "/manage/links/console-edit",
        &token,
        &csrf,
        &[
            ("destination", "https://edit.example/again"),
            ("slug", "console-edit"),
            ("title", "Again"),
            ("notes", ""),
            ("status", "active"),
            ("redirect_status", "302"),
            ("expires_at", ""),
        ],
    )
    .await;
    assert_eq!(
        fragment.status,
        StatusCode::NO_CONTENT,
        "htmx update: {fragment:?}"
    );
    assert_eq!(
        fragment.header("hx-redirect"),
        Some("/manage/links/console-edit?notice=saved"),
        "the HTMX instruction"
    );

    // The landing page shows the notice text.
    let page = fixture
        .request(
            Req::get("/manage/links/console-edit?notice=saved")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "landing page: {page:?}");
    assert!(page.text().contains("Changes saved."), "the saved notice");

    // The rename route moves the code and invalidates both cache keys.
    let before = fixture.get("/console-edit").await;
    assert_eq!(
        before.status,
        StatusCode::FOUND,
        "before rename: {before:?}"
    );
    let renamed = mutation(
        &fixture,
        "/manage/links/console-edit/rename",
        &token,
        &csrf,
        &[("slug", "console-moved")],
    )
    .await;
    assert_eq!(renamed.status, StatusCode::SEE_OTHER, "rename: {renamed:?}");
    assert_eq!(
        renamed.header("location"),
        Some("http://example.com/manage/links/console-moved?notice=renamed"),
        "the renamed target"
    );
    let gone = fixture.get("/console-edit").await;
    assert_eq!(
        gone.status,
        StatusCode::NOT_FOUND,
        "the old code is invalidated, not cached: {gone:?}"
    );
    let moved = fixture.get("/console-moved").await;
    assert_eq!(
        moved.status,
        StatusCode::FOUND,
        "the new code redirects: {moved:?}"
    );
    assert_eq!(
        moved.header("location"),
        Some("https://edit.example/again"),
        "the destination survives the rename"
    );
    let landing = fixture
        .request(
            Req::get("/manage/links/console-moved?notice=renamed")
                .cookie(&token)
                .build(),
        )
        .await;
    assert!(
        landing.text().contains("Short code renamed."),
        "the renamed notice"
    );

    // Renaming onto a taken code is the conflict copy on the confirm page.
    let occupied = fixture.create(common::input("console-taken", "https://edit.example/taken"));
    assert_eq!(occupied.slug, "console-taken", "the taken row is stored");
    let blocked = mutation(
        &fixture,
        "/manage/links/console-moved/rename",
        &token,
        &csrf,
        &[("slug", "console-taken")],
    )
    .await;
    assert_eq!(
        blocked.status,
        StatusCode::CONFLICT,
        "rename onto a taken code: {blocked:?}"
    );
    assert!(
        blocked
            .text()
            .contains("This short code is already in use."),
        "the confirm page carries the `humanError` copy (manage.go:395)"
    );

    // An edit page for an absent code is the plain-text 404.
    let absent = fixture
        .request(
            Req::get("/manage/links/console-absent")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(
        absent.status,
        StatusCode::NOT_FOUND,
        "absent edit: {absent:?}"
    );
    assert_eq!(absent.text(), "404 page not found\n");

    fixture.teardown();
}

/// A delete answers with the `deleted` notice, removes the row, and clears the
/// redirect cache for the short code.
#[tokio::test]
async fn deleting_a_link_reports_deleted() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;
    let doomed = fixture.create(common::input(
        "console-gone",
        "https://delete-console.example/one",
    ));
    assert_eq!(doomed.slug, "console-gone", "the doomed row is stored");
    let first = fixture.get("/console-gone").await;
    assert_eq!(first.status, StatusCode::FOUND, "before delete: {first:?}");

    // The confirm page is rendered before the post.
    let confirm = fixture
        .request(
            Req::get("/manage/links/console-gone/delete")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(confirm.status, StatusCode::OK, "confirm page: {confirm:?}");
    let html = confirm.text();
    assert!(
        html.contains("id=\"confirm-form\""),
        "the confirm form marker:\n{html}"
    );

    let deleted = mutation(
        &fixture,
        "/manage/links/console-gone/delete",
        &token,
        &csrf,
        &[],
    )
    .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "delete: {deleted:?}");
    assert_eq!(
        deleted.header("location"),
        Some("http://example.com/manage/links?notice=deleted"),
        "the list is the target"
    );

    let short = fixture.get("/console-gone").await;
    assert_eq!(
        short.status,
        StatusCode::NOT_FOUND,
        "the short path is invalidated: {short:?}"
    );
    let read = fixture
        .request(Req::get("/api/links/console-gone").api_key().build())
        .await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "the row is gone: {read:?}"
    );

    let listing = fixture
        .request(
            Req::get("/manage/links?notice=deleted")
                .cookie(&token)
                .build(),
        )
        .await;
    assert!(
        listing.text().contains("Link deleted."),
        "the deleted notice on the list"
    );

    // HTMX delete answers 204 with the same instruction.
    let doomed2 = fixture.create(common::input(
        "console-gone2",
        "https://delete-console.example/two",
    ));
    assert_eq!(doomed2.slug, "console-gone2", "the second row is stored");
    let fragment = htmx_mutation(
        &fixture,
        "/manage/links/console-gone2/delete",
        &token,
        &csrf,
        &[],
    )
    .await;
    assert_eq!(
        fragment.status,
        StatusCode::NO_CONTENT,
        "htmx delete: {fragment:?}"
    );
    assert_eq!(
        fragment.header("hx-redirect"),
        Some("/manage/links?notice=deleted"),
        "the HTMX instruction"
    );

    // Deleting a code that does not exist is the plain-text 404, as Go's
    // `http.NotFound` answers (`internal/web/manage.go:186-189`).
    let again = mutation(
        &fixture,
        "/manage/links/console-gone/delete",
        &token,
        &csrf,
        &[],
    )
    .await;
    assert_eq!(
        again.status,
        StatusCode::NOT_FOUND,
        "second delete: {again:?}"
    );
    assert_eq!(again.text(), "404 page not found\n");

    fixture.teardown();
}

/// The console list honours the search box, the status filter, the sort field
/// and direction, and the cursor, with the pager link carrying the controls.
#[tokio::test]
async fn links_list_supports_the_console_controls() {
    let fixture = Fixture::new();
    let (token, _csrf) = session(&fixture).await;
    let base = 1_767_225_600_000_i64;
    let rows: Vec<Link> = (0..40_i64)
        .map(|index| {
            let slug = format!("page-row-{index:02}");
            let mut record = aged_record(
                &slug,
                &format!("https://onez.test/{slug}"),
                at_millis(base + index),
            );
            record.status = Status::Active;
            record
        })
        .collect();
    fixture
        .store()
        .import_batch(&rows, "replace")
        .expect("import the paged rows");

    // The first page shows the page size, newest first.
    let first = fixture
        .request(Req::get("/manage/links").cookie(&token).build())
        .await;
    assert_eq!(first.status, StatusCode::OK, "first page: {first:?}");
    let html = first.text();
    assert!(
        html.contains("page-row-39"),
        "the newest row is on the first page:\n{}",
        snippet(&html)
    );
    assert!(
        !html.contains("page-row-09"),
        "the oldest row is beyond the page size"
    );

    // The pager link carries the cursor and the controls, in the sorted order
    // the previous `nextURL` emitted.
    // Both template engines escape `&` inside attribute values, so decode the
    // entity before treating the href as a query string.
    let pager = html
        .split('"')
        .find(|chunk| chunk.starts_with("/manage/links?cursor="))
        .expect("the pager href carries the next cursor")
        .replace("&#38;", "&")
        .replace("&amp;", "&");
    assert!(
        pager.contains("&dir=desc") && pager.contains("&sort=updated"),
        "the pager keeps direction and sort: {pager}"
    );
    assert!(
        pager.find("cursor=").is_some()
            && pager.find("dir=").is_some()
            && pager.find("sort=").is_some()
            && pager.find("dir=") < pager.find("sort="),
        "the pairs are sorted by name: {pager}"
    );

    // Following the pager gives the rest of the workspace.
    let next = fixture
        .request(Req::get(&pager).cookie(&token).build())
        .await;
    assert_eq!(next.status, StatusCode::OK, "second page: {next:?}");
    let second_html = next.text();
    assert!(
        second_html.contains("page-row-09"),
        "the tail page lists the older rows:\n{}",
        snippet(&second_html)
    );
    assert!(
        !second_html.contains("page-row-39"),
        "the second page does not repeat the newest row"
    );

    // The search box filters the rows and is echoed back into the form.
    let searched = fixture
        .request(
            Req::get("/manage/links?q=page-row-12")
                .cookie(&token)
                .build(),
        )
        .await;
    let searched_html = searched.text();
    assert!(
        searched_html.contains("page-row-12"),
        "the search matches the slug:\n{}",
        snippet(&searched_html)
    );
    assert!(
        !searched_html.contains("page-row-13"),
        "the search excludes the rest"
    );
    assert!(
        searched_html.contains("value=\"page-row-12\"")
            || searched_html.contains("value='page-row-12'"),
        "the search field keeps the query:\n{}",
        snippet(&searched_html)
    );

    // The status filter narrows the list and is echoed in the select.
    let filtered = fixture
        .request(
            Req::get("/manage/links?status=disabled")
                .cookie(&token)
                .build(),
        )
        .await;
    let filtered_html = filtered.text();
    assert!(
        filtered_html.contains("No matching links"),
        "an all-active workspace has no disabled rows:\n{}",
        snippet(&filtered_html)
    );
    assert!(
        filtered_html.contains("selected") && filtered_html.contains("disabled"),
        "the filter select keeps the choice:\n{}",
        snippet(&filtered_html)
    );

    // A search with no match renders the empty state.
    let empty = fixture
        .request(
            Req::get("/manage/links?q=nothing-matches-this")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(empty.status, StatusCode::OK, "empty search: {empty:?}");
    assert!(
        empty.text().contains("No matching links"),
        "the empty state"
    );

    // Sorting by the short code ascending starts with the alphabetically first.
    let sorted = fixture
        .request(
            Req::get("/manage/links?sort=slug&dir=asc")
                .cookie(&token)
                .build(),
        )
        .await;
    let sorted_html = sorted.text();
    let first_row = sorted_html
        .find("page-row-")
        .expect("the sorted page lists rows");
    assert!(
        sorted_html[first_row..first_row + 20].contains("page-row-00"),
        "the ascending sort starts with the first code:\n{}",
        snippet(&sorted_html)
    );

    // An unsupported control is the store's 400 copy, as the previous console
    // surfaced list failures as plain text (`internal/web/manage.go:73-76`).
    let bad = fixture
        .request(
            Req::get("/manage/links?sort=mystery")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "bad sort: {bad:?}");
    assert_eq!(
        bad.text(),
        "Invalid list request: unsupported sort\n",
        "the console copy wraps the store error"
    );
    bad.assert_security_headers();

    // An HTMX request answers the list fragment, which carries the same marker
    // the Go surface renders.
    let fragment = fixture
        .request(Req::get("/manage/links").cookie(&token).htmx().build())
        .await;
    assert_eq!(fragment.status, StatusCode::OK, "fragment: {fragment:?}");
    let fragment_html = fragment.text();
    assert!(
        fragment_html.contains("id=\"links-region\""),
        "the fragment carries the region marker:\n{}",
        snippet(&fragment_html)
    );
    assert!(
        !fragment_html.contains("<html"),
        "the fragment is not a full document:\n{}",
        snippet(&fragment_html)
    );

    fixture.teardown();
}

/// The list page renders the Go markup markers: the region, the table, the
/// count, and the search control.
#[tokio::test]
async fn links_list_renders_the_go_markers() {
    let fixture = Fixture::new();
    let (token, _csrf) = session(&fixture).await;
    let created = fixture.create(common::input("marker-row", "https://markers.example/one"));
    assert_eq!(created.slug, "marker-row", "the marker row is stored");

    let reply = fixture
        .request(Req::get("/manage/links").cookie(&token).build())
        .await;
    assert_eq!(reply.status, StatusCode::OK, "list page: {reply:?}");
    assert_eq!(
        reply.content_type(),
        "text/html; charset=utf-8",
        "the console media type"
    );
    let html = reply.text();
    for marker in [
        "id=\"links-region\"",
        "id=\"link-count\"",
        "id=\"search\"",
        "name=\"q\"",
        "name=\"status\"",
        "name=\"sort\"",
        "name=\"dir\"",
        "class=\"data-table\"",
        "Showing ",
        "marker-row",
        "https://markers.example/one",
        "name=\"csrf\" value=\"",
    ] {
        assert!(html.contains(marker), "list marker {marker:?}:\n{html}");
    }
    assert!(
        html.contains("http://manage.example/marker-row"),
        "the short URL column uses the public base URL:\n{}",
        snippet(&html)
    );
    reply.assert_security_headers();
    reply.assert_no_cache_control();

    fixture.teardown();
}

/// The cleanup page previews the candidates, refuses an unconfirmed post with
/// 422, and answers a confirmed post with the notice carrying the deleted count.
#[tokio::test]
async fn cleanup_preview_and_confirmation() {
    let fixture = Fixture::new();
    let (token, csrf) = session(&fixture).await;
    // 2024-01-01, older than any cutoff the twelve-month preview computes.
    let base = 1_704_067_200_000_i64;
    let rows: Vec<Link> = (0..7_i64)
        .map(|index| {
            aged_record(
                &format!("old-row-{index}"),
                &format!("https://onez.test/old-{index}"),
                at_millis(base + index),
            )
        })
        .collect();
    fixture
        .store()
        .import_batch(&rows, "replace")
        .expect("import the aged rows");

    let preview = fixture
        .request(
            Req::get("/manage/cleanup?months=12&scope=all")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK, "preview: {preview:?}");
    let html = preview.text();
    for marker in [
        "Deletion preview",
        "id=\"cleanup-preview-title\"",
        "id=\"cleanup-months\"",
        "id=\"cleanup-scope\"",
        "name=\"months\"",
        "name=\"scope\"",
        "name=\"confirm\"",
        "name=\"csrf\" value=\"",
        "old-row-0",
    ] {
        assert!(html.contains(marker), "cleanup marker {marker:?}:\n{html}");
    }
    preview.assert_security_headers();

    // An unsupported scope is the plain-text 400 of the previous console.
    let bad_scope = fixture
        .request(
            Req::get("/manage/cleanup?months=12&scope=mystery")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(
        bad_scope.status,
        StatusCode::BAD_REQUEST,
        "bad scope: {bad_scope:?}"
    );
    assert_eq!(bad_scope.text(), "Invalid cleanup request.\n");

    // The same for an unsupported age.
    let bad_months = fixture
        .request(
            Req::get("/manage/cleanup?months=5&scope=all")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(
        bad_months.status,
        StatusCode::BAD_REQUEST,
        "bad months: {bad_months:?}"
    );
    assert_eq!(bad_months.text(), "Invalid cleanup request.\n");

    // A post without `confirm=yes` answers 422 with the preview and the copy.
    let unconfirmed = mutation(
        &fixture,
        "/manage/cleanup",
        &token,
        &csrf,
        &[("months", "12"), ("scope", "all")],
    )
    .await;
    assert_eq!(
        unconfirmed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unconfirmed cleanup: {unconfirmed:?}"
    );
    assert!(
        unconfirmed
            .text()
            .contains("confirm the permanent deletion before continuing"),
        "the confirmation copy (manage.go:223)"
    );
    assert!(
        unconfirmed.text().contains("Deletion preview"),
        "the preview is re-rendered with the error"
    );
    let kept = fixture
        .request(Req::get("/api/links?limit=100").api_key().build())
        .await;
    assert_eq!(
        kept.json()["links"].as_array().expect("links").len(),
        7,
        "nothing is deleted by an unconfirmed post"
    );

    // The confirmed post answers 303 with the notice and the count, and the
    // rows are gone.
    let confirmed = mutation(
        &fixture,
        "/manage/cleanup",
        &token,
        &csrf,
        &[("months", "12"), ("scope", "all"), ("confirm", "yes")],
    )
    .await;
    assert_eq!(
        confirmed.status,
        StatusCode::SEE_OTHER,
        "confirmed: {confirmed:?}"
    );
    assert_eq!(
        confirmed.header("location"),
        Some("http://example.com/manage/cleanup?months=12&scope=all&notice=cleaned&deleted=7"),
        "the redirect carries the age, the scope, the notice and the count"
    );

    let landing = fixture
        .request(
            Req::get("/manage/cleanup?months=12&scope=all&notice=cleaned&deleted=7")
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(landing.status, StatusCode::OK, "landing page: {landing:?}");
    assert!(
        landing.text().contains("7 old links deleted."),
        "the notice counts the deleted rows:\n{}",
        snippet(&landing.text())
    );

    let after = fixture
        .request(Req::get("/api/links?limit=100").api_key().build())
        .await;
    assert_eq!(
        after.json()["links"].as_array().expect("links").len(),
        0,
        "the confirmed cleanup deleted the workspace"
    );

    // The cleanup clears the redirect cache, as Go's `cache.Clear` does
    // (`internal/web/manage.go:234`).
    let short = fixture.get("/old-row-0").await;
    assert_eq!(
        short.status,
        StatusCode::NOT_FOUND,
        "the deleted short code is not cached: {short:?}"
    );

    // A cleanup that deletes nothing reports the zero count.
    let nothing = mutation(
        &fixture,
        "/manage/cleanup",
        &token,
        &csrf,
        &[("months", "3"), ("scope", "all"), ("confirm", "yes")],
    )
    .await;
    assert_eq!(
        nothing.status,
        StatusCode::SEE_OTHER,
        "empty cleanup: {nothing:?}"
    );
    assert_eq!(
        nothing.header("location"),
        Some("http://example.com/manage/cleanup?months=3&scope=all&notice=cleaned&deleted=0"),
        "the count is zero"
    );

    fixture.teardown();
}

/// A cookie whose payload does not verify is rejected: the visitor is sent to
/// the sign-in page and the cookie is cleared on the way.
#[tokio::test]
async fn forged_session_cookies_are_rejected() {
    let fixture = Fixture::new();
    let (token, _csrf) = session(&fixture).await;
    let open = fixture
        .request(Req::get("/manage/links").cookie(&token).build())
        .await;
    assert_eq!(
        open.status,
        StatusCode::OK,
        "the issued token opens the console: {open:?}"
    );

    // Flip the final character of the signature segment.
    let mut forged: Vec<String> = token.split('.').map(str::to_string).collect();
    assert!(forged.len() >= 2, "the token has a payload and a signature");
    let last = forged.len() - 1;
    let tail = forged[last].clone();
    let replacement = if tail.ends_with('a') { 'b' } else { 'a' };
    forged[last] = format!(
        "{}{replacement}",
        tail.trim_end_matches('a').trim_end_matches('b')
    );
    let forged_token = forged.join(".");
    assert_ne!(
        forged_token, token,
        "the forged token differs from the issued one"
    );

    let reply = fixture
        .request(Req::get("/manage/links").cookie(&forged_token).build())
        .await;
    assert_eq!(
        reply.status,
        StatusCode::SEE_OTHER,
        "a forged token is rejected: {reply:?}"
    );
    assert_eq!(
        reply.header("location"),
        Some("http://example.com/manage/login"),
        "the visitor is sent to sign-in"
    );
    let cookie = reply
        .header("set-cookie")
        .expect("the forged cookie is cleared");
    assert_eq!(
        cookie_attribute(cookie, "Max-Age").as_deref(),
        Some("-1"),
        "the clearing attributes are sent: {cookie}"
    );

    fixture.teardown();
}

/// The mutation limiter allows 120 actions per minute from one client address
/// and then answers the 429 plain-text copy.
#[tokio::test]
async fn console_mutations_are_rate_limited() {
    let fixture = Fixture::new();
    let peer = "198.51.100.99:42000";
    let (token, csrf) = {
        let reply = fixture
            .request(
                Req::post("/manage/login")
                    .peer(peer)
                    .form(&[("api_key", common::API_KEY)])
                    .build(),
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "login: {reply:?}");
        let issued = reply.session_cookie_value().expect("session cookie value");
        let token_for_forms = common::fixture_csrf(&fixture, &issued).await;
        (issued, token_for_forms)
    };

    // Every attempt is a delete of an absent code: the guard runs first, so the
    // limiter counts attempts, not successes.
    for attempt in 1..=120 {
        let reply = fixture
            .request(
                Req::post("/manage/links/console-absent/delete")
                    .peer(peer)
                    .cookie(&token)
                    .form(&[("csrf", &csrf)])
                    .build(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "attempt {attempt} is counted, not blocked: {reply:?}"
        );
    }

    let blocked = fixture
        .request(
            Req::post("/manage/links/console-absent/delete")
                .peer(peer)
                .cookie(&token)
                .form(&[("csrf", &csrf)])
                .build(),
        )
        .await;
    assert_eq!(
        blocked.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the 121st attempt: {blocked:?}"
    );
    assert_eq!(blocked.text(), "Too many changes. Try again shortly.\n");
    assert_eq!(
        blocked.content_type(),
        "text/plain; charset=utf-8",
        "the http.Error media type"
    );
    blocked.assert_security_headers();

    // A second client address is unaffected.
    let other = fixture
        .request(
            Req::post("/manage/links/console-absent/delete")
                .peer("198.51.100.98:42000")
                .form(&[("csrf", &csrf)])
                .cookie(&token)
                .build(),
        )
        .await;
    assert_eq!(
        other.status,
        StatusCode::NOT_FOUND,
        "the second address is not limited: {other:?}"
    );

    fixture.teardown();
}

/// A shortened excerpt of a page, for assertion messages that would otherwise
/// bury the reader in markup.
fn snippet(html: &str) -> String {
    let mut excerpt: String = html.chars().take(4_000).collect();
    excerpt.push('…');
    excerpt
}
