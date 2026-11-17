//! `backup` — online SQLite snapshot with an integrity check and manifest.
//!
//! Port of `cmd/backup/main.go`. Flags, defaults, stdout and stderr lines and
//! the exit codes are reproduced from the Go program:
//!
//! ```text
//! -database  source SQLite database   (default .data/shorten.db)
//! -output    backup path              (no default: stamped under .data/backups)
//! ```
//!
//! Flags are parsed, and the usage is printed, exactly the way Go's `flag`
//! package did — see [`shorten_cli::cli`].

use std::fs::{self, File};
use std::io::Write;

use chrono::{DateTime, Utc};
use serde::Serialize;
use shorten::store::{Store, Summary};
use shorten_cli::cli::{self, go_escaped_json, Kind, Spec, Values};

/// `flag.String("database", ".data/shorten.db", "source SQLite database")` and
/// `flag.String("output", "", "backup path")`.
const FLAGS: [Spec; 2] = [
    Spec {
        name: "database",
        kind: Kind::String,
        default: ".data/shorten.db",
        usage: "source SQLite database",
    },
    Spec {
        name: "output",
        kind: Kind::String,
        default: "",
        usage: "backup path",
    },
];

/// `manifest` in the Go program: `MarshalIndent` with two-space indent, so the
/// untagged `Source` and `Backup` fields keep their Go field names.
#[derive(Serialize)]
struct Manifest {
    #[serde(rename = "createdAt", with = "shorten::json::time")]
    created_at: DateTime<Utc>,
    #[serde(rename = "Source")]
    source: String,
    #[serde(rename = "Backup")]
    backup: String,
    #[serde(rename = "summary")]
    summary: Summary,
}

fn main() {
    let values = cli::parse(&FLAGS, "backup");
    if let Err(message) = run(&values) {
        cli::fatal(message);
    }
}

fn run(values: &Values) -> Result<(), String> {
    let database = values.string("database");
    let output = if values.string("output").is_empty() {
        // `filepath.Join(".data", "backups", "shorten-"+time.Now().UTC().Format(...)+".db")`
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        format!(".data/backups/shorten-{stamp}.db")
    } else {
        values.string("output")
    };

    let store = Store::open(&database, 2).map_err(message)?;
    store.backup(&output).map_err(message)?;
    let backup = Store::open(&output, 1).map_err(message)?;
    backup.integrity_check().map_err(message)?;
    let summary = backup.summary().map_err(message)?;

    let manifest = Manifest {
        created_at: Utc::now(),
        source: database,
        backup: output.clone(),
        summary,
    };
    let manifest_path = format!("{output}.manifest.json");
    write_manifest(&manifest_path, &manifest)?;

    println!(
        "backup={output} manifest={manifest_path} links={} visits={}",
        summary.links, summary.visits
    );
    Ok(())
}

/// `json.MarshalIndent(v, "", "  ")` plus a trailing newline, written `0o640`.
/// Go's encoder also escapes `<`, `>` and `&`, which `serde_json` leaves plain.
fn write_manifest(path: &str, manifest: &Manifest) -> Result<(), String> {
    let data = go_escaped_json(&serde_json::to_string_pretty(manifest).map_err(message)?);
    let mut file = File::create(path).map_err(message)?;
    file.write_all(data.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(message)?;
    file.flush().map_err(message)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o640))
            .map_err(message)?;
    }
    Ok(())
}

fn message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use shorten_cli::cli::go_escaped_json;

    use super::{Manifest, FLAGS};
    use shorten::store::Summary;

    #[test]
    fn manifest_field_names_follow_go() {
        let manifest = Manifest {
            created_at: "2026-08-01T10:00:00Z".parse().expect("timestamp"),
            source: ".data/shorten.db".to_string(),
            backup: "/tmp/a.db".to_string(),
            summary: Summary {
                links: 6,
                visits: 15,
                schema_version: 3,
            },
        };
        let encoded = serde_json::to_string_pretty(&manifest).expect("encode");
        assert_eq!(
            encoded,
            "{\n  \"createdAt\": \"2026-08-01T10:00:00Z\",\n  \"Source\": \
             \".data/shorten.db\",\n  \"Backup\": \"/tmp/a.db\",\n  \
             \"summary\": {\n    \"links\": 6,\n    \"visits\": 15,\n    \
             \"schemaVersion\": 3\n  }\n}"
        );
    }

    #[test]
    fn escapes_html_like_go_encoder() {
        assert_eq!(
            go_escaped_json(r#"{"path":"a&b<c>d"}"#),
            r#"{"path":"a\u0026b\u003cc\u003ed"}"#
        );
    }

    #[test]
    fn flag_table_keeps_go_defaults() {
        assert_eq!(FLAGS[0].default, ".data/shorten.db");
        assert_eq!(FLAGS[1].default, "");
        assert_eq!(FLAGS[1].usage, "backup path");
    }
}
