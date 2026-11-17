//! `seed` — deterministic benchmark fixture loader.
//!
//! Port of `cmd/seed/main.go`. Flags, defaults, stdout and the exit codes are
//! reproduced from the Go program:
//!
//! ```text
//! -batch     transaction batch size            (default 1000)
//! -count     number of deterministic links     (default 1000000)
//! -database  SQLite database                   (default .data/benchmark.db)
//! ```
//!
//! Flags are parsed, and the usage is printed, exactly the way Go's `flag`
//! package did — see [`shorten_cli::cli`].

use std::time::Instant;

use chrono::{TimeZone, Utc};
use shorten::link::{Link, Status};
use shorten::store::Store;
use shorten_cli::cli::{self, Kind, Spec};
use shorten_cli::go_duration;

/// `flag.Int("batch", 1000, …)`, `flag.Int("count", 1_000_000, …)` and
/// `flag.String("database", ".data/benchmark.db", …)`.
const FLAGS: [Spec; 3] = [
    Spec {
        name: "batch",
        kind: Kind::Int,
        default: "1000",
        usage: "transaction batch size",
    },
    Spec {
        name: "count",
        kind: Kind::Int,
        default: "1000000",
        usage: "number of deterministic links",
    },
    Spec {
        name: "database",
        kind: Kind::String,
        default: ".data/benchmark.db",
        usage: "SQLite database",
    },
];

/// `time.Unix(1_700_000_000 + index%1_000_000, 0).UTC()`.
const FIXTURE_EPOCH: i64 = 1_700_000_000;

fn main() {
    let values = cli::parse(&FLAGS, "seed");
    let count = values.int("count");
    let batch_size = values.int("batch");
    if count < 1 || batch_size < 1 {
        cli::fatal_usage("count and batch must be positive");
    }
    let database = values.string("database");
    let batch_size = usize::try_from(batch_size).unwrap_or(usize::MAX);
    let count = usize::try_from(count).unwrap_or(usize::MAX);

    let store = match Store::open(&database, 8) {
        Ok(store) => store,
        Err(error) => cli::fatal(error),
    };
    let started = Instant::now();
    let mut batch: Vec<Link> = Vec::with_capacity(batch_size.min(1_000_000));
    for index in 0..count {
        batch.push(fixture(i64::try_from(index).unwrap_or(0)));
        if batch.len() == batch_size {
            if let Err(error) = store.import_batch(&batch, "skip") {
                cli::fatal(error);
            }
            batch.clear();
        }
    }
    if !batch.is_empty() {
        if let Err(error) = store.import_batch(&batch, "skip") {
            cli::fatal(error);
        }
    }
    let summary = match store.summary() {
        Ok(summary) => summary,
        Err(error) => cli::fatal(error),
    };
    // `fmt.Printf("seeded=%d visits=%d duration=%s database=%s\n", summary.Links,
    // summary.Visits, time.Since(started).Round(time.Millisecond), *database)`
    println!(
        "seeded={} visits={} duration={} database={}",
        summary.links,
        summary.visits,
        go_duration::format_go_duration(go_duration::round_milliseconds(started.elapsed())),
        database
    );
}

/// The deterministic record Go built for each index.
fn fixture(index: i64) -> Link {
    let raw = base36(index);
    let slug = format!("{}{}", "0".repeat(8_usize.saturating_sub(raw.len())), raw);
    let created = Utc
        .timestamp_opt(FIXTURE_EPOCH + index % 1_000_000, 0)
        .single()
        .expect("fixture timestamps are in range");
    Link {
        id: 0,
        slug: slug.clone(),
        destination_url: format!("https://example.com/resources/{slug}"),
        title: format!("Seed link {slug}"),
        notes: "deterministic benchmark fixture".to_string(),
        status: Status::Active,
        redirect_status: 302,
        expires_at: None,
        visit_count: index % 100_000,
        last_visited_at: None,
        created_at: created,
        updated_at: created,
    }
}

/// `strconv.FormatInt(index, 36)` with Go's lower-case digits. Indices are
/// non-negative here, so only the unsigned form is needed.
fn base36(value: i64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut remaining = value.unsigned_abs();
    let mut digits: Vec<u8> = Vec::new();
    while remaining > 0 {
        digits.push(DIGITS[usize::try_from(remaining % 36).unwrap_or(0)]);
        remaining /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base 36 digits are ASCII")
}

#[cfg(test)]
mod tests {
    use super::{base36, fixture, FLAGS};
    use shorten::link::Status;

    #[test]
    fn slugs_match_go_base36_padding() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(9), "9");
        assert_eq!(base36(10), "a");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(100), "2s");
        assert_eq!(base36(1_000_000), "lfls");
        assert_eq!(fixture(100).slug, "0000002s");
        assert_eq!(fixture(1_000_000).slug, "0000lfls");
    }

    #[test]
    fn fixture_records_match_go_shape() {
        let first = fixture(0);
        assert_eq!(first.slug, "00000000");
        assert_eq!(
            first.destination_url,
            "https://example.com/resources/00000000"
        );
        assert_eq!(first.title, "Seed link 00000000");
        assert_eq!(first.notes, "deterministic benchmark fixture");
        assert_eq!(first.status, Status::Active);
        assert_eq!(first.redirect_status, 302);
        assert_eq!(first.visit_count, 0);
        assert_eq!(first.created_at.timestamp(), 1_700_000_000);
        assert_eq!(first.created_at, first.updated_at);
        assert!(first.expires_at.is_none() && first.last_visited_at.is_none());

        let hundred = fixture(100);
        assert_eq!(hundred.slug, "0000002s");
        assert_eq!(hundred.title, "Seed link 0000002s");
        assert_eq!(hundred.visit_count, 100);
        assert_eq!(hundred.created_at.timestamp(), 1_700_000_100);

        let wrapped = fixture(1_000_000);
        assert_eq!(
            wrapped.slug, "0000lfls",
            "the slug is padded to eight characters"
        );
        assert_eq!(wrapped.visit_count, 0, "visits wrap every 100000");
        assert_eq!(wrapped.created_at.timestamp(), 1_700_000_000);
    }

    #[test]
    fn flag_table_keeps_go_defaults() {
        assert_eq!(FLAGS[0].default, "1000");
        assert_eq!(FLAGS[1].default, "1000000");
        assert_eq!(FLAGS[2].default, ".data/benchmark.db");
    }
}
