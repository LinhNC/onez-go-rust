//! The KV importer core, ported from `cmd/kvimport`.
//!
//! Reads the NDJSON key/value export produced by the legacy worker, converts
//! every `link:` record into a current [`shorten::link::Link`] and classifies
//! it against the live database. The report on standard output is the same
//! single JSON line the Go program printed, in the same field order, and every
//! `json.Encoder.Encode` call ends with its newline.
//!
//! Record decoding follows `encoding/json`'s rules, which the legacy export
//! depends on: field names match case-insensitively (so `Key`/`key` and
//! `URL`/`url` are the same field), a missing `key` is the empty string, a
//! missing `value` is a decode failure, `"value": null` decodes to an empty
//! payload, and a string payload is unwrapped one level.
//!
//! Flags are parsed by [`crate::cli`]; this module holds the classification and
//! the report so they can be tested without a process.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use shorten::json;
use shorten::link::{self, CreateInput, Link, Status};
use shorten::store::{ImportOutcome, Store};

use crate::cli::go_escaped_json;

/// `bufio.Scanner`'s limit from the Go program: `scanner.Buffer(make([]byte,
/// 64*1024), 2<<20)`. The limit applies to the token, without its newline.
const MAX_LINE_BYTES: usize = 2 << 20;

/// Reader connections the importer opens the store with
/// (`storesqlite.Open(ctx, path, 4)`).
const READ_CONNECTIONS: usize = 4;

/// `sha256.Size` from the Go program.
const CHECKSUM_BYTES: usize = 32;

/// What one source line produced, matching the Go `(link, bool, error)` triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedRecord {
    /// A usable record for the importer to classify.
    Accepted(Link),
    /// A record that is not a `link:` key: counted as ignored.
    Ignored,
    /// A malformed record: counted as rejected, with the reason kept for tests.
    Rejected(String),
}

/// Flags accepted by the importer, mirroring Go's `flag` defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// NDJSON path, or `-` for standard input.
    pub input: String,
    pub database: String,
    pub commit: bool,
    pub policy: String,
    pub batch: i64,
    pub verify: bool,
}

impl Options {
    /// The two validation rules Go applied before opening the store.
    ///
    /// # Errors
    ///
    /// Returns the same messages the Go program printed, which exit with
    /// status 1.
    pub fn validate(&self) -> Result<(), String> {
        if self.batch < 1 || self.batch > 5_000 {
            return Err("batch must be between 1 and 5000".to_string());
        }
        if !matches!(self.policy.as_str(), "abort" | "skip" | "replace") {
            return Err("conflict must be abort, skip, or replace".to_string());
        }
        Ok(())
    }
}

/// The JSON report line, declared in Go's field order so the encoding matches.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    #[serde(rename = "source")]
    pub source: String,
    #[serde(rename = "mode")]
    pub mode: String,
    #[serde(rename = "conflictPolicy")]
    pub conflict_policy: String,
    #[serde(rename = "Input")]
    pub input: i64,
    #[serde(rename = "Imported")]
    pub imported: i64,
    #[serde(rename = "Identical")]
    pub identical: i64,
    #[serde(rename = "Ignored")]
    pub ignored: i64,
    #[serde(rename = "Rejected")]
    pub rejected: i64,
    #[serde(rename = "Conflicting")]
    pub conflicting: i64,
    #[serde(rename = "Replaced")]
    pub replaced: i64,
    #[serde(rename = "Skipped")]
    pub skipped: i64,
    #[serde(rename = "checksum")]
    pub checksum: String,
    #[serde(rename = "verified")]
    pub verified: bool,
}

/// Run the importer and write the report line to `stdout`.
///
/// # Errors
///
/// Returns the message Go printed on standard error before exiting with
/// status 1.
pub fn run(options: &Options, stdout: &mut dyn Write) -> Result<(), String> {
    let store = Store::open(&options.database, READ_CONNECTIONS).map_err(message)?;
    let digest = Rc::new(RefCell::new([0_u8; CHECKSUM_BYTES]));
    let mut report = Report {
        source: options.input.clone(),
        mode: if options.commit { "commit" } else { "dry-run" }.to_string(),
        conflict_policy: options.policy.clone(),
        ..Report::default()
    };
    let batch_size = usize::try_from(options.batch).unwrap_or(1);
    let mut batch: Vec<Link> = Vec::with_capacity(batch_size);

    let mut reader = open_input(&options.input)?;
    while let Some(line) = read_line(&mut reader)? {
        report.input += 1;
        match parse_record(&line) {
            ParsedRecord::Ignored => report.ignored += 1,
            ParsedRecord::Rejected(_) => report.rejected += 1,
            ParsedRecord::Accepted(candidate) => {
                xor_digest(&mut digest.borrow_mut(), &checksum(&candidate));
                if options.commit {
                    batch.push(candidate);
                    if batch.len() >= batch_size {
                        flush(&store, &mut batch, options, &mut report, &digest, stdout)?;
                    }
                } else {
                    match store.inspect_import(&candidate) {
                        Ok(ImportOutcome::Identical) => report.identical += 1,
                        Ok(_) => report.imported += 1,
                        Err(error) if error.is_conflict() => report.conflicting += 1,
                        Err(error) => return Err(message(error)),
                    }
                }
            }
        }
    }
    if options.commit && !batch.is_empty() {
        flush(&store, &mut batch, options, &mut report, &digest, stdout)?;
    }

    report.checksum = hex_digest(&digest.borrow());
    if options.verify {
        if options.input == "-" {
            return Err("verification requires a seekable input file".to_string());
        }
        let (verified, failure) = verify_source(&store, &options.input);
        report.verified = verified;
        if let Some(message) = failure {
            write_report(stdout, &report)?;
            return Err(message);
        }
    }
    write_report(stdout, &report)?;
    if report.rejected > 0 || report.conflicting > 0 {
        return Err("migration completed with rejected or conflicting records".to_string());
    }
    Ok(())
}

/// Run the importer against the process standard output.
///
/// # Errors
///
/// Returns the message to print on standard error for exit status 1.
pub fn execute(options: &Options) -> Result<(), String> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    run(options, &mut handle)
}

/// Classify and write one full batch, exactly like Go's `flush` closure: a
/// conflict is counted, the report is printed with the checksum so far, and the
/// error is returned.
fn flush(
    store: &Store,
    batch: &mut Vec<Link>,
    options: &Options,
    report: &mut Report,
    digest: &Rc<RefCell<[u8; 32]>>,
    stdout: &mut dyn Write,
) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    let outcome = store.import_batch(batch, &options.policy).map_err(|error| {
        if error.is_conflict() {
            report.conflicting += 1;
        }
        report.checksum = hex_digest(&digest.borrow());
        let _result = write_report(stdout, report);
        message(error)
    });
    match outcome {
        Ok(outcomes) => {
            for outcome in outcomes {
                match outcome {
                    ImportOutcome::Inserted => report.imported += 1,
                    ImportOutcome::Identical => report.identical += 1,
                    ImportOutcome::Replaced => report.replaced += 1,
                    ImportOutcome::Skipped => report.skipped += 1,
                }
            }
            batch.clear();
            Ok(())
        }
        Err(reason) => Err(reason),
    }
}

/// `bufio.Scanner`-compatible line reader: the token drops its trailing newline
/// and a trailing carriage return, a token longer than the Go scanner's limit is
/// the scanner's own error, and undecodable bytes are kept as replacement
/// characters so the line is rejected by the JSON decoder as Go rejected it.
fn read_line<R: BufRead + ?Sized>(reader: &mut R) -> Result<Option<String>, String> {
    let mut bytes: Vec<u8> = Vec::new();
    match reader.read_until(b'\n', &mut bytes) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(error) => return Err(format!("read input: {error}")),
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_LINE_BYTES {
        return Err("scanner: token too long".to_string());
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// `openInput`: standard input for `-`, otherwise the named file, whose failure
/// Go reported as `open <path>: <cause>`.
fn open_input(path: &str) -> Result<Box<dyn BufRead>, String> {
    if path == "-" {
        return Ok(Box::new(BufReader::new(io::stdin())));
    }
    match File::open(path) {
        Ok(file) => Ok(Box::new(BufReader::new(file))),
        Err(error) => Err(format!("open {path}: {}", go_io_message(&error))),
    }
}

/// The cause text Go's `os.Open` printed for the failures an import hits:
/// `*fs.PathError` formatted its cause with the syscall message, which is what
/// Rust reports without the path prefix.
fn go_io_message(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "no such file or directory".to_string(),
        io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        io::ErrorKind::IsADirectory => "is a directory".to_string(),
        io::ErrorKind::FileTooLarge => "file too large".to_string(),
        _ => error.to_string(),
    }
}

/// `encoding/json`'s decoder message for a syntax failure. Go quoted the
/// offending character (`invalid character 'o' in literal null (expecting 'u')`);
/// Rust's decoder reports the position instead, which is kept verbatim.
fn json_syntax_message(line: &str) -> String {
    serde_json::from_str::<Value>(line)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| "invalid character in JSON".to_string())
}

/// `json.NewEncoder(os.Stdout).Encode(result)`: one compact line plus a newline.
fn write_report(stdout: &mut dyn Write, report: &Report) -> Result<(), String> {
    let encoded = go_escaped_json(&serde_json::to_string(report).map_err(message)?);
    stdout
        .write_all(encoded.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .map_err(message)
}

/// `parseRecord`: decode one source line into a candidate link.
///
/// Legacy keys arrive as `"link:<slug>"`; a string payload is unwrapped one
/// level, matching the double-encoded exports the Go importer accepted.
///
/// # Errors
///
/// The reason the record was rejected, which Go counted without printing.
pub fn parse_record(line: &str) -> ParsedRecord {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        // Go reported `encoding/json`'s own decoder message, which Rust reports
        // with a line/column position instead of Go's character quote.
        return ParsedRecord::Rejected(json_syntax_message(line));
    };
    let Value::Object(object) = &record else {
        return ParsedRecord::Rejected(
            "json: cannot unmarshal non-object into Go value of type main.sourceRecord".to_string(),
        );
    };
    // `key` is a Go `string`; absent means the empty string, and any other JSON
    // type is the decode error Go reported.
    let key = match go_field(object, "key") {
        None => String::new(),
        Some(Value::String(key)) => key.clone(),
        Some(_) => {
            return ParsedRecord::Rejected(
                "json: cannot unmarshal value into Go struct field sourceRecord.key of type string"
                    .to_string(),
            );
        }
    };
    if !key.starts_with("link:") {
        return ParsedRecord::Ignored;
    }
    let Some(slug) = key.strip_prefix("link:") else {
        return ParsedRecord::Ignored;
    };
    let payload = match decode_payload(go_field(object, "value")) {
        Ok(payload) => payload,
        Err(reason) => return ParsedRecord::Rejected(reason),
    };
    let Value::Object(legacy) = &payload else {
        return ParsedRecord::Rejected(
            "json: cannot unmarshal non-object into Go value of type main.legacyLink".to_string(),
        );
    };
    let url = match go_field(legacy, "url") {
        None => String::new(),
        Some(Value::String(url)) => url.clone(),
        Some(_) => {
            return ParsedRecord::Rejected(
                "json: cannot unmarshal value into Go struct field legacyLink.url of type string"
                    .to_string(),
            );
        }
    };
    let created_at = match go_field(legacy, "createdAt") {
        None => String::new(),
        Some(Value::String(created)) => created.clone(),
        Some(_) => {
            return ParsedRecord::Rejected(
                "json: cannot unmarshal value into Go struct field legacyLink.createdAt of type string"
                    .to_string(),
            );
        }
    };
    let visits = match go_field(legacy, "visits") {
        None | Some(Value::Null) => 0,
        Some(Value::Number(number)) => match number.as_i64() {
            Some(visits) => visits,
            None => {
                return ParsedRecord::Rejected(format!(
                    "{key}: cannot unmarshal number into Go struct field legacyLink.visits of type int64"
                ))
            }
        },
        Some(_) => {
            return ParsedRecord::Rejected(
                "json: cannot unmarshal value into Go struct field legacyLink.visits of type int64"
                    .to_string(),
            );
        }
    };
    let last_visited_at = match go_field(legacy, "lastVisitedAt") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return ParsedRecord::Rejected(
                "json: cannot unmarshal value into Go struct field legacyLink.lastVisitedAt of type string"
                    .to_string(),
            );
        }
    };

    let Some(created) = json::parse_rfc3339(&created_at) else {
        return ParsedRecord::Rejected(format!("{key}: invalid createdAt"));
    };
    if visits < 0 {
        return ParsedRecord::Rejected(format!("{key}: visits must be nonnegative"));
    }
    let last_visited_at = match last_visited_at {
        None => None,
        Some(text) => match json::parse_rfc3339(&text) {
            Some(value) => Some(value),
            None => return ParsedRecord::Rejected(format!("{key}: invalid lastVisitedAt")),
        },
    };

    let mut input = CreateInput {
        slug: slug.to_string(),
        destination_url: url,
        title: String::new(),
        notes: String::new(),
        status: Some(Status::Active),
        redirect_status: 302,
        expires_at: None,
    };
    if let Err(error) = link::validate_create(&mut input) {
        return ParsedRecord::Rejected(message(error));
    }
    ParsedRecord::Accepted(Link {
        id: 0,
        slug: slug.to_string(),
        destination_url: input.destination_url,
        title: String::new(),
        notes: String::new(),
        status: Status::Active,
        redirect_status: 302,
        expires_at: None,
        visit_count: visits,
        last_visited_at,
        created_at: created,
        updated_at: created,
    })
}

/// Unwrap the `value` field the way Go's `json.Unmarshal` did.
///
/// A missing field is an empty `json.RawMessage`, which decodes as Go's
/// "unexpected end of JSON input". A string payload is unwrapped one level, as
/// the double-encoded exports required, and `null` decodes to an empty payload.
fn decode_payload(field: Option<&Value>) -> Result<Value, String> {
    match field {
        None => Err("unexpected end of JSON input".to_string()),
        Some(Value::String(text)) => {
            serde_json::from_str::<Value>(text).map_err(|error| error.to_string())
        }
        Some(Value::Null) => Ok(Value::Object(Map::new())),
        Some(value) => Ok(value.clone()),
    }
}

/// `encoding/json`'s field-name match: the exact spelling wins, and failing that
/// the case-insensitive one, so the legacy export's `Key`/`key` and
/// `URL`/`url` spellings all bind to the same field.
fn go_field<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    object.get(name).or_else(|| {
        object
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    })
}

/// The canonical field digest Go hashed with `sha256`, over the same anonymous
/// struct `cmd/kvimport` marshalled: Go field names in declaration order, with
/// the two nullable milliseconds as JSON numbers or `null`.
#[must_use]
pub fn checksum(link: &Link) -> [u8; 32] {
    let canonical = CanonicalRecord::of(link);
    let encoded = go_escaped_json(&serde_json::to_string(&canonical).unwrap_or_default());
    Sha256::digest(encoded.as_bytes()).into()
}

/// The struct `cmd/kvimport` marshalled for `checksum`, field for field.
#[derive(Serialize)]
struct CanonicalRecord {
    #[serde(rename = "Slug")]
    slug: String,
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Notes")]
    notes: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Redirect")]
    redirect: i32,
    #[serde(rename = "Expires")]
    expires: Option<i64>,
    #[serde(rename = "LastVisited")]
    last_visited: Option<i64>,
    #[serde(rename = "Visits")]
    visits: i64,
    #[serde(rename = "Created")]
    created: i64,
    #[serde(rename = "Updated")]
    updated: i64,
}

impl CanonicalRecord {
    fn of(link: &Link) -> Self {
        Self {
            slug: link.slug.clone(),
            url: link.destination_url.clone(),
            title: link.title.clone(),
            notes: link.notes.clone(),
            status: link.status.as_str().to_string(),
            redirect: link.redirect_status,
            expires: link.expires_at.map(millis),
            last_visited: link.last_visited_at.map(millis),
            visits: link.visit_count,
            created: millis(link.created_at),
            updated: millis(link.updated_at),
        }
    }
}

/// `millis`: the UTC millisecond stamp Go hashed, or `nil`.
fn millis(value: chrono::DateTime<chrono::Utc>) -> i64 {
    value.timestamp_millis()
}

/// `equalChecksum`.
#[must_use]
pub fn equal_checksum(a: &Link, b: &Link) -> bool {
    checksum(a) == checksum(b)
}

/// `xorDigest`: fold one record digest into the running source digest.
pub fn xor_digest(target: &mut [u8; 32], value: &[u8; 32]) {
    for (target, value) in target.iter_mut().zip(value) {
        *target ^= value;
    }
}

/// `hex.EncodeToString`.
#[must_use]
pub fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut text, byte| {
            let _ = write!(&mut text, "{byte:02x}");
            text
        })
}

/// `verifySource`: re-read the export and compare every record against the
/// database.
fn verify_source(store: &Store, path: &str) -> (bool, Option<String>) {
    let mut reader = match open_input(path) {
        Ok(reader) => reader,
        Err(reason) => return (false, Some(reason)),
    };
    loop {
        let line = match read_line(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => return (true, None),
            Err(reason) => return (false, Some(reason)),
        };
        let candidate = match parse_record(&line) {
            ParsedRecord::Ignored => continue,
            ParsedRecord::Rejected(reason) => return (false, Some(reason)),
            ParsedRecord::Accepted(candidate) => candidate,
        };
        match store.get_by_slug(&candidate.slug) {
            Ok(existing) if equal_checksum(&existing, &candidate) => {}
            Ok(_) => {
                return (
                    false,
                    Some(format!("verification mismatch for {}", candidate.slug)),
                )
            }
            Err(error) => return (false, Some(format!("verify {}: {}", candidate.slug, error))),
        }
    }
}

/// The message Go printed for a failure value.
fn message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn link(slug: &str, url: &str, visits: i64) -> Link {
        let created = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        Link {
            id: 1,
            slug: slug.to_string(),
            destination_url: url.to_string(),
            title: String::new(),
            notes: String::new(),
            status: Status::Active,
            redirect_status: 302,
            expires_at: None,
            visit_count: visits,
            last_visited_at: None,
            created_at: created,
            updated_at: created,
        }
    }

    #[test]
    fn parses_a_plain_legacy_record() {
        let line = r#"{"key":"link:launch","value":{"url":"https://example.com/a","createdAt":"2026-08-01T10:00:00Z","visits":12,"lastVisitedAt":"2026-08-02T11:00:00Z"}}"#;
        let ParsedRecord::Accepted(candidate) = parse_record(line) else {
            panic!("record must be accepted");
        };
        assert_eq!(candidate.slug, "launch");
        assert_eq!(candidate.destination_url, "https://example.com/a");
        assert_eq!(candidate.visit_count, 12);
        assert_eq!(candidate.status, Status::Active);
        assert_eq!(candidate.redirect_status, 302);
        assert_eq!(candidate.title, "");
        assert_eq!(candidate.notes, "");
        assert_eq!(candidate.created_at, candidate.updated_at);
        assert_eq!(
            candidate.last_visited_at.map(|value| value.to_rfc3339()),
            Some("2026-08-02T11:00:00+00:00".to_string())
        );
    }

    #[test]
    fn unwraps_a_nested_string_payload() {
        let nested = r#"{"key":"link:nested","value":"{\"url\":\"https://example.com/nested\",\"createdAt\":\"2026-08-01T10:00:00Z\"}"}"#;
        let ParsedRecord::Accepted(candidate) = parse_record(nested) else {
            panic!("nested payload must be accepted");
        };
        assert_eq!(candidate.slug, "nested");
        assert_eq!(candidate.destination_url, "https://example.com/nested");
        assert_eq!(candidate.visit_count, 0);
        assert!(candidate.last_visited_at.is_none());
    }

    #[test]
    fn ignores_every_key_that_is_not_a_link() {
        for key in ["config:any", "link", "links:abc", "setting:link:x"] {
            let line = format!(r#"{{"key":"{key}","value":{{}}}}"#);
            assert_eq!(
                parse_record(&line),
                ParsedRecord::Ignored,
                "{key} must be ignored"
            );
        }
        assert_eq!(
            parse_record(r#"{"value":{"url":"https://example.com"}}"#),
            ParsedRecord::Ignored,
            "a missing key is the empty string, as in Go"
        );
    }

    #[test]
    fn strips_exactly_one_prefix_like_trimprefix() {
        // Go used strings.TrimPrefix, so a doubled prefix leaves a colon in the
        // slug and the slug validation rejects it.
        let line = r#"{"key":"link:link:abc","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z"}}"#;
        assert!(matches!(parse_record(line), ParsedRecord::Rejected(_)));
    }

    #[test]
    fn rejects_malformed_records_the_way_go_did() {
        let cases = [
            // Unparsable JSON.
            "{",
            // No `value` field at all: Go decoded an empty RawMessage.
            r#"{"key":"link:a"}"#,
            // A string payload that is not JSON.
            r#"{"key":"link:a","value":"not json"}"#,
            // Bad timestamps.
            r#"{"key":"link:bad","value":{"url":"javascript:alert(1)","createdAt":"bad"}}"#,
            r#"{"key":"link:bad","value":{"url":"https://example.com","createdAt":"2026-08-01"}}"#,
            // Negative visits.
            r#"{"key":"link:bad","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z","visits":-1}}"#,
            // Bad lastVisitedAt.
            r#"{"key":"link:bad","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z","lastVisitedAt":"nope"}}"#,
            // Destination that is not http(s).
            r#"{"key":"link:bad","value":{"url":"javascript:alert(1)","createdAt":"2026-08-01T10:00:00Z"}}"#,
            r#"{"key":"link:bad","value":{"url":"","createdAt":"2026-08-01T10:00:00Z"}}"#,
            // Slug shape: too short and reserved.
            r#"{"key":"link:ab","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z"}}"#,
            r#"{"key":"link:metrics","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z"}}"#,
            // Type mismatches Go rejected.
            r#"{"key":"link:a","value":{"url":"https://example.com","createdAt":"2026-08-01T10:00:00Z","visits":"12"}}"#,
            r#"{"key":5,"value":{}}"#,
        ];
        for line in cases {
            assert!(
                matches!(parse_record(line), ParsedRecord::Rejected(_)),
                "must reject {line}"
            );
        }
    }

    #[test]
    fn matches_field_names_case_insensitively_like_go() {
        let upper = r#"{"KEY":"link:upper","VALUE":{"URL":"https://example.com/u","CREATEDAT":"2026-08-01T10:00:00Z","VISITS":3,"LASTVISITEDAT":"2026-08-02T10:00:00Z"}}"#;
        let ParsedRecord::Accepted(candidate) = parse_record(upper) else {
            panic!("Go's decoder binds case-insensitive field names");
        };
        assert_eq!(candidate.slug, "upper");
        assert_eq!(candidate.visit_count, 3);
        assert_eq!(candidate.destination_url, "https://example.com/u");
    }

    #[test]
    fn accepts_null_visits_and_last_visited() {
        let line = r#"{"key":"link:nulls","value":{"url":"https://example.com/n","createdAt":"2026-08-01T10:00:00Z","visits":null,"lastVisitedAt":null}}"#;
        let ParsedRecord::Accepted(candidate) = parse_record(line) else {
            panic!("null fields decode as Go's nil pointers");
        };
        assert_eq!(candidate.visit_count, 0);
        assert!(candidate.last_visited_at.is_none());
    }

    #[test]
    fn canonical_digest_is_go_marshal_of_the_anonymous_struct() {
        let candidate = link("launch", "https://example.com/a", 12);
        let encoded = serde_json::to_string(&CanonicalRecord::of(&candidate)).expect("encode");
        assert_eq!(
            encoded,
            r#"{"Slug":"launch","URL":"https://example.com/a","Title":"","Notes":"","Status":"active","Redirect":302,"Expires":null,"LastVisited":null,"Visits":12,"Created":1785578400000,"Updated":1785578400000}"#
        );
        assert_eq!(hex_digest(&checksum(&candidate)).len(), 64);
    }

    #[test]
    fn digest_changes_with_the_hashed_fields() {
        let base = link("verify", "https://example.com/v", 5);
        assert!(equal_checksum(
            &base,
            &link("verify", "https://example.com/v", 5)
        ));
        let moved_visits = Link {
            visit_count: 6,
            ..base.clone()
        };
        assert!(!equal_checksum(&base, &moved_visits));
        let moved_updated = Link {
            updated_at: base.updated_at + chrono::Duration::seconds(1),
            ..base.clone()
        };
        assert!(!equal_checksum(&base, &moved_updated));
        let moved_title = Link {
            title: "seeded".to_string(),
            ..base.clone()
        };
        assert!(!equal_checksum(&base, &moved_title));
    }

    #[test]
    fn xor_digest_folds_in_and_out() {
        let first = checksum(&link("a", "https://example.com/a", 1));
        let second = checksum(&link("b", "https://example.com/b", 2));
        let mut digest = [0_u8; 32];
        xor_digest(&mut digest, &first);
        xor_digest(&mut digest, &second);
        let mut expected = first;
        for (expected, value) in expected.iter_mut().zip(&second) {
            *expected ^= value;
        }
        assert_eq!(digest, expected);
        // XOR is its own inverse, so adding both records back cancels out.
        xor_digest(&mut digest, &first);
        xor_digest(&mut digest, &second);
        assert_eq!(digest, [0_u8; 32]);
    }

    #[test]
    fn hex_digest_is_go_lowercase_hex() {
        let digest = [0xde_u8, 0xad, 0x00, 0x01];
        let mut padded = [0_u8; 32];
        padded[..4].copy_from_slice(&digest);
        assert!(hex_digest(&padded).starts_with("dead0001"));
        assert_eq!(hex_digest(&[0_u8; 32]).len(), 64);
    }

    #[test]
    fn rejects_oversized_lines_like_the_scanner() {
        let line = "x".repeat(MAX_LINE_BYTES + 1);
        let mut reader = BufReader::new(line.as_bytes());
        assert_eq!(
            read_line(&mut reader).expect_err("oversized"),
            "scanner: token too long"
        );
        // The limit applies to the token, so a token of exactly the limit is
        // accepted with its newline.
        let exact = format!("{}\n", "y".repeat(MAX_LINE_BYTES));
        let mut reader = BufReader::new(exact.as_bytes());
        assert_eq!(
            read_line(&mut reader)
                .expect("exact")
                .map(|line| line.len()),
            Some(MAX_LINE_BYTES),
        );
    }

    #[test]
    fn drops_the_line_terminator_like_scanlines() {
        let mut reader = BufReader::new(b"first\r\nsecond\n".as_ref());
        assert_eq!(
            read_line(&mut reader).expect("first"),
            Some("first".to_string())
        );
        assert_eq!(
            read_line(&mut reader).expect("second"),
            Some("second".to_string())
        );
        assert_eq!(read_line(&mut reader).expect("end"), None);
    }

    #[test]
    fn reports_a_missing_input_file_like_go() {
        match open_input("/no/such/export.ndjson") {
            Ok(_) => panic!("a missing export must not open"),
            Err(reason) => assert_eq!(
                reason,
                "open /no/such/export.ndjson: no such file or directory"
            ),
        }
    }

    #[test]
    fn report_line_is_go_encoder_output() {
        let report = Report {
            source: "-".to_string(),
            mode: "dry-run".to_string(),
            conflict_policy: "abort".to_string(),
            input: 3,
            imported: 2,
            identical: 0,
            ignored: 1,
            rejected: 0,
            conflicting: 0,
            replaced: 0,
            skipped: 0,
            checksum: "0".repeat(64),
            verified: false,
        };
        let mut buffer: Vec<u8> = Vec::new();
        write_report(&mut buffer, &report).expect("write");
        let text = String::from_utf8(buffer).expect("utf-8");
        assert!(text.ends_with('\n'), "Go's Encoder terminates every value");
        let (line, _) = text.split_once('\n').expect("one line");
        // Go marshalled the report struct in field order, compactly, with the
        // untagged counters keeping their Go field names.
        let expected = format!(
            "{{\"source\":\"-\",\"mode\":\"dry-run\",\"conflictPolicy\":\"abort\",\"Input\":3,\"Imported\":2,\"Identical\":0,\"Ignored\":1,\"Rejected\":0,\"Conflicting\":0,\"Replaced\":0,\"Skipped\":0,\"checksum\":\"{}\",\"verified\":false}}",
            "0".repeat(64)
        );
        assert_eq!(line, expected);
    }

    #[test]
    fn validates_options_like_go() {
        let options = Options {
            input: "-".to_string(),
            database: ".data/shorten.db".to_string(),
            commit: false,
            policy: "abort".to_string(),
            batch: 500,
            verify: false,
        };
        assert_eq!(options.validate(), Ok(()));
        for batch in [0, 5_001, -3] {
            let bad = Options {
                batch,
                ..options.clone()
            };
            assert_eq!(
                bad.validate().err(),
                Some("batch must be between 1 and 5000".to_string())
            );
        }
        for policy in ["Skip", "abort ", "", "replace-all"] {
            let bad = Options {
                policy: policy.to_string(),
                ..options.clone()
            };
            assert_eq!(
                bad.validate().err(),
                Some("conflict must be abort, skip, or replace".to_string())
            );
        }
    }

    #[test]
    fn normalises_the_destination_like_go() {
        // Go stored `parsed.String()`, which drops the default port and keeps
        // the path as written.
        let line = r#"{"key":"link:normalise","value":{"url":"https://example.com:443/a/b?x=1#frag","createdAt":"2026-08-01T10:00:00Z"}}"#;
        let ParsedRecord::Accepted(candidate) = parse_record(line) else {
            panic!("record must be accepted");
        };
        assert_eq!(
            candidate.destination_url, "https://example.com:443/a/b?x=1#frag",
            "Go's url.URL.String kept the explicit default port"
        );
    }
}
