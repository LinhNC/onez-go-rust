//! Shared implementation behind the Shorten operational binaries.
//!
//! Each program under `src/bin/` is a byte-for-byte port of the matching Go
//! program under `cmd/` in the original service: the same flags, the same
//! defaults, the same stdout/stderr lines and the same exit codes. The helpers
//! here hold the pieces more than one binary needs — the Go `flag` grammar and
//! usage printer, the `time.Duration` parser and text formatter the benchmark
//! and seed reports print, the `json.Marshal` escaping the machine-readable
//! output needs, and the KV importer core that the `kvimport` binary drives.
//!
//! # Flag spelling
//!
//! Go's `flag` package accepts a long flag written with one hyphen (`-database`)
//! and also accepts the two-hyphen form (`--database`); a value attaches with
//! `=` (`-database=path`) or follows as the next argument (`-database path`),
//! except for bool flags. [`cli::FlagSet`] implements that grammar, so every
//! command line documented for the Go programs runs unchanged against these
//! binaries, and so does `--help`, whose text Go printed to standard error.

pub mod cli;
pub mod go_duration;
pub mod kvimport;
