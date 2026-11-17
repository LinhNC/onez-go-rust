//! Store behaviour tests, ported from the Go service's SQLite suite.
//!
//! These cover create/read/rename, keyset pagination, cleanup scope rules,
//! import conflict policies and the index usage the benchmark depends on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use shorten::analytics::VisitDelta;
use shorten::link::{CreateInput, DomainError, Link, Status};
use shorten::store::{ImportOutcome, ListOptions, Store};

fn open_store(name: &str) -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path: PathBuf = dir.path().join(format!("{name}.db"));
    let store = Store::open(path.to_str().expect("utf8 path"), 4).expect("open store");
    (store, dir)
}

fn input(slug: &str, destination: &str) -> CreateInput {
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

fn fixture(slug: &str, destination: &str, status: Status, created: DateTime<Utc>) -> Link {
    Link {
        id: 0,
        slug: slug.to_string(),
        destination_url: destination.to_string(),
        title: String::new(),
        notes: String::new(),
        status,
        redirect_status: 302,
        expires_at: None,
        visit_count: 0,
        last_visited_at: None,
        created_at: created,
        updated_at: created,
    }
}

fn options<'a>(
    search: &'a str,
    sort: &'a str,
    dir: &'a str,
    cursor: &'a str,
    limit: i64,
    now: DateTime<Utc>,
) -> ListOptions<'a> {
    ListOptions {
        search,
        filter: "",
        sort,
        dir,
        cursor,
        limit,
        now: Some(now),
    }
}

#[test]
fn crud_rename_and_analytics() {
    let (store, _dir) = open_store("crud");
    let created = store
        .create(input("launch", "https://example.com/one"))
        .expect("create");
    assert_eq!(created.visit_count, 0);
    assert_eq!(created.slug, "launch");

    let mut rename = input("release", "https://example.com/two");
    rename.title = "Release".to_string();
    rename.status = Some(Status::Disabled);
    rename.redirect_status = 308;
    let updated = store.update("launch", rename).expect("update");
    assert_eq!(updated.id, created.id);
    assert_eq!(updated.visit_count, 0);
    assert_eq!(updated.slug, "release");

    assert!(matches!(
        store.get_by_slug("launch"),
        Err(DomainError::NotFound)
    ));

    let mut deltas = HashMap::new();
    deltas.insert(
        created.id,
        VisitDelta {
            count: 3,
            latest: Utc::now(),
        },
    );
    store.apply_visits(&deltas).expect("apply visits");

    let visited = store.get_by_slug("release").expect("read visited");
    assert_eq!(visited.visit_count, 3);
    assert!(visited.last_visited_at.is_some());

    store.delete("release").expect("delete");
    assert!(matches!(
        store.get_by_slug("release"),
        Err(DomainError::NotFound)
    ));
}

#[test]
fn list_search_filter_and_cursor() {
    let (store, _dir) = open_store("list");
    for (slug, title, status) in [
        ("alpha", "Alpha docs", Status::Active),
        ("beta", "Beta launch", Status::Disabled),
        ("gamma", "Gamma docs", Status::Active),
    ] {
        let mut create = input(slug, &format!("https://example.com/{slug}"));
        create.title = title.to_string();
        create.status = Some(status);
        store.create(create).expect("create");
        std::thread::sleep(Duration::from_millis(2));
    }

    let now = Utc::now();
    let first = store
        .list(options("docs", "slug", "asc", "", 1, now))
        .expect("first page");
    assert_eq!(first.links.len(), 1);
    assert_ne!(first.next_cursor, String::new());
    assert!(!first.total_known);
    assert_eq!(first.total, -1);

    let second = store
        .list(options("docs", "slug", "asc", &first.next_cursor, 1, now))
        .expect("second page");
    assert_eq!(second.links.len(), 1);
    assert_ne!(second.links[0].slug, first.links[0].slug);

    let disabled = store
        .list(ListOptions {
            filter: "disabled",
            limit: 30,
            ..options("", "updated", "desc", "", 30, now)
        })
        .expect("disabled filter");
    assert_eq!(disabled.links.len(), 1);
    assert_eq!(disabled.links[0].slug, "beta");

    let all = store
        .list(options("", "updated", "desc", "", 30, now))
        .expect("all links");
    assert!(all.total_known);
    assert_eq!(all.total, 3);
}

#[test]
fn conflict_is_atomic() {
    let (store, _dir) = open_store("conflict");
    store
        .create(input("same", "https://example.com"))
        .expect("first create");
    let error = store
        .create(input("same", "https://other.example"))
        .expect_err("duplicate slug");
    assert!(error.is_conflict());
    // The rejected insert left exactly one row behind.
    let summary = store.summary().expect("summary");
    assert_eq!(summary.links, 1);
}

#[test]
fn preview_and_delete_old_links() {
    let (store, _dir) = open_store("cleanup");
    let now = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
    let old = now - chrono::Duration::days(210);
    let recent = now - chrono::Duration::days(30);
    let expired = now - chrono::Duration::hours(1);

    let mut links = vec![
        fixture(
            "old-disabled",
            "https://example.com/disabled",
            Status::Disabled,
            old,
        ),
        fixture(
            "old-expired",
            "https://example.com/expired",
            Status::Active,
            old,
        ),
        fixture(
            "old-active",
            "https://example.com/active",
            Status::Active,
            old,
        ),
        fixture(
            "recent-disabled",
            "https://example.com/recent",
            Status::Disabled,
            recent,
        ),
    ];
    links[1].expires_at = Some(expired);
    store.import_batch(&links, "abort").expect("import");

    let cutoff = now - chrono::Duration::days(182);
    let preview = store
        .preview_cleanup(cutoff, now, "inactive", 10)
        .expect("preview");
    assert_eq!(preview.count, 2);
    assert_eq!(preview.links.len(), 2);

    let deleted = store
        .delete_before(cutoff, now, "inactive")
        .expect("delete");
    assert_eq!(deleted, 2);
    assert!(store.get_by_slug("old-active").is_ok());

    links.retain(|link| link.slug == "old-active");
    let all = store
        .preview_cleanup(cutoff, now, "all", 10)
        .expect("all scope");
    assert_eq!(all.count, 1);
    assert_eq!(all.links[0].slug, "old-active");
}

#[test]
fn import_batch_policies_and_idempotence() {
    let (store, _dir) = open_store("import");
    let created = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut original = fixture(
        "legacy",
        "https://example.com/legacy",
        Status::Active,
        created,
    );
    original.visit_count = 42;

    let outcomes = store
        .import_batch(std::slice::from_ref(&original), "abort")
        .expect("insert");
    assert_eq!(outcomes, vec![ImportOutcome::Inserted]);

    let outcomes = store
        .import_batch(std::slice::from_ref(&original), "abort")
        .expect("idempotent");
    assert_eq!(outcomes, vec![ImportOutcome::Identical]);

    let mut replacement = original.clone();
    replacement.destination_url = "https://example.com/new".to_string();
    let expected = replacement.clone();

    let error = store
        .import_batch(std::slice::from_ref(&replacement), "abort")
        .expect_err("abort conflict");
    assert!(error.is_conflict());

    let outcomes = store
        .import_batch(std::slice::from_ref(&replacement), "skip")
        .expect("skip");
    assert_eq!(outcomes, vec![ImportOutcome::Skipped]);

    let outcomes = store
        .import_batch(std::slice::from_ref(&replacement), "replace")
        .expect("replace");
    assert_eq!(outcomes, vec![ImportOutcome::Replaced]);

    let got = store.get_by_slug("legacy").expect("read legacy");
    assert_eq!(got.destination_url, expected.destination_url);
    assert_eq!(got.visit_count, 42);
}

#[test]
fn supported_list_sorts_page_and_use_indexes() {
    let (store, _dir) = open_store("plans");
    for (index, slug) in ["delta", "alpha", "charlie", "bravo"].iter().enumerate() {
        let created = store
            .create(input(slug, &format!("https://example.com/{slug}")))
            .expect("create");
        let mut deltas = HashMap::new();
        deltas.insert(
            created.id,
            VisitDelta {
                count: i64::try_from(index + 1).expect("small"),
                latest: Utc::now(),
            },
        );
        store.apply_visits(&deltas).expect("apply visits");
        std::thread::sleep(Duration::from_millis(2));
    }

    let now = Utc::now();
    for sort_name in ["created", "updated", "visits", "destination", "slug"] {
        let first = store
            .list(options("", sort_name, "desc", "", 2, now))
            .expect("sort page");
        assert_eq!(first.links.len(), 2, "sort {sort_name}");
        assert!(!first.next_cursor.is_empty(), "sort {sort_name} cursor");
        let second = store
            .list(options("", sort_name, "desc", &first.next_cursor, 2, now))
            .expect("sort second page");
        assert_eq!(second.links.len(), 2, "sort {sort_name} second");
    }

    let plan = store
        .explain_query_plan("SELECT id FROM links ORDER BY updated_at DESC, id DESC LIMIT 30")
        .expect("explain");
    assert!(
        plan.iter()
            .any(|line| line.contains("links_updated_id_idx")),
        "plan: {plan:?}"
    );

    let plan = store
        .explain_query_plan(
            "SELECT id FROM links WHERE status='active' AND (expires_at IS NULL OR expires_at > 99999999999999) ORDER BY updated_at DESC, id DESC LIMIT 30",
        )
        .expect("explain active");
    assert!(
        plan.iter()
            .any(|line| line.contains("links_status_updated_id_idx")),
        "plan: {plan:?}"
    );
}

#[test]
fn migrations_report_current_schema_and_summary() {
    let (store, _dir) = open_store("schema");
    assert_eq!(store.schema_version().expect("version"), 3);
    store.ping().expect("ping");
    store.integrity_check().expect("integrity");
    let summary = store.summary().expect("summary");
    assert_eq!(summary.links, 0);
    assert_eq!(summary.visits, 0);
    assert_eq!(summary.schema_version, 3);

    let backup = tempfile::tempdir().expect("backup dir");
    let destination = backup.path().join("copy.db");
    let destination = destination.to_str().expect("utf8").to_string();
    store.backup(&destination).expect("backup");
    let copy = Store::open(&destination, 1).expect("open copy");
    copy.integrity_check().expect("copy integrity");
    assert_eq!(copy.summary().expect("copy summary").schema_version, 3);
    store.checkpoint().expect("checkpoint");
}

#[test]
fn rejects_bad_list_requests() {
    let (store, _dir) = open_store("rejects");
    let now = Utc::now();
    assert!(store
        .list(options("", "nonsense", "desc", "", 30, now))
        .is_err());
    assert!(store
        .list(ListOptions {
            filter: "nonsense",
            ..options("", "updated", "desc", "", 30, now)
        })
        .is_err());
    assert!(store
        .list(ListOptions {
            limit: 500,
            ..options("", "updated", "desc", "", 500, now)
        })
        .is_err());
    assert!(store
        .list(ListOptions {
            cursor: "not-base64",
            ..options("", "updated", "desc", "not-base64", 30, now)
        })
        .is_err());
    assert!(store.preview_cleanup(now, now, "nonsense", 10).is_err());
}
