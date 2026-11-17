//! `kvimport` — migrate the legacy KV export into the SQLite store.
//!
//! Port of `cmd/kvimport/main.go`. Flags, defaults, the single JSON report
//! line and the exit codes are reproduced from the Go program:
//!
//! ```text
//! -batch     records per transaction                     (default 500)
//! -commit    write accepted records
//! -conflict  abort, skip, or replace …                   (default abort)
//! -database  SQLite database path                        (default .data/shorten.db)
//! -input     NDJSON KV export path, or - for stdin       (default -)
//! -verify    verify source records after import …
//! ```
//!
//! Flags are parsed, and the usage is printed, exactly the way Go's `flag`
//! package did — see [`shorten_cli::cli`]. Both validation messages Go checked
//! before opening the store exit 1, like every other failure of `run`.

use shorten_cli::cli::{self, fatal, Kind, Spec, Values};
use shorten_cli::kvimport::{execute, Options};

/// The six flags `cmd/kvimport` registered, in Go's spelling.
const FLAGS: [Spec; 6] = [
    Spec {
        name: "batch",
        kind: Kind::Int,
        default: "500",
        usage: "records per transaction",
    },
    Spec {
        name: "commit",
        kind: Kind::Bool,
        default: "false",
        usage: "write accepted records",
    },
    Spec {
        name: "conflict",
        kind: Kind::String,
        default: "abort",
        usage: "abort, skip, or replace different existing records",
    },
    Spec {
        name: "database",
        kind: Kind::String,
        default: ".data/shorten.db",
        usage: "SQLite database path",
    },
    Spec {
        name: "input",
        kind: Kind::String,
        default: "-",
        usage: "NDJSON KV export path, or - for stdin",
    },
    Spec {
        name: "verify",
        kind: Kind::Bool,
        default: "false",
        usage: "verify source records after import (requires a file input)",
    },
];

fn main() {
    let values = cli::parse(&FLAGS, "kvimport");
    let options = options_of(&values);
    match options.validate().and_then(|()| execute(&options)) {
        Ok(()) => {}
        Err(message) => {
            // `main` printed the error from `run` and exited 1.
            fatal(message);
        }
    }
}

/// The importer options, taken from the parsed flags.
fn options_of(values: &Values) -> Options {
    Options {
        input: values.string("input"),
        database: values.string("database"),
        commit: values.flag("commit"),
        policy: values.string("conflict"),
        batch: values.int("batch"),
        verify: values.flag("verify"),
    }
}

#[cfg(test)]
mod tests {
    use super::{options_of, FLAGS};
    use shorten_cli::cli::{FlagSet, Values};

    fn parse(arguments: &[&str]) -> Values {
        FlagSet::new("kvimport", &FLAGS).parse(
            &arguments
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn defaults_match_the_go_program() {
        let options = options_of(&parse(&[]));
        assert_eq!(options.input, "-");
        assert_eq!(options.database, ".data/shorten.db");
        assert_eq!(options.policy, "abort");
        assert_eq!(options.batch, 500);
        assert!(!options.commit && !options.verify);
        assert_eq!(options.validate(), Ok(()));
    }

    #[test]
    fn go_spellings_reach_the_options() {
        let options = options_of(&parse(&[
            "-input",
            "kv-export.ndjson",
            "-commit",
            "-conflict=skip",
            "-batch",
            "10",
            "-verify=true",
        ]));
        assert_eq!(options.input, "kv-export.ndjson");
        assert!(options.commit && options.verify);
        assert_eq!(options.policy, "skip");
        assert_eq!(options.batch, 10);
    }

    #[test]
    fn validation_rejects_out_of_range_batches() {
        let mut options = options_of(&parse(&[]));
        options.batch = 0;
        assert_eq!(
            options.validate().err(),
            Some("batch must be between 1 and 5000".to_string())
        );
        options.batch = 5_001;
        assert_eq!(
            options.validate().err(),
            Some("batch must be between 1 and 5000".to_string())
        );
        options.batch = 5_000;
        assert_eq!(options.validate(), Ok(()));
    }

    #[test]
    fn validation_rejects_unknown_policies() {
        let mut options = options_of(&parse(&[]));
        options.policy = "Abort".to_string();
        assert_eq!(
            options.validate().err(),
            Some("conflict must be abort, skip, or replace".to_string())
        );
        for policy in ["abort", "skip", "replace"] {
            options.policy = policy.to_string();
            assert_eq!(options.validate(), Ok(()), "{policy} must be accepted");
        }
    }
}
