//! Parity of the JSON API under `/api`.
//!
//! Reference: Go `internal/web/api.go` (the `X-API-Key` gate, the 415 content
//! type gate, the 16 KiB body cap, the representation map, the list envelope
//! with its keyset cursor, the partial `PATCH` decoder and the error mapping),
//! `internal/link/link.go` (validation messages) and
//! `internal/web/server.go:166-175` (`writeAPIError`).

mod common;

use axum::http::{Method, StatusCode};
use common::{
    aged_record, at_millis, base64url_decode, base_url_of, go_time, input, json_keys_at,
    json_top_level_keys, Fixture, Req,
};
use serde_json::Value;
use shorten::link::{Link, Status};

/// The representation keys of a link, alphabetically sorted as the Go
/// `map[string]any` encoder writes them.
const REPRESENTATION_KEYS: [&str; 14] = [
    "createdAt",
    "destination",
    "expiresAt",
    "id",
    "lastVisitedAt",
    "notes",
    "redirectStatus",
    "shortUrl",
    "slug",
    "status",
    "title",
    "updatedAt",
    "url",
    "visits",
];

/// `GET /api/links/{slug}` with the administrator key.
async fn api_get(fixture: &Fixture, slug: &str) -> common::Reply {
    fixture
        .request(Req::get(&format!("/api/links/{slug}")).api_key().build())
        .await
}

/// Every endpoint of the API answers 401 with the same JSON envelope when the
/// `X-API-Key` header is missing or wrong, and 2xx when it matches.
#[tokio::test]
async fn missing_or_wrong_api_key_is_unauthorized() {
    let fixture = Fixture::new();
    let created = fixture.create(input("keygate-link", "https://keygate.example/target"));
    assert_eq!(created.slug, "keygate-link", "seed link stored");

    let cases: [(Method, &str, Option<&str>); 6] = [
        (Method::GET, "/api/links", None),
        (Method::GET, "/api/links", Some("the-wrong-admin-key")),
        (Method::POST, "/api/shorten", None),
        (Method::GET, "/api/links/keygate-link", Some("")),
        (Method::PATCH, "/api/links/keygate-link", None),
        (
            Method::DELETE,
            "/api/links/keygate-link",
            Some("nope-nope-nope-nope"),
        ),
    ];
    for (method, path, supplied) in cases {
        let builder = match supplied {
            Some(value) => Req::of(method.as_str(), path).header("x-api-key", value),
            None => Req::of(method.as_str(), path),
        };
        let reply = fixture.request(builder.build()).await;
        assert_eq!(
            reply.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} with key {supplied:?}: {reply:?}"
        );
        assert_eq!(reply.json_line(), "{\"error\":\"Invalid API key.\"}\n");
        assert_eq!(
            reply.content_type(),
            "application/json; charset=utf-8",
            "writeJSON media type of {method} {path}"
        );
        reply.assert_security_headers();
        reply.assert_no_cache_control();
    }

    // The same header value accepted by the constant-time comparison opens
    // the very same routes.
    let listed = fixture
        .request(Req::get("/api/links").api_key().build())
        .await;
    assert_eq!(listed.status, StatusCode::OK, "authorized list: {listed:?}");
    let read = api_get(&fixture, "keygate-link").await;
    assert_eq!(read.status, StatusCode::OK, "authorized get: {read:?}");

    fixture.teardown();
}

/// A create with a non-JSON content type is refused with 415, matching the
/// previous `net/http` behaviour of writing the envelope with that status.
#[tokio::test]
async fn non_json_content_type_is_unsupported_media_type() {
    let fixture = Fixture::new();

    for content_type in [
        "text/plain; charset=utf-8",
        "application/x-www-form-urlencoded",
        "",
    ] {
        let reply = fixture
            .request(
                Req::post("/api/shorten")
                    .api_key()
                    .json_with_type("{\"url\":\"https://ct.example/target\"}", content_type)
                    .build(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content type {content_type:?}: {reply:?}"
        );
        assert_eq!(
            reply.json_line(),
            "{\"error\":\"Content-Type must be application/json.\"}\n",
            "envelope of content type {content_type:?}"
        );
        reply.assert_security_headers();
    }

    // A parameterised JSON media type passes the prefix test.
    let accepted = fixture
        .request(
            Req::post("/api/shorten")
                .api_key()
                .json_with_type(
                    "{\"url\":\"https://ct.example/target\",\"slug\":\"ct-accepted\"}",
                    "application/json; charset=utf-8",
                )
                .build(),
        )
        .await;
    assert_eq!(
        accepted.status,
        StatusCode::CREATED,
        "application/json with parameters: {accepted:?}"
    );

    fixture.teardown();
}

/// A valid create answers 201 with the full representation: keys in Go's
/// alphabetical map order, the derived `shortUrl`, null optional fields, and a
/// trailing newline.
#[tokio::test]
async fn create_answers_201_with_the_full_representation() {
    let fixture = Fixture::new();
    let base_url = base_url_of(&fixture);

    let reply = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://create.example/target?x=1#frag\",\"slug\":\"created-one\
             \",\"title\":\"Created one\",\"notes\":\"private note\",\"status\":\"active\",\
             \"redirectStatus\":301}",
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "create: {reply:?}");
    assert_eq!(
        reply.content_type(),
        "application/json; charset=utf-8",
        "writeJSON media type"
    );
    reply.assert_security_headers();
    reply.assert_no_cache_control();

    let body = reply.json_line();
    assert!(
        body.ends_with('\n'),
        "JSON bodies end with a newline: {body}"
    );
    assert_eq!(
        json_top_level_keys(&body),
        REPRESENTATION_KEYS.to_vec(),
        "the Go encoder sorts map keys"
    );

    let value = reply.json();
    assert_eq!(value["slug"], Value::from("created-one"));
    assert_eq!(value["id"], Value::from(1), "the first stored row");
    assert_eq!(
        value["shortUrl"],
        Value::from(format!("{base_url}/created-one")),
        "shortUrl is built from the public base URL"
    );
    assert_eq!(
        value["url"],
        Value::from("https://create.example/target?x=1#frag")
    );
    assert_eq!(
        value["destination"], value["url"],
        "url is duplicated per Go"
    );
    assert_eq!(value["title"], Value::from("Created one"));
    assert_eq!(value["notes"], Value::from("private note"));
    assert_eq!(value["status"], Value::from("active"));
    assert_eq!(value["redirectStatus"], Value::from(301));
    assert_eq!(value["visits"], Value::from(0));
    assert_eq!(value["expiresAt"], Value::Null, "absent expiry is null");
    assert_eq!(value["lastVisitedAt"], Value::Null, "no visit yet");
    assert_eq!(
        value["createdAt"].as_str().expect("createdAt is a string"),
        value["updatedAt"].as_str().expect("updatedAt is a string"),
        "a fresh row has matching stamps"
    );

    fixture.teardown();
}

/// Omitting `slug` gets the generated code, omitting the optional fields gets
/// the stored defaults: active status and 302.
#[tokio::test]
async fn create_defaults_the_slug_status_and_redirect() {
    let fixture = Fixture::new();

    let reply = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://defaults.example/target\"}",
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "create: {reply:?}");
    let value = reply.json();
    let slug = value["slug"].as_str().expect("generated slug").to_string();
    assert_eq!(
        slug.len(),
        8,
        "six random bytes are eight base64url characters: {slug}"
    );
    assert!(
        slug.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
        "the generated code is base64url: {slug}"
    );
    assert_eq!(
        value["status"],
        Value::from("active"),
        "Go defaults the status"
    );
    assert_eq!(
        value["redirectStatus"],
        Value::from(302),
        "Go defaults the redirect"
    );
    assert_eq!(value["title"], Value::from(""));
    assert_eq!(value["notes"], Value::from(""));
    assert_eq!(value["expiresAt"], Value::Null);

    // The generated code redirects, which proves it is stored and served.
    let redirect = fixture.get(&format!("/{slug}")).await;
    assert_eq!(
        redirect.status,
        StatusCode::FOUND,
        "generated slug: {redirect:?}"
    );
    assert_eq!(
        redirect.header("location"),
        Some("https://defaults.example/target"),
        "generated slug destination"
    );

    fixture.teardown();
}

/// Timestamps use Go's `time.Time` JSON shape: RFC 3339 in UTC, the fractional
/// part only when it is non-zero and with its trailing zeros trimmed.
#[tokio::test]
async fn timestamps_are_rfc3339nano_with_trimmed_zeros() {
    let fixture = Fixture::new();
    let base = 1_767_225_600_000_i64; // 2026-01-01T00:00:00Z
    let cases = [
        ("stamp-zero", base, "2026-01-01T00:00:00Z"),
        ("stamp-one", base + 1, "2026-01-01T00:00:00.001Z"),
        ("stamp-twof", base + 12, "2026-01-01T00:00:00.012Z"),
        ("stamp-drop", base + 100, "2026-01-01T00:00:00.1Z"),
        ("stamp-sev7", base + 123, "2026-01-01T00:00:00.123Z"),
    ];
    let expiry = at_millis(1_780_272_000_000); // 2026-06-01T00:00:00Z
    let rows: Vec<Link> = cases
        .iter()
        .map(|(slug, millis, _)| {
            let mut record = aged_record(
                slug,
                &format!("https://stamp.example/{slug}"),
                at_millis(*millis),
            );
            record.expires_at = Some(expiry);
            record
        })
        .collect();
    fixture
        .store()
        .import_batch(&rows, "replace")
        .expect("import the timestamp rows");

    for (slug, millis, expected) in cases {
        let reply = api_get(&fixture, slug).await;
        assert_eq!(reply.status, StatusCode::OK, "{slug}: {reply:?}");
        let value = reply.json();
        assert_eq!(
            value["createdAt"].as_str().expect("createdAt"),
            expected,
            "createdAt of {slug} stored at {millis} ms"
        );
        assert_eq!(
            value["updatedAt"].as_str().expect("updatedAt"),
            expected,
            "updatedAt of {slug}"
        );
        assert_eq!(
            value["expiresAt"].as_str().expect("expiresAt"),
            "2026-06-01T00:00:00Z",
            "the imported expiry is whole seconds"
        );
    }

    // A fractional `expiresAt` in the request is stored at millisecond
    // resolution and printed with its trailing zeros trimmed.
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://stamp.example/target\",\"slug\":\"stamp-api\",\"expiresAt\":\
             \"2026-06-01T00:00:00.123456700Z\"}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");
    assert_eq!(
        created.json()["expiresAt"].as_str().expect("expiresAt"),
        "2026-06-01T00:00:00.123Z",
        "the fraction is truncated to the stored milliseconds"
    );

    // Round-trip check against the shared formatter for the same instants.
    assert_eq!(go_time(at_millis(base + 123)), "2026-01-01T00:00:00.123Z");
    assert_eq!(go_time(at_millis(base)), "2026-01-01T00:00:00Z");

    fixture.teardown();
}

/// Validation failures are the 400 envelope with the `fields` object keyed by
/// form field, carrying the exact Go validation copy.
#[tokio::test]
async fn validation_failure_reports_the_go_field_messages() {
    let fixture = Fixture::new();

    let cases: [(&str, &str, &str); 6] = [
        (
            "empty destination",
            "{\"url\":\"\"}",
            "Enter an HTTP or HTTPS destination up to 8,192 characters.",
        ),
        (
            "relative destination",
            "{\"url\":\"not-a-url\"}",
            "Enter a complete URL starting with http:// or https://.",
        ),
        (
            "ftp destination",
            "{\"url\":\"ftp://example.invalid/path\"}",
            "Enter a complete URL starting with http:// or https://.",
        ),
        (
            "short slug",
            "{\"url\":\"https://ok.example/target\",\"slug\":\"ab\"}",
            "Use 3–64 letters, numbers, hyphens, or underscores.",
        ),
        (
            "reserved slug",
            "{\"url\":\"https://ok.example/target\",\"slug\":\"api\"}",
            "This short code is reserved by the application.",
        ),
        (
            "bad status",
            "{\"url\":\"https://ok.example/target\",\"status\":\"paused\"}",
            "Choose active or disabled.",
        ),
    ];
    for (label, body, message) in cases {
        let reply = fixture.post_json("/api/shorten", body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{label}: {reply:?}");
        let wire = reply.json_line();
        let parsed = reply.json();
        assert_eq!(
            parsed["error"],
            Value::from("Validation failed."),
            "{label} envelope: {wire}"
        );
        assert_eq!(
            parsed["fields"]["destination"]
                .as_str()
                .or(parsed["fields"]["slug"].as_str())
                .or(parsed["fields"]["status"].as_str())
                .expect("one field message"),
            message,
            "{label} message: {wire}"
        );
        reply.assert_security_headers();
    }

    // Field keys are sorted alphabetically by the map encoder: `destination`
    // before `slug` in a body that fails both.
    let both = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"not-a-url\",\"slug\":\"ab\",\"title\":\"x\",\"notes\":\"y\"}",
        )
        .await;
    let wire = both.json_line();
    assert_eq!(both.status, StatusCode::BAD_REQUEST, "{both:?}");
    assert_eq!(
        json_keys_at(&wire, 2),
        vec!["destination".to_string(), "slug".to_string()],
        "sorted field keys: {wire}"
    );

    fixture.teardown();
}

/// A duplicate custom slug is the 409 conflict copy.
#[tokio::test]
async fn duplicate_slug_is_a_conflict() {
    let fixture = Fixture::new();
    let first = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://dupe.example/first\",\"slug\":\"dupe-slug\"}",
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "first create: {first:?}");

    let second = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://dupe.example/second\",\"slug\":\"dupe-slug\"}",
        )
        .await;
    assert_eq!(second.status, StatusCode::CONFLICT, "duplicate: {second:?}");
    assert_eq!(
        second.json_line(),
        "{\"error\":\"This custom slug is already in use. Please choose another one.\"}\n"
    );
    second.assert_security_headers();
    second.assert_no_cache_control();

    // The stored row keeps the first destination.
    let stored = api_get(&fixture, "dupe-slug").await;
    assert_eq!(
        stored.json()["url"],
        Value::from("https://dupe.example/first"),
        "the first row survives"
    );

    fixture.teardown();
}

/// The list envelope is `{links, cursor, total}` in Go's alphabetical map
/// order, with the keyset cursor as a base64url string whose payload keeps the
/// Go field order and version.
#[tokio::test]
async fn list_envelope_shape_and_cursor() {
    let fixture = Fixture::new();
    let base = 1_767_225_600_000_i64;
    let rows: Vec<Link> = (0..12_i64)
        .map(|index| {
            aged_record(
                &format!("list-row-{index:02}"),
                &format!("https://onez.test/list-row-{index:02}"),
                at_millis(base + index),
            )
        })
        .collect();
    fixture
        .store()
        .import_batch(&rows, "replace")
        .expect("import list rows");

    let reply = fixture
        .request(Req::get("/api/links?limit=5").api_key().build())
        .await;
    assert_eq!(reply.status, StatusCode::OK, "list: {reply:?}");
    let wire = reply.json_line();
    assert_eq!(
        json_top_level_keys(&wire),
        vec![
            "cursor".to_string(),
            "links".to_string(),
            "total".to_string()
        ],
        "the Go encoder sorts the envelope keys: {wire}"
    );
    let value = reply.json();
    let links = value["links"].as_array().expect("links array");
    assert_eq!(links.len(), 5, "limit=5 page: {wire}");
    assert_eq!(
        value["total"],
        Value::from(12),
        "total counts the workspace"
    );
    assert_eq!(
        json_keys_at(
            &serde_json::to_string(&links[0]).expect("encode one row"),
            1
        ),
        REPRESENTATION_KEYS.to_vec(),
        "each row is the full representation"
    );

    // The cursor is the base64url payload of the Go `cursor` struct.
    let cursor = value["cursor"].as_str().expect("a next cursor").to_string();
    let decoded = base64url_decode(&cursor).expect("cursor is base64url");
    let payload = String::from_utf8(decoded).expect("cursor payload is UTF-8");
    assert_eq!(
        json_top_level_keys(&payload),
        vec![
            "v".to_string(),
            "sort".to_string(),
            "dir".to_string(),
            "n".to_string(),
            "id".to_string()
        ],
        "Go cursor field order, numeric sort: {payload}"
    );
    let parsed: Value = serde_json::from_str(&payload).expect("cursor payload is JSON");
    assert_eq!(parsed["v"], Value::from(1), "cursor version");
    assert_eq!(parsed["sort"], Value::from("updated"), "default sort");
    assert_eq!(parsed["dir"], Value::from("desc"), "default direction");
    assert_eq!(
        parsed["n"],
        Value::from(base + 7),
        "the cursor key is the fifth row's updated stamp"
    );

    // Walking the cursor gives the rest of the page.
    let next = fixture
        .request(
            Req::get(&format!("/api/links?limit=5&cursor={cursor}"))
                .api_key()
                .build(),
        )
        .await;
    assert_eq!(next.status, StatusCode::OK, "second page: {next:?}");
    let next_value = next.json();
    assert_eq!(
        next_value["links"].as_array().expect("links").len(),
        5,
        "the second page is full"
    );
    let third = fixture
        .request(
            Req::get(&format!(
                "/api/links?limit=5&cursor={}",
                next_value["cursor"].as_str().expect("second cursor")
            ))
            .api_key()
            .build(),
        )
        .await;
    let third_value = third.json();
    assert_eq!(
        third_value["links"].as_array().expect("links").len(),
        2,
        "the tail page"
    );
    assert_eq!(
        third_value["cursor"],
        Value::Null,
        "the last page carries a null cursor"
    );

    fixture.teardown();
}

/// The list limit defaults to 30, the previous default, and 100 is the cap; 0
/// falls back to the default and 101 is the store's own 400.
#[tokio::test]
async fn list_limit_defaults_and_caps() {
    let fixture = Fixture::new();
    let base = 1_767_225_600_000_i64;
    let rows: Vec<Link> = (0..35_i64)
        .map(|index| {
            aged_record(
                &format!("cap-row-{index:02}"),
                &format!("https://onez.test/cap-row-{index:02}"),
                at_millis(base + index),
            )
        })
        .collect();
    fixture
        .store()
        .import_batch(&rows, "replace")
        .expect("import 35 rows");

    let defaulted = fixture
        .request(Req::get("/api/links").api_key().build())
        .await;
    assert_eq!(
        defaulted.status,
        StatusCode::OK,
        "default page: {defaulted:?}"
    );
    assert_eq!(
        defaulted.json()["links"].as_array().expect("links").len(),
        30,
        "the Go default limit is 30 (api.go:76)"
    );

    let zero = fixture
        .request(Req::get("/api/links?limit=0").api_key().build())
        .await;
    assert_eq!(zero.status, StatusCode::OK, "limit=0: {zero:?}");
    assert_eq!(
        zero.json()["links"].as_array().expect("links").len(),
        30,
        "limit<1 falls back to the default (store.go:350)"
    );

    let capped = fixture
        .request(Req::get("/api/links?limit=100").api_key().build())
        .await;
    assert_eq!(capped.status, StatusCode::OK, "limit=100: {capped:?}");
    assert_eq!(
        capped.json()["links"].as_array().expect("links").len(),
        35,
        "the cap admits the whole workspace"
    );

    let over = fixture
        .request(Req::get("/api/links?limit=101").api_key().build())
        .await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST, "limit=101: {over:?}");
    assert_eq!(
        over.json_line(),
        "{\"error\":\"limit must be between 1 and 100\"}\n",
        "the store's own copy (store.go:353)"
    );

    let junk = fixture
        .request(Req::get("/api/links?limit=many").api_key().build())
        .await;
    assert_eq!(junk.status, StatusCode::BAD_REQUEST, "limit=many: {junk:?}");
    assert_eq!(junk.json_line(), "{\"error\":\"Invalid limit.\"}\n");

    fixture.teardown();
}

/// A filtered page reports `total: null`, because the previous service only
/// counted the unfiltered workspace.
#[tokio::test]
async fn list_total_is_null_while_filtering() {
    let fixture = Fixture::new();
    let base = 1_767_225_600_000_i64;
    let mut rows: Vec<Link> = (0..6_i64)
        .map(|index| {
            let mut record = aged_record(
                &format!("filter-row-{index}"),
                &format!("https://onez.test/filter-row-{index}"),
                at_millis(base + index),
            );
            // `aged_record` stores the disabled status; this page needs an
            // active workspace with exactly one disabled row.
            record.status = Status::Active;
            record
        })
        .collect();
    let mut off = aged_record(
        "filter-row-off",
        "https://onez.test/filter-row-off",
        at_millis(base + 90),
    );
    off.status = Status::Disabled;
    rows.push(off);
    fixture
        .store()
        .import_batch(&rows, "replace")
        .expect("import filter rows");

    let plain = fixture
        .request(Req::get("/api/links").api_key().build())
        .await;
    assert_eq!(plain.status, StatusCode::OK, "plain page: {plain:?}");
    assert_eq!(plain.json()["total"], Value::from(7), "unfiltered total");

    let filtered = fixture
        .request(Req::get("/api/links?status=disabled").api_key().build())
        .await;
    assert_eq!(
        filtered.status,
        StatusCode::OK,
        "filtered page: {filtered:?}"
    );
    assert_eq!(filtered.json()["total"], Value::Null, "filtered total");
    assert_eq!(
        filtered.json()["links"].as_array().expect("links").len(),
        1,
        "the disabled row is listed"
    );

    let searched = fixture
        .request(Req::get("/api/links?q=filter").api_key().build())
        .await;
    assert_eq!(searched.status, StatusCode::OK, "search page: {searched:?}");
    assert_eq!(searched.json()["total"], Value::Null, "search total");

    let bad_filter = fixture
        .request(Req::get("/api/links?status=somewhat").api_key().build())
        .await;
    assert_eq!(
        bad_filter.status,
        StatusCode::BAD_REQUEST,
        "unknown filter: {bad_filter:?}"
    );
    assert_eq!(
        bad_filter.json_line(),
        "{\"error\":\"unsupported status filter\"}\n",
        "store copy"
    );

    let bad_sort = fixture
        .request(Req::get("/api/links?sort=mystery").api_key().build())
        .await;
    assert_eq!(
        bad_sort.status,
        StatusCode::BAD_REQUEST,
        "unknown sort: {bad_sort:?}"
    );
    assert_eq!(
        bad_sort.json_line(),
        "{\"error\":\"unsupported sort\"}\n",
        "store copy, store.go:656"
    );

    let bad_dir = fixture
        .request(Req::get("/api/links?dir=sideways").api_key().build())
        .await;
    assert_eq!(
        bad_dir.status,
        StatusCode::BAD_REQUEST,
        "unknown direction: {bad_dir:?}"
    );
    assert_eq!(
        bad_dir.json_line(),
        "{\"error\":\"sort direction must be asc or desc\"}\n",
        "store copy"
    );

    let bad_cursor = fixture
        .request(Req::get("/api/links?cursor=not-a-cursor").api_key().build())
        .await;
    assert_eq!(
        bad_cursor.status,
        StatusCode::BAD_REQUEST,
        "bad cursor: {bad_cursor:?}"
    );
    assert_eq!(bad_cursor.json_line(), "{\"error\":\"invalid cursor\"}\n");

    fixture.teardown();
}

/// `GET /api/links/{slug}` answers the representation with the same key order
/// as a create, and 404 with the Go copy for an absent code.
#[tokio::test]
async fn get_answers_the_same_representation_as_create() {
    let fixture = Fixture::new();
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://shape.example/target\",\"slug\":\"shape-read\",\"title\":\"Shaped\",\
             \"expiresAt\":\"2027-01-01T00:00:00Z\"}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");

    let read = api_get(&fixture, "shape-read").await;
    assert_eq!(read.status, StatusCode::OK, "get: {read:?}");
    let create_wire = created.json_line();
    let read_wire = read.json_line();
    assert_eq!(
        json_top_level_keys(&read_wire),
        json_top_level_keys(&create_wire),
        "identical key order between create and read"
    );
    assert_eq!(
        json_top_level_keys(&read_wire),
        REPRESENTATION_KEYS.to_vec(),
        "alphabetical Go map order: {read_wire}"
    );
    assert_eq!(
        read.json()["expiresAt"],
        Value::from("2027-01-01T00:00:00Z"),
        "expiry round-trips"
    );
    read.assert_security_headers();
    read.assert_no_cache_control();

    let absent = api_get(&fixture, "shape-absent").await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND, "absent: {absent:?}");
    assert_eq!(absent.json_line(), "{\"error\":\"Link not found.\"}\n");

    let malformed = api_get(&fixture, "ab").await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "a code that cannot exist is absent: {malformed:?}"
    );
    assert_eq!(malformed.json_line(), "{\"error\":\"Link not found.\"}\n");

    fixture.teardown();
}

/// `PATCH` keeps every field the body does not mention, ignores unknown ones,
/// and clears the nullable fields with `null`.
#[tokio::test]
async fn patch_updates_only_the_supplied_fields() {
    let fixture = Fixture::new();
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://patch.example/before\",\"slug\":\"patch-me\",\"title\":\"Before\",\
             \"notes\":\"keep me\",\"status\":\"active\",\"redirectStatus\":301,\
             \"expiresAt\":\"2027-06-01T00:00:00Z\"}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");
    let before = created.json();

    // A title-only patch keeps the destination, notes, status, redirect type,
    // expiry and the short code.
    let patched = fixture
        .request(
            Req::patch("/api/links/patch-me")
                .api_key()
                .json("{\"title\":\"After\",\"unknownField\":true,\"expiresAt\":null}")
                .build(),
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "patch: {patched:?}");
    let after = patched.json();
    assert_eq!(after["title"], Value::from("After"), "the patched field");
    assert_eq!(after["slug"], before["slug"], "slug kept");
    assert_eq!(after["url"], before["url"], "destination kept");
    assert_eq!(after["notes"], before["notes"], "notes kept");
    assert_eq!(after["status"], before["status"], "status kept");
    assert_eq!(
        after["redirectStatus"], before["redirectStatus"],
        "redirect kept"
    );
    assert_eq!(after["expiresAt"], Value::Null, "null clears the expiry");
    assert_eq!(after["shortUrl"], before["shortUrl"], "shortUrl kept");

    // The patch wrote through to the store and the redirect reflects it.
    let read = api_get(&fixture, "patch-me").await;
    assert_eq!(
        read.json()["title"],
        Value::from("After"),
        "the store row is patched"
    );
    let redirect = fixture.get("/patch-me").await;
    assert_eq!(
        redirect.header("location"),
        before["url"].as_str(),
        "the cached redirect is invalidated and re-read"
    );

    // Go decoded a string field with `json.Unmarshal(raw, &input.Title)`
    // (`internal/web/api.go:141-150`), and Go treats a JSON `null` as "leave
    // the target untouched", so `{"title":null,"notes":null}` answers 200 with
    // both values unchanged and only the fields the body named are written. The
    // Rust decoder mirrors that (`crates/shorten-web/src/api.rs`): a null on a
    // string field is skipped, and `expiresAt` keeps its explicit null branch
    // (`api.go:163-165`, cleared above).
    let cleared = fixture
        .request(
            Req::patch("/api/links/patch-me")
                .api_key()
                .json("{\"title\":null,\"notes\":null}")
                .build(),
        )
        .await;
    assert_eq!(
        cleared.status,
        StatusCode::OK,
        "Go accepts a null string field as a no-op: {cleared:?}"
    );
    assert_eq!(
        cleared.json()["title"],
        Value::from("After"),
        "the null leaves the title Go would have left"
    );
    assert_eq!(
        cleared.json()["notes"],
        before["notes"],
        "the null leaves the notes untouched"
    );
    let after = api_get(&fixture, "patch-me").await;
    assert_eq!(
        after.json()["title"],
        Value::from("After"),
        "the store row keeps the title"
    );
    assert_eq!(
        after.json()["notes"],
        before["notes"],
        "notes untouched by the null patch"
    );

    fixture.teardown();
}

/// A `PATCH` can move a short code to a new slug, invalidating both cache keys.
#[tokio::test]
async fn patch_can_rename_the_short_code() {
    let fixture = Fixture::new();
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://rename.example/target\",\"slug\":\"rename-before\"}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");
    let first = fixture.get("/rename-before").await;
    assert_eq!(first.status, StatusCode::FOUND, "before rename: {first:?}");

    let renamed = fixture
        .request(
            Req::patch("/api/links/rename-before")
                .api_key()
                .json("{\"slug\":\"rename-after\"}")
                .build(),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "rename: {renamed:?}");
    assert_eq!(renamed.json()["slug"], Value::from("rename-after"));
    assert_eq!(
        renamed.json()["shortUrl"],
        Value::from(format!("{}/rename-after", base_url_of(&fixture))),
        "shortUrl follows the new code"
    );

    let gone = fixture.get("/rename-before").await;
    assert_eq!(
        gone.status,
        StatusCode::NOT_FOUND,
        "the old code is invalidated, not cached: {gone:?}"
    );
    let moved = fixture.get("/rename-after").await;
    assert_eq!(
        moved.status,
        StatusCode::FOUND,
        "the new code redirects: {moved:?}"
    );

    // Renaming onto a code that already exists is the conflict copy.
    let clash = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://rename.example/clash\",\"slug\":\"rename-clash\"}",
        )
        .await;
    assert_eq!(
        clash.status,
        StatusCode::CREATED,
        "second create: {clash:?}"
    );
    let blocked = fixture
        .request(
            Req::patch("/api/links/rename-clash")
                .api_key()
                .json("{\"slug\":\"rename-after\"}")
                .build(),
        )
        .await;
    assert_eq!(
        blocked.status,
        StatusCode::CONFLICT,
        "rename onto a taken code: {blocked:?}"
    );
    assert_eq!(
        blocked.json_line(),
        "{\"error\":\"This custom slug is already in use. Please choose another one.\"}\n"
    );

    fixture.teardown();
}

/// A `PATCH` with no fields is a no-op update, and the expiry copy is shared
/// between create and patch.
#[tokio::test]
async fn patch_edge_cases_use_the_go_copies() {
    let fixture = Fixture::new();
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://edge.example/target\",\"slug\":\"edge-me\",\"title\":\"Edge\",\
             \"redirectStatus\":307}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");

    let noop = fixture
        .request(
            Req::patch("/api/links/edge-me")
                .api_key()
                .json("{}")
                .build(),
        )
        .await;
    assert_eq!(noop.status, StatusCode::OK, "empty patch: {noop:?}");
    assert_eq!(noop.json()["title"], Value::from("Edge"), "fields survive");
    assert_eq!(
        noop.json()["redirectStatus"],
        Value::from(307),
        "the redirect type survives"
    );

    // Go parses the same RFC 3339 requirement in both endpoints, with the
    // `parseAPIExpiration` copy on create and the patch decoder's own copy.
    let bad_create = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://edge.example/x\",\"expiresAt\":\"last tuesday\"}",
        )
        .await;
    assert_eq!(
        bad_create.status,
        StatusCode::BAD_REQUEST,
        "bad create expiry: {bad_create:?}"
    );
    assert_eq!(
        bad_create.json_line(),
        "{\"error\":\"expiresAt must be an RFC3339 timestamp\"}\n",
        "api.go:233"
    );
    let bad_patch = fixture
        .request(
            Req::patch("/api/links/edge-me")
                .api_key()
                .json("{\"expiresAt\":\"last tuesday\"}")
                .build(),
        )
        .await;
    assert_eq!(
        bad_patch.status,
        StatusCode::BAD_REQUEST,
        "bad patch expiry: {bad_patch:?}"
    );
    assert_eq!(
        bad_patch.json_line(),
        "{\"error\":\"expiresAt must be an RFC3339 string or null.\"}\n"
    );

    // Go's decoder rejects a non-integral redirect status with its own copy,
    // matching `json: cannot unmarshal number into Go value of type int`.
    for body in [
        "{\"redirectStatus\":\"302\"}",
        "{\"redirectStatus\":302.5}",
        "{\"redirectStatus\":true}",
    ] {
        let reply = fixture
            .request(
                Req::patch("/api/links/edge-me")
                    .api_key()
                    .json(body)
                    .build(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "patch body {body}: {reply:?}"
        );
        assert_eq!(
            reply.json_line(),
            "{\"error\":\"redirectStatus must be a number.\"}\n",
            "patch body {body}"
        );
    }

    // Go accepts `302.0`: `json.Unmarshal` converts an integral float to int.
    let integral = fixture
        .request(
            Req::patch("/api/links/edge-me")
                .api_key()
                .json("{\"redirectStatus\":301.0}")
                .build(),
        )
        .await;
    assert_eq!(
        integral.status,
        StatusCode::OK,
        "301.0 is an int in Go: {integral:?}"
    );
    assert_eq!(integral.json()["redirectStatus"], Value::from(301));

    fixture.teardown();
}

/// `DELETE` answers 204 with no body, then the code is gone for every reader.
#[tokio::test]
async fn delete_answers_no_content_then_not_found() {
    let fixture = Fixture::new();
    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://delete.example/target\",\"slug\":\"delete-me\"}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");
    let redirect = fixture.get("/delete-me").await;
    assert_eq!(
        redirect.status,
        StatusCode::FOUND,
        "before delete: {redirect:?}"
    );

    let deleted = fixture
        .request(Req::delete("/api/links/delete-me").api_key().build())
        .await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "delete: {deleted:?}"
    );
    assert!(deleted.body.is_empty(), "204 has no body: {deleted:?}");
    assert!(
        deleted.header("content-type").is_none(),
        "204 declares no media type: {:?}",
        deleted.headers
    );
    deleted.assert_security_headers();
    deleted.assert_no_cache_control();

    let read = api_get(&fixture, "delete-me").await;
    assert_eq!(
        read.status,
        StatusCode::NOT_FOUND,
        "get after delete: {read:?}"
    );
    assert_eq!(read.json_line(), "{\"error\":\"Link not found.\"}\n");

    // The redirect cache is invalidated, so the short path is 404 too.
    let short = fixture.get("/delete-me").await;
    assert_eq!(
        short.status,
        StatusCode::NOT_FOUND,
        "redirect after delete: {short:?}"
    );
    assert_eq!(short.text(), "404 page not found\n", "plain-text 404 body");

    // Deleting twice is the same 404 envelope.
    let again = fixture
        .request(Req::delete("/api/links/delete-me").api_key().build())
        .await;
    assert_eq!(
        again.status,
        StatusCode::NOT_FOUND,
        "second delete: {again:?}"
    );
    assert_eq!(again.json_line(), "{\"error\":\"Link not found.\"}\n");

    let body = fixture.metrics_body().await;
    assert!(
        body.contains("shorten_link_mutations_total{operation=\"delete\"} 1\n"),
        "the delete mutation is counted:\n{body}"
    );

    fixture.teardown();
}

/// The body cap is 16 KiB on both API endpoints that read a body. Go caps it
/// with `http.MaxBytesReader(w, r.Body, 16<<10)` (`internal/web/api.go:41` for
/// the create, `:122` for the patch), so the cap surfaces as a JSON decode
/// failure and the answer is the 400 `The request body must be valid JSON.`
/// envelope (`api.go:52` and `:125`): a 413 never reaches the client. The Rust
/// handlers pass the same 16 KiB to `read_body` and map its error to the same
/// envelope (`crates/shorten-web/src/api.rs:24,60,229`).
#[tokio::test]
async fn oversized_bodies_answer_invalid_json_not_payload_too_large() {
    let fixture = Fixture::new();
    // `padding` is an unknown field: both decoders ignore it, so it pads the
    // body to the cap without tripping the 4,000-character note limit.
    let url = "\"https://big.example/target\"";
    let prefix = format!("{{\"url\":{url},\"padding\":\"");
    let suffix = "\"}";

    // Exactly at the cap: the Go reader allows the limit, so the create works.
    let at_cap = 16 << 10;
    let filler = "a".repeat(at_cap - prefix.len() - suffix.len());
    let exact = format!("{prefix}{filler}{suffix}");
    assert_eq!(exact.len(), at_cap, "the probe body is exactly the cap");
    let accepted = fixture.post_json("/api/shorten", &exact).await;
    assert_eq!(
        accepted.status,
        StatusCode::CREATED,
        "body of exactly 16 KiB: {accepted:?}"
    );

    // One byte over: 400 with the invalid-JSON copy, and no 413 anywhere.
    let over = format!("{prefix}{filler}{suffix}a");
    assert_eq!(over.len(), at_cap + 1, "the probe body is one over the cap");
    let created = fixture.post_json("/api/shorten", &over).await;
    assert_eq!(
        created.status,
        StatusCode::BAD_REQUEST,
        "create over the cap: {created:?}"
    );
    assert_eq!(
        created.json_line(),
        "{\"error\":\"The request body must be valid JSON.\"}\n",
        "the Go copy (api.go:52)"
    );
    assert_ne!(
        created.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "Go never answers 413 on the API"
    );
    assert_ne!(
        created.content_type(),
        "text/plain; charset=utf-8",
        "not the plain-text 413 body of `read_body`: {created:?}"
    );
    created.assert_security_headers();

    // An oversized `PATCH` answers the same envelope (`api.go:122-125`).
    let seeded = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://big.example/target\",\"slug\":\"big-patch\"}",
        )
        .await;
    assert_eq!(seeded.status, StatusCode::CREATED, "seed: {seeded:?}");
    let patched = fixture
        .request(
            Req::patch("/api/links/big-patch")
                .api_key()
                .json(&over)
                .build(),
        )
        .await;
    assert_eq!(
        patched.status,
        StatusCode::BAD_REQUEST,
        "patch over the cap: {patched:?}"
    );
    assert_eq!(
        patched.json_line(),
        "{\"error\":\"The request body must be valid JSON.\"}\n",
        "the Go copy (api.go:125)"
    );

    fixture.teardown();
}

/// Malformed JSON, a JSON scalar, and a JSON array all take the same 400
/// envelope: the previous service never reported a parse detail.
#[tokio::test]
async fn malformed_bodies_answer_the_invalid_json_envelope() {
    let fixture = Fixture::new();
    let seed = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://shape2.example/target\",\"slug\":\"shape2-link\"}",
        )
        .await;
    assert_eq!(seed.status, StatusCode::CREATED, "seed: {seed:?}");

    let bodies = [
        "{\"url\": ",
        "not json at all",
        "[]",
        "\"a string\"",
        "42",
        "{\"url\":\"https://ok.example/x\",}",
    ];
    for body in bodies {
        let created = fixture.post_json("/api/shorten", body).await;
        assert_eq!(
            created.status,
            StatusCode::BAD_REQUEST,
            "create body {body:?}: {created:?}"
        );
        assert_eq!(
            created.json_line(),
            "{\"error\":\"The request body must be valid JSON.\"}\n",
            "create body {body:?}"
        );

        let patched = fixture
            .request(
                Req::patch("/api/links/shape2-link")
                    .api_key()
                    .json(body)
                    .build(),
            )
            .await;
        assert_eq!(
            patched.status,
            StatusCode::BAD_REQUEST,
            "patch body {body:?}: {patched:?}"
        );
        assert_eq!(
            patched.json_line(),
            "{\"error\":\"The request body must be valid JSON.\"}\n",
            "patch body {body:?}"
        );
    }

    // A top-level `null` is a valid JSON value that Go's decoder accepted for a
    // struct target, leaving every field zero (`internal/web/api.go:42-50`), so
    // the create continued into validation. The Rust decoder does the same
    // (`crates/shorten-web/src/api.rs`): the body is read as an empty object
    // and the answer is the validation envelope, not the parse-failure one.
    let null_body = fixture.post_json("/api/shorten", "null").await;
    assert_eq!(
        null_body.status,
        StatusCode::BAD_REQUEST,
        "null body: {null_body:?}"
    );
    assert_eq!(
        null_body.json_line(),
        "{\"error\":\"Validation failed.\",\"fields\":{\"destination\":\"Enter an HTTP or HTTPS destination up to 8,192 characters.\"}}\n",
        "Go answers the validation envelope for a null body"
    );

    fixture.teardown();
}

/// No API response is cacheable, and every JSON body ends with the encoder's
/// newline, including the empty 204 which has no body at all.
#[tokio::test]
async fn api_bodies_end_with_a_newline_and_are_not_cacheable() {
    let fixture = Fixture::new();

    let created = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://cache.example/target\",\"slug\":\"cache-me\"}",
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "create: {created:?}");
    created.assert_no_cache_control();
    assert!(
        created.text().ends_with('\n'),
        "create body newline: {:?}",
        created.text()
    );

    for (method, path) in [
        ("GET", "/api/links"),
        ("GET", "/api/links/cache-me"),
        ("GET", "/api/links/absent-9"),
        ("POST", "/api/shorten"),
    ] {
        let reply = fixture
            .request(Req::of(method, path).api_key().build())
            .await;
        reply.assert_no_cache_control();
        if reply.status == StatusCode::NO_CONTENT {
            continue;
        }
        assert_eq!(
            reply.content_type(),
            "application/json; charset=utf-8",
            "{method} {path}: {reply:?}"
        );
        assert!(
            reply.text().ends_with('\n'),
            "{method} {path} body newline: {:?}",
            reply.text()
        );
    }

    let deleted = fixture
        .request(Req::delete("/api/links/cache-me").api_key().build())
        .await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "delete: {deleted:?}"
    );
    assert!(deleted.body.is_empty(), "204 has no body to newline");
    deleted.assert_no_cache_control();

    fixture.teardown();
}

/// `expiresAt` is parsed by Go's `time.Parse(time.RFC3339, …)`, whose layout is
/// `2006-01-02T15:04:05Z07:00`: a `T` separator and a zone that is exactly `Z`
/// or `±hh:mm`. Fractional seconds are accepted and truncated to the stored
/// milliseconds. Reference: `internal/web/api.go:229-236` (create) and
/// `internal/web/api.go:163-178` (patch); implemented by
/// `shorten::json::parse_rfc3339` and used by both API endpoints
/// (`crates/shorten-web/src/api.rs:411` and `:279`).
#[tokio::test]
async fn expires_at_must_be_a_strict_rfc3339_timestamp() {
    let fixture = Fixture::new();

    let accepted: [(&str, &str, &str); 3] = [
        (
            "rfc-plain-z",
            "2030-01-01T00:00:00Z",
            "2030-01-01T00:00:00Z",
        ),
        (
            "rfc-colon-offset",
            "2030-01-01T00:00:00+07:00",
            // Normalised to UTC, as Go's `time.Time` marshals.
            "2029-12-31T17:00:00Z",
        ),
        (
            "rfc-fraction",
            "2030-01-01T00:00:00.250Z",
            // `RFC3339Nano` trims trailing zeros of the fraction.
            "2030-01-01T00:00:00.25Z",
        ),
    ];
    for (slug, supplied, expected) in accepted {
        let body = format!(
            "{{\"url\":\"https://rfc.example/target\",\"slug\":\"{slug}\",\"expiresAt\":\
             \"{supplied}\"}}"
        );
        let reply = fixture.post_json("/api/shorten", &body).await;
        assert_eq!(
            reply.status,
            StatusCode::CREATED,
            "create with expiresAt {supplied:?}: {reply:?}"
        );
        assert_eq!(
            reply.json()["expiresAt"].as_str().expect("expiresAt"),
            expected,
            "stored expiry of {supplied:?}"
        );
    }

    let rejected = [
        // The compact offset is not RFC 3339.
        "2030-01-01T00:00:00+0700",
        // A space instead of the `T` separator.
        "2030-01-01 00:00:00Z",
        // A lower-case `z` zone marker.
        "2030-01-01T00:00:00z",
        // A single-digit offset hour.
        "2030-01-01T00:00:00+7:00",
        // Unpadded date parts.
        "2030-1-1T00:00:00Z",
        // A date without a zone.
        "2030-01-01T00:00:00",
        // Not a timestamp at all.
        "next tuesday",
    ];
    for supplied in rejected {
        let body = format!(
            "{{\"url\":\"https://rfc.example/target\",\"slug\":\"rfc-reject\",\
             \"expiresAt\":\"{supplied}\"}}"
        );
        let reply = fixture.post_json("/api/shorten", &body).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "create with expiresAt {supplied:?}: {reply:?}"
        );
        assert_eq!(
            reply.json_line(),
            "{\"error\":\"expiresAt must be an RFC3339 timestamp\"}\n",
            "create copy for {supplied:?} (api.go:233)"
        );
    }

    // The patch endpoint shares the parser but keeps its own copy.
    let seeded = fixture
        .post_json(
            "/api/shorten",
            "{\"url\":\"https://rfc.example/target\",\"slug\":\"rfc-patch\"}",
        )
        .await;
    assert_eq!(seeded.status, StatusCode::CREATED, "seed: {seeded:?}");
    for supplied in [
        "2030-01-01T00:00:00+0700",
        "2030-01-01 00:00:00Z",
        "2030-01-01T00:00:00z",
    ] {
        let reply = fixture
            .request(
                Req::patch("/api/links/rfc-patch")
                    .api_key()
                    .json(&format!("{{\"expiresAt\":\"{supplied}\"}}"))
                    .build(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "patch with expiresAt {supplied:?}: {reply:?}"
        );
        assert_eq!(
            reply.json_line(),
            "{\"error\":\"expiresAt must be an RFC3339 string or null.\"}\n",
            "patch copy for {supplied:?} (api.go:168, api.go:173)"
        );
    }

    let moved = fixture
        .request(
            Req::patch("/api/links/rfc-patch")
                .api_key()
                .json("{\"expiresAt\":\"2030-01-01T12:00:00-05:00\"}")
                .build(),
        )
        .await;
    assert_eq!(
        moved.status,
        StatusCode::OK,
        "patch with a colon offset: {moved:?}"
    );
    assert_eq!(
        moved.json()["expiresAt"].as_str().expect("expiresAt"),
        "2030-01-01T17:00:00Z",
        "the patched expiry is normalised to UTC"
    );

    fixture.teardown();
}
