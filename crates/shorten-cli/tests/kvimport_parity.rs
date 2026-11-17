//! KV importer parity tests, ported from `cmd/kvimport/main_test.go`.
//!
//! The Go file tests two things: `parseRecord` over the legacy record shapes,
//! and the checksum comparison the importer uses to classify a stored record.
//! Both are ported here unchanged, plus end-to-end runs of the importer that
//! pin the report line the Go program printed and the message it exited with.
//!
//! Everything runs offline against temporary directories: no network and no
//! live server are required. The Go tests used `t.TempDir()`; [`TempPath`] is
//! the dependency-free equivalent, so the crate needs no dev-only crates.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use shorten::json;
use shorten::link::{Link, Status};
use shorten::store::{ImportOutcome, Store};
use shorten::time::now_truncated;
use shorten_cli::kvimport::{self, Options, ParsedRecord, Report};

/// Legacy fixture from the Go suite, ending with a record that must be rejected.
const FULL_EXPORT: &str = concat!(
    r#"{"key":"link:launch","value":{"url":"https://example.com/a","createdAt":"2026-08-01T10:00:00Z","visits":12,"lastVisitedAt":"2026-08-02T11:00:00Z"}}"#,
    "\n",
    r#"{"key":"config:site","value":{"theme":"dark"}}"#,
    "\n",
    r#"{"key":"link:nested","value":"{\"url\":\"https://example.com/nested\",\"createdAt\":\"2026-08-01T10:00:00Z\"}"}"#,
    "\n",
    r#"{"key":"link:bad","value":{"url":"javascript:alert(1)","createdAt":"bad"}}"#,
    "\n",
);

/// The same records without the rejected line, so a commit can succeed.
const CLEAN_EXPORT: &str = concat!(
    r#"{"key":"link:launch","value":{"url":"https://example.com/a","createdAt":"2026-08-01T10:00:00Z","visits":12,"lastVisitedAt":"2026-08-02T11:00:00Z"}}"#,
    "\n",
    r#"{"key":"config:site","value":{"theme":"dark"}}"#,
    "\n",
    r#"{"key":"link:nested","value":"{\"url\":\"https://example.com/nested\",\"createdAt\":\"2026-08-01T10:00:00Z\"}"}"#,
    "\n",
);

/// `TestParseLegacyRecords`: the object shape, the double-encoded string shape,
/// an unrelated key, and the record Go rejected.
#[test]
fn parses_legacy_records() {
    let line = r#"{"key":"link:launch","value":{"url":"https://example.com/a","createdAt":"2026-08-01T10:00:00Z","visits":12,"lastVisitedAt":"2026-08-02T11:00:00Z"}}"#;
    let candidate = match kvimport::parse_record(line) {
        ParsedRecord::Accepted(candidate) => candidate,
        other => panic!("record rejected: {other:?}"),
    };
    assert_eq!(candidate.slug, "launch");
    assert_eq!(candidate.visit_count, 12);
    assert_eq!(
        candidate.last_visited_at,
        json::parse_rfc3339("2026-08-02T11:00:00Z")
    );
    assert_eq!(candidate.destination_url, "https://example.com/a");
    assert_eq!(
        Some(json::parse_rfc3339("2026-08-01T10:00:00Z").expect("timestamp")),
        Some(candidate.created_at)
    );
    assert_eq!(candidate.status, Status::Active);
    assert_eq!(candidate.redirect_status, 302);

    let nested = r#"{"key":"link:nested","value":"{\"url\":\"https://example.com/nested\",\"createdAt\":\"2026-08-01T10:00:00Z\"}"}"#;
    let candidate = match kvimport::parse_record(nested) {
        ParsedRecord::Accepted(candidate) => candidate,
        other => panic!("nested record rejected: {other:?}"),
    };
    assert_eq!(candidate.visit_count, 0);
    assert_eq!(candidate.last_visited_at, None);
    assert_eq!(candidate.destination_url, "https://example.com/nested");

    assert_eq!(
        kvimport::parse_record(r#"{"key":"config:any","value":{}}"#),
        ParsedRecord::Ignored,
        "unrelated key must be ignored"
    );

    let malformed = kvimport::parse_record(
        r#"{"key":"link:bad","value":{"url":"javascript:alert(1)","createdAt":"bad"}}"#,
    );
    assert!(
        matches!(malformed, ParsedRecord::Rejected(_)),
        "malformed record accepted: {malformed:?}"
    );
}

/// The rejections the Go program counted as `Rejected` rather than failing.
#[test]
fn rejects_malformed_records() {
    let cases = [
        (
            r#"{"key":"link:ab","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z"}}"#,
            "link validation failed",
        ),
        (
            r#"{"key":"link:api","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z"}}"#,
            "link validation failed",
        ),
        (
            r#"{"key":"link:negative","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z","visits":-1}}"#,
            "visits must be nonnegative",
        ),
        (
            r#"{"key":"link:last","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z","lastVisitedAt":"nope"}}"#,
            "invalid lastVisitedAt",
        ),
        (
            r#"{"key":"link:nokey","value":{"url":"https://example.com","createdAt":"bad"}}"#,
            "invalid createdAt",
        ),
        ("{not json}", "column"),
    ];
    for (line, fragment) in cases {
        match kvimport::parse_record(line) {
            ParsedRecord::Rejected(message) => assert!(
                message.contains(fragment),
                "{line} reported {message}, which does not mention {fragment}"
            ),
            other => panic!("{line} produced {other:?}, expected a rejection"),
        }
    }
}

/// `TestChecksumAndVerification`: a stored record compares equal to its source
/// candidate, and a changed timestamp is detected.
#[test]
fn checksum_and_verification() {
    let directory = TempPath::new("checksum");
    let store = Store::open(&directory.file("import.db").to_string_lossy(), 2).expect("open store");

    let candidate = match kvimport::parse_record(
        r#"{"key":"link:verify","value":{"url":"https://example.com/v","createdAt":"2026-08-01T10:00:00Z","visits":5}}"#,
    ) {
        ParsedRecord::Accepted(candidate) => candidate,
        other => panic!("record rejected: {other:?}"),
    };
    let outcomes = store
        .import_batch(std::slice::from_ref(&candidate), "abort")
        .expect("import batch");
    assert_eq!(outcomes, vec![ImportOutcome::Inserted]);

    let existing = store.get_by_slug("verify").expect("read stored record");
    assert!(
        kvimport::equal_checksum(&existing, &candidate),
        "logical checksum mismatch"
    );

    let mut changed = candidate;
    changed.updated_at = now_truncated();
    assert!(
        !kvimport::equal_checksum(&existing, &changed),
        "checksum did not detect change"
    );
}

/// Dry-run classification plus the exact JSON line Go printed, in Go's field
/// order, and the exit message it followed with.
#[test]
fn dry_run_reports_every_classification() {
    let directory = TempPath::new("dryrun");
    let input = directory.file("export.ndjson");
    fs::write(&input, FULL_EXPORT).expect("write export");

    let (report, result) = run_capture(
        &input,
        &directory.file("dryrun.db"),
        false,
        "abort",
        500,
        false,
    );
    let report = report.expect("report line");
    assert_eq!(
        result.expect_err("rejected records must fail the run"),
        "migration completed with rejected or conflicting records"
    );
    assert_eq!(report.source, input.to_string_lossy());
    assert_eq!(report.mode, "dry-run");
    assert_eq!(report.conflict_policy, "abort");
    assert_eq!(report.input, 4);
    assert_eq!(report.imported, 2);
    assert_eq!(report.identical, 0);
    assert_eq!(report.ignored, 1);
    assert_eq!(report.rejected, 1);
    assert_eq!(report.conflicting, 0);
    assert_eq!(report.replaced, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.checksum.len(), 64);
    assert!(!report.verified);

    assert_eq!(
        serde_json::to_string(&report).expect("encode report"),
        format!(
            concat!(
                r#"{{"source":"{source}","mode":"dry-run","conflictPolicy":"abort","#,
                r#""Input":4,"Imported":2,"Identical":0,"Ignored":1,"Rejected":1,"Conflicting":0,"Replaced":0,"Skipped":0,"#,
                r#""checksum":"{checksum}","verified":false}}"#
            ),
            source = report.source,
            checksum = report.checksum
        ),
        "report field order must match Go's struct order"
    );
}

/// Commit mode writes accepted records in the requested batch size, verifies
/// them against the database, and a second dry run sees them as identical.
#[test]
fn commit_and_verify_round_trip() {
    let directory = TempPath::new("commit");
    let input = directory.file("export.ndjson");
    let database = directory.file("commit.db");
    fs::write(&input, CLEAN_EXPORT).expect("write export");

    let (report, result) = run_capture(&input, &database, true, "abort", 2, true);
    let report = report.expect("report line");
    result.expect("commit run must succeed");
    assert_eq!(report.mode, "commit");
    assert_eq!(report.input, 3);
    assert_eq!(report.imported, 2);
    assert_eq!(report.ignored, 1);
    assert_eq!(report.rejected, 0);
    assert!(report.verified);

    let (second, result) = run_capture(&input, &database, false, "abort", 500, false);
    let second = second.expect("report line");
    result.expect("second dry run must succeed");
    assert_eq!(second.identical, 2);
    assert_eq!(second.imported, 0);
    assert_eq!(
        second.checksum, report.checksum,
        "digest is order-independent"
    );
}

/// `skip` leaves a differing record alone and `replace` overwrites it.
#[test]
fn conflict_policies_classify_existing_records() {
    let directory = TempPath::new("policies");
    let input = directory.file("export.ndjson");
    fs::write(&input, CLEAN_EXPORT).expect("write export");

    let skipped_database = directory.file("skip.db");
    seed_existing(&skipped_database, "https://example.com/other");
    let (report, result) = run_capture(&input, &skipped_database, true, "skip", 500, false);
    let report = report.expect("report line");
    result.expect("skip run must succeed");
    assert_eq!(
        report.skipped, 2,
        "both links differ from the seeded records"
    );
    assert_eq!(report.imported, 0);

    let replaced_database = directory.file("replace.db");
    seed_existing(&replaced_database, "https://example.com/other");
    let (report, result) = run_capture(&input, &replaced_database, true, "replace", 500, true);
    let report = report.expect("report line");
    result.expect("replace run must succeed");
    assert_eq!(report.replaced, 2);
    assert_eq!(report.imported, 0);
    assert!(report.verified, "the replaced records match the source");
}

/// Under `abort`, a differing record stops the run: the dry run reports the
/// conflict, and the commit writes the report line before Go's exit message.
#[test]
fn abort_policy_reports_conflicts() {
    let directory = TempPath::new("abort");
    let input = directory.file("export.ndjson");
    fs::write(&input, CLEAN_EXPORT).expect("write export");
    let database = directory.file("abort.db");
    seed_existing(&database, "https://example.com/other");

    let (report, result) = run_capture(&input, &database, false, "abort", 500, false);
    let report = report.expect("report line");
    assert_eq!(report.conflicting, 2);
    assert_eq!(
        result.expect_err("conflicts must fail the dry run"),
        "migration completed with rejected or conflicting records"
    );

    let (report, result) = run_capture(&input, &database, true, "abort", 1, false);
    let report = report.expect("report line written before the failure");
    assert_eq!(report.mode, "commit");
    assert_eq!(report.conflicting, 1);
    assert_eq!(
        result.expect_err("commit must abort on the conflict"),
        "slug already exists: launch"
    );
}

/// The two flag rules Go applied before it opened the store.
#[test]
fn validates_flags_before_opening_the_store() {
    let base = Options {
        input: "-".to_string(),
        database: "definitely-missing.db".to_string(),
        commit: false,
        policy: "abort".to_string(),
        batch: 500,
        verify: false,
    };
    for (batch, message) in [
        (0_i64, "batch must be between 1 and 5000"),
        (5_001_i64, "batch must be between 1 and 5000"),
    ] {
        let options = Options {
            batch,
            ..base.clone()
        };
        assert_eq!(options.validate().expect_err("batch size"), message);
    }

    for policy in ["", "abort-and-retry", "SKIP"] {
        let options = Options {
            policy: policy.to_string(),
            ..base.clone()
        };
        assert_eq!(
            options.validate().expect_err("conflict policy"),
            "conflict must be abort, skip, or replace"
        );
    }
    assert!(Options {
        batch: 5_000,
        ..base
    }
    .validate()
    .is_ok());
}

/// A missing input file fails with the Go open error, before any report.
#[test]
fn reports_a_missing_input_file() {
    let directory = TempPath::new("missing");
    let (report, result) = run_capture(
        &directory.file("absent.ndjson"),
        &directory.file("missing.db"),
        false,
        "abort",
        500,
        false,
    );
    assert!(report.is_none());
    let message = result.expect_err("missing input must fail");
    assert!(
        message.starts_with("open ") && message.contains("no such file or directory"),
        "unexpected message {message}"
    );
}

/// Seed a record with the same slugs as the export but a different destination.
fn seed_existing(database: &Path, destination: &str) {
    let store = Store::open(&database.to_string_lossy(), 2).expect("open store");
    let created = json::parse_rfc3339("2026-08-01T10:00:00Z").expect("timestamp");
    let records: Vec<Link> = ["launch", "nested"]
        .iter()
        .map(|slug| Link {
            id: 0,
            slug: (*slug).to_string(),
            destination_url: destination.to_string(),
            title: String::new(),
            notes: String::new(),
            status: Status::Active,
            redirect_status: 302,
            expires_at: None,
            visit_count: 0,
            last_visited_at: None,
            created_at: created,
            updated_at: created,
        })
        .collect();
    store
        .import_batch(&records, "abort")
        .expect("seed existing records");
}

/// Run the importer, decode the report line it wrote, and return both it and
/// the exit message Go would have printed.
fn run_capture(
    input: &Path,
    database: &Path,
    commit: bool,
    policy: &str,
    batch: i64,
    verify: bool,
) -> (Option<Report>, Result<(), String>) {
    let options = Options {
        input: input.to_string_lossy().into_owned(),
        database: database.to_string_lossy().into_owned(),
        commit,
        policy: policy.to_string(),
        batch,
        verify,
    };
    options.validate().expect("valid options");
    let mut written: Vec<u8> = Vec::new();
    let result = kvimport::run(&options, &mut written);
    let text = String::from_utf8(written).expect("report line is UTF-8");
    let report = if text.is_empty() {
        None
    } else {
        assert!(text.ends_with('\n'), "Go's encoder terminated the line");
        Some(serde_json::from_str(text.trim_end()).expect("decode report"))
    };
    (report, result)
}

/// A unique directory under the system temp dir, removed when the test ends.
struct TempPath {
    path: PathBuf,
}

impl TempPath {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "shorten-cli-kvimport-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temporary directory");
        Self { path }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
