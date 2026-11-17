//! View models and presentation formatting, ported from the Go models.

use chrono::{DateTime, Datelike, Local, Utc};
use shorten::link::{Link, Status, ValidationErrors};
use std::fmt::Write;

/// Page chrome shared by every management template.
#[derive(Clone, Debug, Default)]
pub struct PageMeta {
    pub title: String,
    pub active: String,
    pub csrf: String,
    pub notice: String,
    pub error: String,
}

/// The sign-in page model.
#[derive(Clone, Debug, Default)]
pub struct LoginModel {
    pub error: String,
}

/// The link list page model.
#[derive(Clone, Debug)]
pub struct LinksModel {
    pub meta: PageMeta,
    pub links: Vec<Link>,
    pub total: i64,
    pub total_known: bool,
    pub next_cursor: String,
    pub search: String,
    pub filter: String,
    pub sort: String,
    pub dir: String,
    pub base_url: String,
    /// Reference instant for status labels, captured once per request.
    pub now: DateTime<Utc>,
}

/// The create/edit form model.
#[derive(Clone, Debug)]
pub struct LinkFormModel {
    pub meta: PageMeta,
    pub link: Link,
    pub original_slug: String,
    pub destination: String,
    pub slug: String,
    pub title: String,
    pub notes: String,
    pub status: String,
    pub redirect_status: String,
    pub expires_at: String,
    pub base_url: String,
    pub errors: ValidationErrors,
    pub is_create: bool,
    /// Reference instant for the live preview panel.
    pub now: DateTime<Utc>,
}

/// The rename/delete confirmation model.
#[derive(Clone, Debug)]
pub struct ConfirmModel {
    pub meta: PageMeta,
    pub link: Link,
    pub base_url: String,
    pub rename: bool,
    /// Reference instant for the status chip.
    pub now: DateTime<Utc>,
}

/// The cleanup page model.
#[derive(Clone, Debug)]
pub struct CleanupModel {
    pub meta: PageMeta,
    pub links: Vec<Link>,
    pub base_url: String,
    pub scope: String,
    pub months: i64,
    pub count: i64,
    pub cutoff: DateTime<Utc>,
    pub deleted: i64,
    pub form_error: String,
    /// Reference instant for status labels.
    pub now: DateTime<Utc>,
}

impl LinksModel {
    /// The pager link, carrying the current search, filter and ordering.
    #[must_use]
    pub fn next_url(&self) -> String {
        let mut pairs: Vec<(String, String)> = Vec::new();
        if !self.filter.is_empty() && self.filter != "all" {
            pairs.push(("status".to_string(), self.filter.clone()));
        }
        pairs.push(("cursor".to_string(), self.next_cursor.clone()));
        if !self.dir.is_empty() {
            pairs.push(("dir".to_string(), self.dir.clone()));
        }
        if !self.search.is_empty() {
            pairs.push(("q".to_string(), self.search.clone()));
        }
        if !self.sort.is_empty() {
            pairs.push(("sort".to_string(), self.sort.clone()));
        }
        pairs.sort_by(|left, right| left.0.cmp(&right.0));
        let encoded: Vec<String> = pairs
            .iter()
            .map(|(key, value)| format!("{key}={}", crate::context::query_escape(value)))
            .collect();
        format!("/manage/links?{}", encoded.join("&"))
    }

    /// Status chip label for a row.
    #[must_use]
    pub fn status_label(&self, item: &Link) -> String {
        status_label(item, self.now)
    }

    /// Status chip class for a row.
    #[must_use]
    pub fn status_class(&self, item: &Link) -> String {
        status_class(item, self.now)
    }

    /// Grouped total for the table summary.
    #[must_use]
    pub fn total_text(&self) -> String {
        format_count(self.total)
    }

    /// Rows listed on this page.
    #[must_use]
    pub fn listed(&self) -> String {
        self.links.len().to_string()
    }

    /// Grouped visit count for one row.
    #[must_use]
    pub fn visits(&self, item: &Link) -> String {
        format_count(item.visit_count)
    }

    /// Row note: the title, or the untitled placeholder.
    #[must_use]
    pub fn row_note(&self, item: &Link) -> String {
        if item.title.is_empty() {
            "Untitled link".to_string()
        } else {
            item.title.clone()
        }
    }

    /// Updated column of a row.
    #[must_use]
    pub fn updated(&self, item: &Link) -> String {
        format_date(item.updated_at)
    }

    /// Short URL of a row, as shown in the table.
    #[must_use]
    pub fn short_url(&self, item: &Link) -> String {
        short_url(&self.base_url, &item.slug)
    }

    /// Edit link for a row.
    #[must_use]
    pub fn edit_url(&self, item: &Link) -> String {
        format!("/manage/links/{}", path_escape(&item.slug))
    }
}

impl LinkFormModel {
    /// Where the form posts to: the collection for a create, the item for an
    /// edit.
    #[must_use]
    pub fn form_action(&self) -> String {
        if self.is_create {
            return "/manage/links".to_string();
        }
        format!("/manage/links/{}", path_escape(&self.original_slug))
    }

    #[must_use]
    pub fn short_url(&self) -> String {
        short_url(&self.base_url, &self.link.slug)
    }

    #[must_use]
    pub fn rename_url(&self) -> String {
        format!("/manage/links/{}/rename", path_escape(&self.link.slug))
    }

    #[must_use]
    pub fn delete_url(&self) -> String {
        format!("/manage/links/{}/delete", path_escape(&self.link.slug))
    }

    #[must_use]
    pub fn status_label(&self) -> String {
        status_label(&self.link, self.now)
    }

    #[must_use]
    pub fn status_class(&self) -> String {
        status_class(&self.link, self.now)
    }

    /// Validation message for one field, rendered under the control.
    #[must_use]
    pub fn error(&self, key: &str) -> Option<&String> {
        self.errors.get(key)
    }

    /// Is `aria-invalid` set on a control?
    #[must_use]
    /// Field error message, empty when the field is valid.
    pub fn error_text(&self, key: &str) -> String {
        self.error(key).cloned().unwrap_or_default()
    }

    #[must_use]
    pub fn invalid(&self, key: &str) -> String {
        (self.errors.contains_key(key)).to_string()
    }

    /// Does the advanced options panel start open?
    #[must_use]
    pub fn advanced_open(&self) -> bool {
        self.status != "active"
            || self.redirect_status != "302"
            || !self.expires_at.is_empty()
            || !self.notes.is_empty()
            || self.errors.contains_key("status")
            || self.errors.contains_key("redirectStatus")
            || self.errors.contains_key("expiresAt")
            || self.errors.contains_key("notes")
    }

    /// Redirect label for the live preview panel.
    #[must_use]
    pub fn redirect_label(&self) -> String {
        redirect_label(self.link.redirect_status)
    }

    #[must_use]
    pub fn visits(&self) -> String {
        format_count(self.link.visit_count)
    }

    #[must_use]
    pub fn last_visited(&self) -> String {
        format_relative(self.link.last_visited_at)
    }

    #[must_use]
    pub fn created(&self) -> String {
        format_date(self.link.created_at)
    }
}

impl ConfirmModel {
    #[must_use]
    pub fn confirm_action(&self) -> String {
        let action = if self.rename { "rename" } else { "delete" };
        format!("/manage/links/{}/{}", path_escape(&self.link.slug), action)
    }

    #[must_use]
    pub fn back_url(&self) -> String {
        format!("/manage/links/{}", path_escape(&self.link.slug))
    }

    #[must_use]
    pub fn short_url(&self) -> String {
        short_url(&self.base_url, &self.link.slug)
    }
}

impl CleanupModel {
    #[must_use]
    pub fn status_label(&self, item: &Link) -> String {
        status_label(item, self.now)
    }

    #[must_use]
    pub fn status_class(&self, item: &Link) -> String {
        status_class(item, self.now)
    }

    #[must_use]
    pub fn short_url(&self, item: &Link) -> String {
        short_url(&self.base_url, &item.slug)
    }

    #[must_use]
    pub fn link_url(&self, item: &Link) -> String {
        format!("/manage/links/{}", path_escape(&item.slug))
    }

    /// RFC 3339 stamp for the `<time datetime>` attribute.
    #[must_use]
    pub fn created_stamp(&self, item: &Link) -> String {
        shorten::json::format_go_time(item.created_at)
    }

    #[must_use]
    pub fn created_date(&self, item: &Link) -> String {
        format_date(item.created_at)
    }

    #[must_use]
    pub fn count_text(&self) -> String {
        format_count(self.count)
    }

    /// Preview line note: the title, falling back to the destination.
    #[must_use]
    pub fn item_note(&self, item: &Link) -> String {
        if item.title.is_empty() {
            item.destination_url.clone()
        } else {
            item.title.clone()
        }
    }

    /// Is the preview truncated to the oldest matches?
    #[must_use]
    pub fn shows_more(&self) -> bool {
        self.count > i64::try_from(self.links.len()).unwrap_or(i64::MAX)
    }

    /// Number of preview rows listed.
    #[must_use]
    pub fn listed(&self) -> String {
        self.links.len().to_string()
    }

    #[must_use]
    pub fn cutoff_date(&self) -> String {
        format_date(self.cutoff)
    }

    #[must_use]
    pub fn months_text(&self) -> String {
        self.months.to_string()
    }
}

/// Build the edit form from a stored link.
#[must_use]
pub fn form_from_link(
    meta: PageMeta,
    item: Link,
    base_url: &str,
    now: DateTime<Utc>,
) -> LinkFormModel {
    LinkFormModel {
        expires_at: format_local_input(item.expires_at),
        meta,
        original_slug: item.slug.clone(),
        destination: item.destination_url.clone(),
        slug: item.slug.clone(),
        title: item.title.clone(),
        notes: item.notes.clone(),
        status: item.status.as_str().to_string(),
        redirect_status: item.redirect_status.to_string(),
        base_url: base_url.to_string(),
        errors: ValidationErrors::new(),
        is_create: false,
        link: item,
        now,
    }
}

/// The public short URL for a slug.
#[must_use]
pub fn short_url(base: &str, slug: &str) -> String {
    format!("{base}/{}", path_escape(slug))
}

/// Human status for a link at an instant.
#[must_use]
pub fn status_label(item: &Link, now: DateTime<Utc>) -> String {
    if item.is_expired(now) {
        return "Expired".to_string();
    }
    if item.status == Status::Disabled {
        return "Disabled".to_string();
    }
    "Active".to_string()
}

/// CSS class triple for the status chip.
#[must_use]
pub fn status_class(item: &Link, now: DateTime<Utc>) -> String {
    match status_label(item, now).as_str() {
        "Active" => "status status-active".to_string(),
        "Expired" => "status status-expired".to_string(),
        _ => "status status-disabled".to_string(),
    }
}

/// Grouped decimal count, `1,234,567`.
#[must_use]
pub fn format_count(value: i64) -> String {
    let raw = value.to_string();
    let (sign, digits) = match raw.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", raw.as_str()),
    };
    let chars: Vec<char> = digits.chars().collect();
    let mut out = String::with_capacity(chars.len() + chars.len() / 3 + 1);
    out.push_str(sign);
    let lead = chars.len() % 3;
    if lead > 0 {
        out.extend(chars[..lead].iter());
        if chars.len() > lead {
            out.push(',');
        }
    }
    let rest = &chars[lead..];
    for (index, ch) in rest.iter().enumerate() {
        out.push(*ch);
        if index % 3 == 2 && index + 1 < rest.len() {
            out.push(',');
        }
    }
    out
}

/// `Jan 2, 2006` in the server's local zone.
#[must_use]
pub fn format_date(value: DateTime<Utc>) -> String {
    let local = value.with_timezone(&Local);
    format!("{} {}, {}", local.format("%b"), local.day(), local.year())
}

/// Relative "last visited" label.
#[must_use]
pub fn format_relative(value: Option<DateTime<Utc>>) -> String {
    let Some(value) = value else {
        return "Never".to_string();
    };
    let elapsed = Utc::now()
        .signed_duration_since(value)
        .to_std()
        .unwrap_or(std::time::Duration::ZERO);
    if elapsed < std::time::Duration::from_secs(60) {
        return "Just now".to_string();
    }
    if elapsed < std::time::Duration::from_secs(3_600) {
        return format!("{} min ago", elapsed.as_secs() / 60);
    }
    if elapsed < std::time::Duration::from_secs(86_400) {
        return format!("{} hr ago", elapsed.as_secs() / 3_600);
    }
    format_date(value)
}

/// Redirect semantics label.
#[must_use]
pub fn redirect_label(value: i32) -> String {
    match value {
        301 => "301 Permanent".to_string(),
        307 => "307 Temporary · preserve method".to_string(),
        308 => "308 Permanent · preserve method".to_string(),
        _ => "302 Temporary".to_string(),
    }
}

/// Value for the `datetime-local` control, `2006-01-02T15:04` in UTC.
#[must_use]
pub fn format_local_input(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|value| value.format("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default()
}

/// Percent-escape one path segment like `url.PathEscape` does.
#[must_use]
pub fn path_escape(value: &str) -> String {
    // `url.PathEscape` keeps RFC 3986 segment-safe characters and escapes
    // everything else, `/` and `?` included.
    const SAFE: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ01_234_56789!$&'()*+,;=-._~:@";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if SAFE.contains(byte) {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            let _ = write!(out, "{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn link(slug: &str) -> Link {
        Link {
            id: 1,
            slug: slug.to_string(),
            destination_url: "https://example.com/x".to_string(),
            title: String::new(),
            notes: String::new(),
            status: Status::Active,
            redirect_status: 302,
            expires_at: None,
            visit_count: 1_234_567,
            last_visited_at: None,
            created_at: Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap(),
        }
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(10_000_000), "10,000,000");
        assert_eq!(format_count(-1_234), "-1,234");
    }

    #[test]
    fn dates_match_the_go_layout() {
        let value = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let formatted = format_date(value);
        let local = value.with_timezone(&Local);
        assert_eq!(
            formatted,
            format!("{} {}, {}", local.format("%b"), local.day(), local.year())
        );
    }

    #[test]
    fn escaping_matches_path_escape() {
        assert_eq!(path_escape("abc-1_2"), "abc-1_2");
        assert_eq!(path_escape("a b"), "a%20b");
        assert_eq!(path_escape("a/b"), "a%2Fb");
        assert_eq!(path_escape("a?b"), "a%3Fb");
        assert_eq!(
            short_url("https://example.com", "a b"),
            "https://example.com/a%20b"
        );
    }

    #[test]
    fn pager_link_carries_current_controls() {
        let model = LinksModel {
            meta: PageMeta::default(),
            links: Vec::new(),
            total: 0,
            total_known: false,
            next_cursor: "abc".to_string(),
            search: "docs".to_string(),
            filter: "active".to_string(),
            sort: "slug".to_string(),
            dir: "asc".to_string(),
            base_url: "https://example.com".to_string(),
            now: Utc::now(),
        };
        assert_eq!(
            model.next_url(),
            "/manage/links?cursor=abc&dir=asc&q=docs&sort=slug&status=active"
        );
    }

    #[test]
    fn status_chips() {
        let now = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let mut item = link("a");
        assert_eq!(status_class(&item, now), "status status-active");
        item.status = Status::Disabled;
        assert_eq!(status_label(&item, now), "Disabled");
        assert_eq!(status_class(&item, now), "status status-disabled");
        item.status = Status::Active;
        item.expires_at = Some(now - chrono::Duration::hours(1));
        assert_eq!(status_label(&item, now), "Expired");
        assert_eq!(status_class(&item, now), "status status-expired");
    }

    #[test]
    fn form_action_targets() {
        let now = Utc::now();
        let mut model = form_from_link(
            PageMeta::default(),
            link("code"),
            "https://example.com",
            now,
        );
        assert_eq!(model.form_action(), "/manage/links/code");
        model.is_create = true;
        assert_eq!(model.form_action(), "/manage/links");
        let confirm = ConfirmModel {
            meta: PageMeta::default(),
            link: link("code"),
            base_url: "https://example.com".to_string(),
            rename: true,
            now,
        };
        assert_eq!(confirm.confirm_action(), "/manage/links/code/rename");
    }

    #[test]
    fn advanced_panel_opens_for_non_defaults() {
        let now = Utc::now();
        let model = form_from_link(
            PageMeta::default(),
            link("code"),
            "https://example.com",
            now,
        );
        let mut closed = model.clone();
        closed.status = "active".to_string();
        closed.redirect_status = "302".to_string();
        closed.expires_at = String::new();
        closed.notes = String::new();
        assert!(!closed.advanced_open());
        closed.notes = "note".to_string();
        assert!(closed.advanced_open());
    }
}
