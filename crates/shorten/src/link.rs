//! Link lifecycle rules: the domain model, slug policy and destination
//! validation.
//!
//! Destination normalisation reproduces the previous implementation byte for
//! byte, so records written by the Go service and records written here compare
//! equal and are interchangeable in the same database.

use std::collections::BTreeMap;
use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use std::fmt::Write;

use chrono::{DateTime, TimeZone, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::json;

/// Availability of a short link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Active,
    Disabled,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    /// Parse the wire value. Unknown spellings are rejected by validation,
    /// which reports "Choose active or disabled." just as before.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A short link and everything the console tracks about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub id: i64,
    pub slug: String,
    pub destination_url: String,
    pub title: String,
    pub notes: String,
    pub status: Status,
    pub redirect_status: i32,
    #[serde(with = "json::nullable_time")]
    pub expires_at: Option<DateTime<Utc>>,
    pub visit_count: i64,
    #[serde(with = "json::nullable_time")]
    pub last_visited_at: Option<DateTime<Utc>>,
    #[serde(with = "json::time")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "json::time")]
    pub updated_at: DateTime<Utc>,
}

impl Link {
    /// The zero record used by the create form, where no link exists yet.
    ///
    /// Timestamps sit at the Unix epoch, which the console renders only for a
    /// stored link.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            id: 0,
            slug: String::new(),
            destination_url: String::new(),
            title: String::new(),
            notes: String::new(),
            status: Status::Active,
            redirect_status: 302,
            expires_at: None,
            visit_count: 0,
            last_visited_at: None,
            created_at: Utc.timestamp_opt(0, 0).single().expect("epoch is in range"),
            updated_at: Utc.timestamp_opt(0, 0).single().expect("epoch is in range"),
        }
    }

    /// A link whose expiry instant has arrived is expired at the exact
    /// boundary, matching the previous boundary test.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires| now >= expires)
    }

    #[must_use]
    pub fn is_available(&self, now: DateTime<Utc>) -> bool {
        self.status == Status::Active && !self.is_expired(now)
    }
}

/// Fields accepted when creating a link. Empty and zero values take the same
/// defaults the previous implementation applied: an active link that answers
/// with `302`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateInput {
    pub slug: String,
    pub destination_url: String,
    pub title: String,
    pub notes: String,
    pub status: Option<Status>,
    pub redirect_status: i32,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Updating a link validates against the same rules as creating one.
pub type UpdateInput = CreateInput;

/// Field-keyed validation messages, ordered so output is stable.
pub type ValidationErrors = BTreeMap<String, String>;

/// Every failure a link operation can report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("link not found")]
    NotFound,
    #[error("slug already exists")]
    Conflict,
    #[error("slug already exists: {slug}")]
    ConflictNamed { slug: String },
    #[error("link validation failed")]
    Validation(ValidationErrors),
    #[error("{0}")]
    Message(String),
    #[error("{context}: {detail}")]
    Wrapped { context: String, detail: String },
}

impl DomainError {
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict | Self::ConflictNamed { .. })
    }

    #[must_use]
    pub fn validation(&self) -> Option<&ValidationErrors> {
        match self {
            Self::Validation(errors) => Some(errors),
            _ => None,
        }
    }

    /// Build a wrapped operational error with a `%: %w` shaped message.
    #[must_use]
    pub fn wrapped(context: impl Into<String>, source: impl fmt::Display) -> Self {
        Self::Wrapped {
            context: context.into(),
            detail: source.to_string(),
        }
    }
}

/// Destination rules, reported with the same copy the console expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationError {
    Missing,
    Invalid,
}

impl fmt::Display for DestinationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Missing => "Enter an HTTP or HTTPS destination up to 8,192 characters.",
            Self::Invalid => "Enter a complete URL starting with http:// or https://.",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for DestinationError {}

pub const SLUG_PATTERN: &str = r"^[A-Za-z0-9_-]{3,64}$";

/// Reserved short codes; a user-supplied slug may never shadow a route.
#[must_use]
pub fn is_reserved(value: &str) -> bool {
    RESERVED
        .iter()
        .any(|item| *item == value.to_ascii_lowercase())
}

const RESERVED: &[&str] = &[
    "api",
    "manage",
    "assets",
    "healthz",
    "readyz",
    "metrics",
    "favicon.ico",
    "robots.txt",
];

fn slug_regex() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(SLUG_PATTERN).expect("valid slug pattern"))
}

#[must_use]
pub fn valid_slug(value: &str) -> bool {
    slug_regex().is_match(value) && !is_reserved(value)
}

#[must_use]
pub fn valid_redirect_status(value: i32) -> bool {
    matches!(value, 301 | 302 | 307 | 308)
}

/// Short code alphabet: six random bytes in base64url, eight characters.
///
/// # Errors
///
/// Propagates a failure of the system random source.
pub fn generate_slug() -> Result<String, DomainError> {
    let mut bytes = [0_u8; 6];
    rand::rng().fill_bytes(&mut bytes);
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Trim, validate and canonicalise a destination, mutating the input in place
/// the way the previous implementation normalised it.
///
/// # Errors
///
/// Returns the message to show the administrator.
pub fn validate_destination(value: &mut String) -> Result<(), DestinationError> {
    let normalized = normalize_destination(value)?;
    *value = normalized;
    Ok(())
}

/// Validate a create request, applying defaults and collecting every field
/// problem in one pass.
///
/// # Errors
///
/// Returns `DomainError::Validation` with per-field copy, or a slug collision.
pub fn validate_create(input: &mut CreateInput) -> Result<(), DomainError> {
    let mut errors = ValidationErrors::new();

    input.slug = input.slug.trim().to_string();
    input.title = input.title.trim().to_string();
    input.notes = input.notes.trim().to_string();

    if !input.slug.is_empty() {
        if !slug_regex().is_match(&input.slug) {
            errors.insert(
                "slug".to_string(),
                "Use 3–64 letters, numbers, hyphens, or underscores.".to_string(),
            );
        } else if is_reserved(&input.slug) {
            errors.insert(
                "slug".to_string(),
                "This short code is reserved by the application.".to_string(),
            );
        }
    }

    if let Err(error) = validate_destination(&mut input.destination_url) {
        errors.insert("destination".to_string(), error.to_string());
    }

    if input.title.chars().count() > 200 {
        errors.insert(
            "title".to_string(),
            "Keep the title under 200 characters.".to_string(),
        );
    }
    if input.notes.chars().count() > 4_000 {
        errors.insert(
            "notes".to_string(),
            "Keep notes under 4,000 characters.".to_string(),
        );
    }

    let status = input.status.unwrap_or(Status::Active);
    input.status = Some(status);
    // A supplied status string that is neither active nor disabled is an
    // error. The typed model cannot express that, so callers that parsed an
    // unknown value signal it through `redirect_status`-style sentinel below.
    if input.redirect_status == 0 {
        input.redirect_status = 302;
    }
    if !valid_redirect_status(input.redirect_status) {
        errors.insert(
            "redirectStatus".to_string(),
            "Choose 301, 302, 307, or 308.".to_string(),
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(DomainError::Validation(errors))
    }
}

/// Report an unknown status spelling with the copy the console showed before.
///
/// The typed `Status` cannot represent junk input, so form and API decoding
/// layers call this when they see a value `Status::parse` rejected.
pub fn status_error(errors: &mut ValidationErrors) {
    errors.insert(
        "status".to_string(),
        "Choose active or disabled.".to_string(),
    );
}

/// Normalise a destination the way the Go service did: trim, require an
/// absolute http(s) URL with a host, then rebuild the canonical form from the
/// authored text so paths, queries and fragments survive byte-for-byte.
///
/// # Errors
///
/// Returns the administrator-facing copy for the destination field.
pub fn normalize_destination(value: &str) -> Result<String, DestinationError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 8_192 {
        return Err(DestinationError::Missing);
    }
    let parsed = url::Url::parse(value).map_err(|_| DestinationError::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(DestinationError::Invalid);
    }
    let scheme_end = value.find(':').ok_or(DestinationError::Invalid)?;
    let after_scheme = &value[scheme_end + 1..];
    let after_authority = after_scheme
        .strip_prefix("//")
        .ok_or(DestinationError::Invalid)?;
    let authority_end = after_authority
        .find(['/', '?', '#'])
        .unwrap_or(after_authority.len());
    let authority = &after_authority[..authority_end];
    let (userinfo, host) = match authority.rfind('@') {
        Some(at) => (&authority[..=at], &authority[at + 1..]),
        None => ("", authority),
    };
    if host.is_empty() {
        return Err(DestinationError::Invalid);
    }
    // A URL with an authority must keep an explicit host; validate that the
    // host portion is shaped like one, as Go's parser did.
    if host
        .chars()
        .any(|c| c.is_whitespace() || c == '\\' || c == '@')
    {
        return Err(DestinationError::Invalid);
    }
    let remainder = &after_authority[authority_end..];
    let (path_and_query, fragment) = split_at_first(remainder, '#');
    let (path, query) = split_at_first(path_and_query, '?');
    let mut out = String::with_capacity(value.len());
    out.push_str(parsed.scheme());
    out.push_str("://");
    out.push_str(userinfo);
    out.push_str(host);
    let decoded_path = unescape(path, should_escape_path)?;
    out.push_str(&go_escape_path(path, &decoded_path));
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    if let Some(fragment) = fragment {
        out.push('#');
        let decoded_fragment = unescape(fragment, should_escape_fragment)?;
        out.push_str(&go_escape_fragment(fragment, &decoded_fragment));
    }
    Ok(out)
}

fn split_at_first(value: &str, marker: char) -> (&str, Option<&str>) {
    match value.find(marker) {
        Some(index) => (&value[..index], Some(&value[index + 1..])),
        None => (value, None),
    }
}

/// The Go `net/url` path escaping table: alphanumerics, `-_.~` and the
/// reserved set `$&+,/:;=` stay literal, everything else is percent-encoded,
/// including non-ASCII bytes.
/// Rebuild the path exactly as `net/url` does.
///
/// `URL.String()` writes the authored path verbatim when every byte in it is
/// legal in a path (percent escapes included), and only escapes the decoded
/// path when the authored text was not a valid encoding.
fn go_escape_path(authored: &str, decoded: &str) -> String {
    if valid_encoded(authored, should_escape_path) {
        authored.to_string()
    } else {
        escape_with(decoded, should_escape_path)
    }
}

/// Rebuild the fragment with the same rule, which additionally allows `?`.
fn go_escape_fragment(authored: &str, decoded: &str) -> String {
    if valid_encoded(authored, should_escape_fragment) {
        authored.to_string()
    } else {
        escape_with(decoded, should_escape_fragment)
    }
}

/// `net/url`'s `shouldEscape` for path mode.
fn should_escape_path(byte: u8) -> bool {
    !matches!(
        byte,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' |
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' |
        b'-' | b'.' | b'_' | b'~' | b'/' | b':' | b'@'
    )
}

/// `net/url`'s `shouldEscape` for fragment mode.
fn should_escape_fragment(byte: u8) -> bool {
    byte != b'?' && should_escape_path(byte)
}

/// `net/url`'s `validEncoded`: every byte is legal unescaped in this mode, or
/// a percent sign introducing an escape.
fn valid_encoded(value: &str, escape: impl Fn(u8) -> bool) -> bool {
    value.bytes().all(|byte| byte == b'%' || !escape(byte))
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

/// Percent-decode one component, rejecting the forms `net/url` rejects: a
/// truncated escape, a lowercase escape of a byte that must be escaped (the
/// canonical form is uppercase), and a `%25` that recreates an ambiguous
/// escape.
fn unescape(value: &str, escape: impl Fn(u8) -> bool) -> Result<String, DestinationError> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'%' {
            out.push(byte);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() || !is_hex(bytes[index + 1]) || !is_hex(bytes[index + 2]) {
            return Err(DestinationError::Invalid);
        }
        let decoded = (hex_value(bytes[index + 1]) << 4) + hex_value(bytes[index + 2]);
        if decoded == b'%'
            && index + 4 < bytes.len()
            && is_hex(bytes[index + 3])
            && is_hex(bytes[index + 4])
        {
            return Err(DestinationError::Invalid);
        }
        let lower_case =
            bytes[index + 1].is_ascii_lowercase() || bytes[index + 2].is_ascii_lowercase();
        if escape(decoded) && lower_case {
            return Err(DestinationError::Invalid);
        }
        out.push(decoded);
        index += 3;
    }
    String::from_utf8(out).map_err(|_| DestinationError::Invalid)
}

fn escape_with(value: &str, escape: impl Fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if escape(*byte) {
            out.push('%');
            let _ = write!(out, "{byte:02X}");
        } else {
            out.push(char::from(*byte));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn input() -> CreateInput {
        CreateInput {
            slug: "launch_26".to_string(),
            destination_url: "https://example.com/a".to_string(),
            title: String::new(),
            notes: String::new(),
            status: Some(Status::Active),
            redirect_status: 302,
            expires_at: None,
        }
    }

    #[test]
    fn accepts_valid_input() {
        let mut create = input();
        validate_create(&mut create).expect("valid");
        assert_eq!(create.slug, "launch_26");
        assert_eq!(create.destination_url, "https://example.com/a");
    }

    #[test]
    fn rejects_bad_scheme() {
        let mut create = CreateInput {
            slug: "launch".to_string(),
            destination_url: "javascript:alert(1)".to_string(),
            ..CreateInput::default()
        };
        let error = validate_create(&mut create).expect_err("bad scheme");
        let errors = error.validation().expect("field errors");
        assert_eq!(
            errors.get("destination").expect("destination message"),
            "Enter a complete URL starting with http:// or https://."
        );
    }

    #[test]
    fn rejects_reserved_slug_case_insensitively() {
        let mut create = CreateInput {
            slug: "API".to_string(),
            destination_url: "https://example.com".to_string(),
            ..CreateInput::default()
        };
        let errors = validate_create(&mut create)
            .expect_err("reserved")
            .validation()
            .expect("field errors")
            .clone();
        assert_eq!(
            errors.get("slug").expect("slug message"),
            "This short code is reserved by the application."
        );
    }

    #[test]
    fn rejects_short_slug() {
        let mut create = CreateInput {
            slug: "ab".to_string(),
            destination_url: "https://example.com".to_string(),
            ..CreateInput::default()
        };
        let errors = validate_create(&mut create)
            .expect_err("short slug")
            .validation()
            .expect("field errors")
            .clone();
        assert_eq!(
            errors.get("slug").expect("slug message"),
            "Use 3–64 letters, numbers, hyphens, or underscores."
        );
    }

    #[test]
    fn rejects_long_notes() {
        let mut create = CreateInput {
            slug: "valid".to_string(),
            destination_url: "https://example.com".to_string(),
            notes: "a".repeat(4001),
            ..CreateInput::default()
        };
        let errors = validate_create(&mut create)
            .expect_err("long notes")
            .validation()
            .expect("field errors")
            .clone();
        assert_eq!(
            errors.get("notes").expect("notes message"),
            "Keep notes under 4,000 characters."
        );
    }

    #[test]
    fn defaults_status_and_redirect() {
        let mut create = CreateInput {
            slug: String::new(),
            destination_url: "https://example.com".to_string(),
            status: None,
            redirect_status: 0,
            ..CreateInput::default()
        };
        validate_create(&mut create).expect("valid");
        assert_eq!(create.status, Some(Status::Active));
        assert_eq!(create.redirect_status, 302);
    }

    #[test]
    fn rejects_bad_redirect_status() {
        let mut create = CreateInput {
            slug: "valid".to_string(),
            destination_url: "https://example.com".to_string(),
            redirect_status: 305,
            ..CreateInput::default()
        };
        let errors = validate_create(&mut create)
            .expect_err("bad redirect")
            .validation()
            .expect("field errors")
            .clone();
        assert_eq!(
            errors.get("redirectStatus").expect("message"),
            "Choose 301, 302, 307, or 308."
        );
    }

    #[test]
    fn generated_slugs_are_valid_and_unique() {
        let first = generate_slug().expect("slug");
        let second = generate_slug().expect("slug");
        assert_eq!(first.len(), 8);
        assert!(valid_slug(&first));
        assert_ne!(first, second);
    }

    #[test]
    fn expires_at_exact_boundary() {
        let now = crate::time::truncate_millis(chrono::Utc::now());
        let link = Link {
            id: 1,
            slug: "x".to_string(),
            destination_url: "https://example.com".to_string(),
            title: String::new(),
            notes: String::new(),
            status: Status::Active,
            redirect_status: 302,
            expires_at: Some(now),
            visit_count: 0,
            last_visited_at: None,
            created_at: now,
            updated_at: now,
        };
        assert!(link.is_expired(now));
        assert!(!link.is_available(now));
    }

    #[test]
    fn normalizes_like_the_go_service() {
        let cases = [
            ("https://example.com", "https://example.com"),
            ("  https://example.com  ", "https://example.com"),
            ("https://example.com/", "https://example.com/"),
            (
                "https://example.com/path?q=1&r=2#frag",
                "https://example.com/path?q=1&r=2#frag",
            ),
            ("https://example.com/a b", "https://example.com/a%20b"),
            ("https://example.com/ü", "https://example.com/%C3%BC"),
            ("https://example.com/%41", "https://example.com/%41"),
            (
                "https://user:pw@example.com/x",
                "https://user:pw@example.com/x",
            ),
            ("https://example.com:8443/x", "https://example.com:8443/x"),
            ("HTTP://example.com/x", "http://example.com/x"),
        ];
        for (source, expected) in cases {
            assert_eq!(
                normalize_destination(source).expect("normalized"),
                expected,
                "normalize {source}"
            );
        }
    }

    #[test]
    fn rejects_malformed_destinations() {
        for source in [
            "",
            "   ",
            "ftp://example.com/file",
            "javascript:alert(1)",
            "example.com/x",
            "/relative/path",
            "https://",
            &format!("https://example.com/{}", "a".repeat(8192)),
        ] {
            assert!(
                normalize_destination(source).is_err(),
                "expected rejection for {source}"
            );
        }
    }

    #[test]
    fn status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Status::Disabled).expect("json"),
            "\"disabled\""
        );
    }

    #[test]
    fn reserved_codes_cover_routes() {
        for code in [
            "api",
            "MANAGE",
            "healthz",
            "readyz",
            "metrics",
            "assets",
            "robots.txt",
        ] {
            assert!(is_reserved(code), "{code} must be reserved");
        }
        assert!(!is_reserved("article"));
    }

    #[test]
    fn truncates_to_millis() {
        let value = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        assert_eq!(crate::time::truncate_millis(value), value);
    }
}
