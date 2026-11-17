//! End-to-end parity checks for the six binaries.
//!
//! These tests drive the compiled binaries with the Go single-hyphen flag
//! spelling (`-database`, `-count`) and assert the exact stdout, stderr and exit
//! codes the Go programs produced. Nothing here needs a network service: the
//! HTTP-facing checks target loopback ports that are not listening, which is the
//! same failure the Go programs reported, and the throughput checks that need a
//! live service are gated behind `SHORTEN_CLI_LIVE_TARGET` (set it to the
//! service base URL to enable them).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Run a binary and return its captured output.
fn run(program: &str, arguments: &[&str], environment: &[(&str, String)]) -> Output {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("spawn binary")
}

/// Run a binary with its working directory set to `directory`, which is how a
/// test exercises a program's own default paths.
fn run_in(
    program: &str,
    arguments: &[&str],
    environment: &[(&str, String)],
    directory: &std::path::Path,
) -> Output {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("spawn binary")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn code_of(output: &Output) -> i32 {
    output.status.code().expect("exit status")
}

/// Go's `PrintDefaults` body for `cmd/backup`, flags sorted by name: a header
/// line with the type, then the usage indented with four spaces and a tab, with
/// the default in parentheses only when it differs from the type's zero value
/// (`-output` defaults to the empty string, so it prints no default at all).
const BACKUP_USAGE: &str = concat!(
    "  -database string\n",
    "    \tsource SQLite database (default \".data/shorten.db\")\n",
    "  -output string\n",
    "    \tbackup path\n",
);

/// `cmd/kvimport`: bool flags print no type and their `false` default is the
/// zero value, so neither is shown.
const KVIMPORT_USAGE: &str = concat!(
    "  -batch int\n",
    "    \trecords per transaction (default 500)\n",
    "  -commit\n",
    "    \twrite accepted records\n",
    "  -conflict string\n",
    "    \tabort, skip, or replace different existing records (default \"abort\")\n",
    "  -database string\n",
    "    \tSQLite database path (default \".data/shorten.db\")\n",
    "  -input string\n",
    "    \tNDJSON KV export path, or - for stdin (default \"-\")\n",
    "  -verify\n",
    "    \tverify source records after import (requires a file input)\n",
);

/// `cmd/seed`.
const SEED_USAGE: &str = concat!(
    "  -batch int\n",
    "    \ttransaction batch size (default 1000)\n",
    "  -count int\n",
    "    \tnumber of deterministic links (default 1000000)\n",
    "  -database string\n",
    "    \tSQLite database (default \".data/benchmark.db\")\n",
);

/// `cmd/bench`: a duration flag prints `(default 1m0s)`, Go's `Duration.String`.
const BENCH_USAGE: &str = concat!(
    "  -concurrency int\n",
    "    \tconcurrent keep-alive clients (default 200)\n",
    "  -duration duration\n",
    "    \ttest duration (default 1m0s)\n",
    "  -target string\n",
    "    \tredirect URL (default \"http://127.0.0.1:8080/00000000\")\n",
);

/// `cmd/querybench`: the API key's default is the empty string, so no default.
const QUERYBENCH_USAGE: &str = concat!(
    "  -api-key string\n",
    "    \tadministrator API key\n",
    "  -base-url string\n",
    "    \tservice base URL (default \"http://127.0.0.1:8080\")\n",
    "  -iterations int\n",
    "    \trequests per query scenario (default 200)\n",
);

/// The full text Go printed for `-h`, `-help` and `--help`: the header names
/// `os.Args[0]`, which is the binary path a test spawns.
fn go_usage(program: &str, body: &str) -> String {
    format!("Usage of {program}:\n{body}")
}

/// Go's usage went to standard error, exited 0, and printed nothing on standard
/// output. Both hyphen spellings are accepted.
#[test]
fn help_prints_the_go_usage_on_stderr_and_exits_zero() {
    for (program, body) in [
        (env!("CARGO_BIN_EXE_backup"), BACKUP_USAGE),
        (env!("CARGO_BIN_EXE_kvimport"), KVIMPORT_USAGE),
        (env!("CARGO_BIN_EXE_seed"), SEED_USAGE),
        (env!("CARGO_BIN_EXE_bench"), BENCH_USAGE),
        (env!("CARGO_BIN_EXE_querybench"), QUERYBENCH_USAGE),
    ] {
        for spelling in ["-h", "-help", "--help"] {
            let output = run(program, &[spelling], &[]);
            assert_eq!(code_of(&output), 0, "usage of {program} with {spelling}");
            assert_eq!(
                stderr_of(&output),
                go_usage(program, body),
                "usage of {program} with {spelling}"
            );
            assert_eq!(
                stdout_of(&output),
                "",
                "{program} must print usage on stderr only"
            );
        }
    }
}

/// `cmd/healthcheck` registered no flags and never parsed its arguments, so
/// `--help` probes the service instead of printing usage.
#[test]
fn healthcheck_ignores_every_flag() {
    let output = run(
        env!("CARGO_BIN_EXE_healthcheck"),
        &["--help"],
        &[("SHORTEN_HEALTHCHECK_URL", unreachable_url("/readyz"))],
    );
    assert_eq!(code_of(&output), 1);
    assert_eq!(stderr_of(&output), "not ready\n", "no usage is printed");
    assert_eq!(stdout_of(&output), "");
}

/// `flag provided but not defined: -<name>`, then the usage, then exit 2.
#[test]
fn unknown_flag_exits_two() {
    let output = run(env!("CARGO_BIN_EXE_backup"), &["-nope"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "flag provided but not defined: -nope\n{}",
            go_usage(env!("CARGO_BIN_EXE_backup"), BACKUP_USAGE)
        )
    );
    assert_eq!(stdout_of(&output), "");
}

/// Go rejected a flag whose body starts with `=` as `bad flag syntax`.
#[test]
fn bad_flag_syntax_exits_two() {
    let output = run(env!("CARGO_BIN_EXE_seed"), &["-=count=3"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "bad flag syntax: -=count=3\n{}",
            go_usage(env!("CARGO_BIN_EXE_seed"), SEED_USAGE)
        )
    );
}

/// Go's `flag needs an argument: -<name>` for a flag at the end of the command
/// line.
#[test]
fn missing_flag_value_exits_two() {
    let output = run(env!("CARGO_BIN_EXE_seed"), &["-count"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "flag needs an argument: -count\n{}",
            go_usage(env!("CARGO_BIN_EXE_seed"), SEED_USAGE)
        )
    );
}

/// An unparsable int value is reported as `invalid value %q for flag -%s`, with
/// strconv's `parse error` cause, then the usage.
#[test]
fn unparsable_int_exits_two() {
    let output = run(env!("CARGO_BIN_EXE_seed"), &["-count", "abc"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "invalid value \"abc\" for flag -count: parse error\n{}",
            go_usage(env!("CARGO_BIN_EXE_seed"), SEED_USAGE)
        )
    );
}

/// A value out of int64 range is strconv's other cause.
#[test]
fn out_of_range_int_exits_two() {
    let output = run(
        env!("CARGO_BIN_EXE_seed"),
        &["-count", "9223372036854775808"],
        &[],
    );
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "invalid value \"9223372036854775808\" for flag -count: value out of range\n{}",
            go_usage(env!("CARGO_BIN_EXE_seed"), SEED_USAGE)
        )
    );
}

/// A bool flag never consumes the following argument: `-commit` is true and
/// `-batch` is parsed as its own flag, which an out-of-range batch proves.
#[test]
fn bool_flags_take_no_value() {
    let directory = TempPath::new("kvimport-bool");
    let export = directory.file("export.ndjson");
    fs::write(&export, "").expect("write export");
    let database = directory.file("shorten.db");
    let export_path = export.to_string_lossy();
    let database_path = database.to_string_lossy();
    let arguments = [
        "-input",
        &export_path,
        "-database",
        &database_path,
        "-commit",
    ];

    let output = run(
        env!("CARGO_BIN_EXE_kvimport"),
        &[&arguments[..], &["-batch", "6000"][..]].concat(),
        &[],
    );
    assert_eq!(
        code_of(&output),
        1,
        "the batch value was parsed: {}",
        stdout_of(&output)
    );
    assert_eq!(stderr_of(&output), "batch must be between 1 and 5000\n");

    let output = run(
        env!("CARGO_BIN_EXE_kvimport"),
        &[&arguments[..], &["-batch", "7"][..]].concat(),
        &[],
    );
    assert_eq!(code_of(&output), 0, "{}", stderr_of(&output));
    let report = stdout_of(&output);
    assert!(
        report.contains(r#""mode":"commit""#),
        "the bool is true: {report}"
    );
    assert!(report.contains(r#""Input":0"#), "empty export: {report}");

    // `-flag=false` is Go's other spelling, and it turns the commit off.
    let output = run(
        env!("CARGO_BIN_EXE_kvimport"),
        &[&arguments[..], &[("-commit=false")][..]].concat(),
        &[],
    );
    assert_eq!(code_of(&output), 0, "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains(r#""mode":"dry-run""#),
        "explicit false: {}",
        stdout_of(&output)
    );
}

/// Go reported a bad bool spelling with `strconv.ParseBool`'s own cause.
#[test]
fn invalid_boolean_value_exits_two() {
    let output = run(env!("CARGO_BIN_EXE_kvimport"), &["-commit=yes"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "invalid boolean value \"yes\" for -commit: parsing bool: yes\n{}",
            go_usage(env!("CARGO_BIN_EXE_kvimport"), KVIMPORT_USAGE)
        )
    );
}

/// Parsing stops at a bare `-`, so the flags after it are ordinary arguments.
/// `cmd/querybench` then runs with an empty key and its own exit 2 check.
#[test]
fn parsing_stops_at_a_bare_dash() {
    let output = run(
        env!("CARGO_BIN_EXE_querybench"),
        &["-", "-api-key", "admin-key-value"],
        &[],
    );
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        "api-key and positive iterations are required\n"
    );
}

/// `--` ends the flags, so everything after it is ignored, and the defaults run:
/// standard input (closed here, so no records) into the default database path.
#[test]
fn double_dash_ends_the_flags() {
    let directory = TempPath::new("kvimport-enddash");
    let output = run_in(
        env!("CARGO_BIN_EXE_kvimport"),
        &["--", "-input", "/no/such/export.ndjson", "-commit"],
        &[],
        &directory.path,
    );
    assert_eq!(code_of(&output), 0, "{}", stderr_of(&output));
    let report = stdout_of(&output);
    assert!(report.contains(r#""source":"-""#), "defaults ran: {report}");
    assert!(
        report.contains(r#""mode":"dry-run""#),
        "defaults ran: {report}"
    );
    assert!(
        fs::File::open(directory.path.join(".data").join("shorten.db")).is_ok(),
        "the default database path was used"
    );
}

/// `kvimport` against the repository's KV export: the `link:` records import and
/// the config record is ignored, exactly as Go counted them.
#[test]
fn kvimport_reports_the_export_summary() {
    let export = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("kv-export.ndjson");
    assert!(export.is_file(), "fixture export {export:?} must exist");
    let directory = TempPath::new("kvimport-export");
    let database = directory.file("shorten.db").to_string_lossy().into_owned();
    let export_path = export.to_string_lossy().into_owned();

    let dry_run = run(
        env!("CARGO_BIN_EXE_kvimport"),
        &["-input", &export_path, "-database", &database],
        &[],
    );
    assert_eq!(code_of(&dry_run), 0, "{}", stderr_of(&dry_run));
    let report = stdout_of(&dry_run);
    assert!(
        report.contains(r#""Input":3,"Imported":2,"Identical":0,"Ignored":1,"Rejected":0"#),
        "dry-run summary: {report}"
    );
    assert!(
        report.contains(r#""mode":"dry-run""#),
        "dry-run summary: {report}"
    );

    let committed = run(
        env!("CARGO_BIN_EXE_kvimport"),
        &[
            "-input",
            &export_path,
            "-database",
            &database,
            "-commit",
            "-verify",
        ],
        &[],
    );
    assert_eq!(code_of(&committed), 0, "{}", stderr_of(&committed));
    let report = stdout_of(&committed);
    assert!(
        report.contains(r#""Input":3,"Imported":2,"Identical":0,"Ignored":1,"Rejected":0"#),
        "commit summary: {report}"
    );
    assert!(
        report.contains(r#""mode":"commit""#),
        "commit summary: {report}"
    );
    assert!(
        report.contains(r#""verified":true"#),
        "commit summary: {report}"
    );

    // A second commit finds every record already stored and identical.
    let repeat = run(
        env!("CARGO_BIN_EXE_kvimport"),
        &["-input", &export_path, "-database", &database, "-commit"],
        &[],
    );
    assert_eq!(code_of(&repeat), 0, "{}", stderr_of(&repeat));
    let report = stdout_of(&repeat);
    assert!(
        report.contains(r#""Imported":0,"Identical":2"#),
        "repeated commit: {report}"
    );
}

/// A duration outside Go's `time.ParseDuration` grammar is a flag parse failure,
/// which exits 2 in both implementations.
#[test]
fn rejects_unparsable_duration() {
    let output = run(
        env!("CARGO_BIN_EXE_bench"),
        &["-duration", "5 minutes"],
        &[],
    );
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        format!(
            "invalid value \"5 minutes\" for flag -duration: time: unknown unit \" minutes\" in duration \"5 minutes\"\n{}",
            go_usage(env!("CARGO_BIN_EXE_bench"), BENCH_USAGE)
        )
    );
}

/// `-duration -5s` parses in Go and then fails the positivity check.
#[test]
fn rejects_negative_duration() {
    let output = run(env!("CARGO_BIN_EXE_bench"), &["-duration", "-5s"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        "duration and concurrency must be positive\n"
    );
}

/// `cmd/bench` checked `-concurrency` and `-duration` before any request.
#[test]
fn bench_validates_limits() {
    let output = run(env!("CARGO_BIN_EXE_bench"), &["-concurrency", "0"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        "duration and concurrency must be positive\n"
    );

    let output = run(env!("CARGO_BIN_EXE_bench"), &["-duration", "0s"], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        "duration and concurrency must be positive\n"
    );
}

/// `cmd/querybench` required an API key and a positive iteration count.
#[test]
fn querybench_validates_limits() {
    let output = run(env!("CARGO_BIN_EXE_querybench"), &[], &[]);
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        "api-key and positive iterations are required\n"
    );

    let output = run(
        env!("CARGO_BIN_EXE_querybench"),
        &["-api-key", "admin-key-value", "-iterations", "0"],
        &[],
    );
    assert_eq!(code_of(&output), 2);
    assert_eq!(
        stderr_of(&output),
        "api-key and positive iterations are required\n"
    );
}

/// `cmd/seed` rejected non-positive counts with exit status 2.
#[test]
fn seed_validates_limits() {
    let output = run(
        env!("CARGO_BIN_EXE_seed"),
        &[
            "-count",
            "0",
            "-database",
            &directory_file("seed-validation.db"),
        ],
        &[],
    );
    assert_eq!(code_of(&output), 2);
    assert_eq!(stderr_of(&output), "count and batch must be positive\n");

    let output = run(
        env!("CARGO_BIN_EXE_seed"),
        &[
            "-batch",
            "-1",
            "-database",
            &directory_file("seed-validation.db"),
        ],
        &[],
    );
    assert_eq!(code_of(&output), 2);
    assert_eq!(stderr_of(&output), "count and batch must be positive\n");
}

/// `cmd/seed` printed one summary line built from the store summary.
#[test]
fn seed_reports_its_fixture_counts() {
    let directory = TempPath::new("seed");
    let database = directory.file("benchmark.db");
    let output = run(
        env!("CARGO_BIN_EXE_seed"),
        &[
            "-database",
            &database.to_string_lossy(),
            "-count",
            "25",
            "-batch",
            "10",
        ],
        &[],
    );
    assert_eq!(code_of(&output), 0, "{}", stderr_of(&output));
    let line = stdout_of(&output);
    let prefix = "seeded=25 visits=300 duration=";
    assert!(line.starts_with(prefix), "unexpected seed report {line}");
    assert!(
        line.ends_with(&format!(" database={}\n", database.to_string_lossy())),
        "seed report must end with the database: {line}"
    );
    let duration = line.trim_end().rsplit(' ').nth(1).expect("duration field");
    assert!(
        duration.ends_with('s'),
        "duration {duration} must use Go's duration units"
    );
}

/// `cmd/backup` printed the backup, manifest and store totals, and wrote a
/// readable manifest next to the snapshot.
#[test]
fn backup_writes_snapshot_and_manifest() {
    let directory = TempPath::new("backup");
    let database = directory.file("shorten.db");
    let seeded = run(
        env!("CARGO_BIN_EXE_seed"),
        &[
            "-database",
            &database.to_string_lossy(),
            "-count",
            "6",
            "-batch",
            "3",
        ],
        &[],
    );
    assert_eq!(code_of(&seeded), 0, "{}", stderr_of(&seeded));

    let output_path = directory.file("backups").join("snapshot.db");
    let output = run(
        env!("CARGO_BIN_EXE_backup"),
        &[
            "-database",
            &database.to_string_lossy(),
            "-output",
            &output_path.to_string_lossy(),
        ],
        &[],
    );
    assert_eq!(code_of(&output), 0, "{}", stderr_of(&output));
    let manifest_path = format!("{}.manifest.json", output_path.to_string_lossy());
    assert_eq!(
        stdout_of(&output),
        format!(
            "backup={} manifest={} links=6 visits=15\n",
            output_path.to_string_lossy(),
            manifest_path
        )
    );
    assert!(output_path.is_file(), "snapshot must exist");

    let manifest = fs::read_to_string(&manifest_path).expect("read manifest");
    assert!(manifest.ends_with('\n'), "manifest ends with a newline");
    assert!(
        manifest.contains("  \"summary\": {"),
        "manifest uses Go's indent: {manifest}"
    );
    assert!(
        manifest.contains("\"schemaVersion\": 3"),
        "manifest carries the schema version"
    );
    // Go's manifest struct left `Source` and `Backup` untagged, so the keys keep
    // the Go field spellings next to the tagged `createdAt` and `summary`.
    assert!(
        manifest.contains(&format!(
            "\"Backup\": \"{}\"",
            output_path.to_string_lossy()
        )),
        "manifest names the snapshot: {manifest}"
    );
    assert!(
        manifest.contains(&format!("\"Source\": \"{}\"", database.to_string_lossy())),
        "manifest names the source database: {manifest}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&manifest_path)
            .expect("manifest metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640, "manifest mode must be 0640");
    }
}

/// `cmd/backup` failed with the store error and exit status 1.
///
/// The case is a directory the process may not create. An unprivileged user
/// cannot create one at the root of the filesystem, so that is the shape the
/// assertion checks; a privileged user — the user a container build runs as —
/// can, so for that user the same invocation succeeds and the assertions
/// follow the success path. The probe below asks the operating system which
/// case applies instead of assuming an unprivileged environment.
#[test]
fn backup_reports_store_failures() {
    let directory = TempPath::new("backup-denied");
    let blocked = std::path::Path::new("/shorten-cli-backup-denied");
    let denied = match std::fs::create_dir(blocked) {
        Ok(()) => {
            let _ = std::fs::remove_dir(blocked);
            false
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            true
        }
        Err(error) => panic!("probe the blocked directory: {error}"),
    };
    let output = run(
        env!("CARGO_BIN_EXE_backup"),
        &[
            "-database",
            &directory.file("shorten.db").to_string_lossy(),
            "-output",
            "/shorten-cli-backup-denied/snapshot.db",
        ],
        &[],
    );
    if denied {
        assert_eq!(code_of(&output), 1);
        assert!(
            stderr_of(&output).contains("mkdir /shorten-cli-backup-denied: permission denied"),
            "the store failure must be reported the way Go printed it: {}",
            stderr_of(&output)
        );
        return;
    }

    // The privileged case: the directory is creatable, so the snapshot is
    // written and reported, and the probe leaves nothing behind.
    assert_eq!(
        code_of(&output),
        0,
        "backup as a privileged user: {output:?}"
    );
    assert!(
        stdout_of(&output).contains("backup=/shorten-cli-backup-denied/snapshot.db"),
        "the snapshot is reported: {}",
        stdout_of(&output)
    );
    let _ = std::fs::remove_dir_all(blocked);
}

/// `cmd/healthcheck` printed `not ready` and exited 1 for an unreachable probe.
#[test]
fn healthcheck_reports_unready_service() {
    let output = run(
        env!("CARGO_BIN_EXE_healthcheck"),
        &[],
        &[("SHORTEN_HEALTHCHECK_URL", unreachable_url("/readyz"))],
    );
    assert_eq!(code_of(&output), 1);
    assert_eq!(stderr_of(&output), "not ready\n");
    assert_eq!(stdout_of(&output), "", "success path prints nothing");
}

/// `cmd/bench` against a closed port: the report line is printed, and every
/// request counts as a failure, which drives the exit status to 1.
#[test]
fn bench_reports_failed_requests() {
    let output = run(
        env!("CARGO_BIN_EXE_bench"),
        &[
            "-target",
            &unreachable_url("/00000000"),
            "-duration",
            "200ms",
            "-concurrency",
            "2",
        ],
        &[],
    );
    assert_eq!(code_of(&output), 1, "{}", stdout_of(&output));
    let line = stdout_of(&output);
    assert!(
        line.starts_with("target="),
        "report starts with target: {line}"
    );
    for field in [
        " duration=",
        " concurrency=2",
        " requests=",
        " rps=",
        " p50=",
        " p95=",
        " p99=",
        " errors=",
        " error_rate=100.000%",
        " go=rustc",
        " os=",
        " arch=",
    ] {
        assert!(line.contains(field), "report omits {field}: {line}");
    }
    assert!(line.ends_with('\n'), "report line is terminated");
    let requests = field_after(&line, " requests=").expect("requests field");
    let errors = field_after(&line, " errors=").expect("errors field");
    assert_eq!(requests, errors, "every failed request counts as an error");
}

/// `cmd/querybench` against a closed port reported the scenario failure and
/// exit status 1, having printed nothing for that scenario.
#[test]
fn querybench_reports_transport_failures() {
    let output = run(
        env!("CARGO_BIN_EXE_querybench"),
        &[
            "-base-url",
            &unreachable_url(""),
            "-api-key",
            "admin-key-value",
            "-iterations",
            "3",
        ],
        &[],
    );
    assert_eq!(code_of(&output), 1);
    assert_eq!(stdout_of(&output), "", "no scenario completed");
    let line = stderr_of(&output);
    assert!(
        line.starts_with("updated request: "),
        "scenario failure line: {line}"
    );
    assert!(line.ends_with('\n'), "failure line is terminated");
}

/// Live-service throughput checks. Off by default so `cargo test` passes offline.
#[test]
fn live_benchmarks_are_skipped_without_a_target() {
    let base = std::env::var("SHORTEN_CLI_LIVE_TARGET").ok();
    let Some(base) = base else {
        eprintln!("SHORTEN_CLI_LIVE_TARGET is unset; skipping live throughput checks");
        return;
    };
    let seeded = run(
        env!("CARGO_BIN_EXE_seed"),
        &[
            "-database",
            ".data/benchmark.db",
            "-count",
            "1000",
            "-batch",
            "500",
        ],
        &[],
    );
    assert_eq!(code_of(&seeded), 0, "{}", stderr_of(&seeded));

    let bench = run(
        env!("CARGO_BIN_EXE_bench"),
        &[
            "-target",
            &format!("{base}/00000000"),
            "-duration",
            "2s",
            "-concurrency",
            "20",
        ],
        &[],
    );
    let report = stdout_of(&bench);
    assert!(report.starts_with("target="), "bench report: {report}");

    let key = std::env::var("SHORTEN_CLI_LIVE_API_KEY").unwrap_or_default();
    if key.is_empty() {
        eprintln!("SHORTEN_CLI_LIVE_API_KEY is unset; skipping querybench");
        return;
    }
    let querybench = run(
        env!("CARGO_BIN_EXE_querybench"),
        &["-base-url", &base, "-api-key", &key, "-iterations", "10"],
        &[],
    );
    let report = stdout_of(&querybench);
    for scenario in [
        "updated",
        "active",
        "expired",
        "visits",
        "destination",
        "fts",
    ] {
        assert!(
            report.contains(&format!("scenario={scenario} iterations=10")),
            "querybench report omits {scenario}: {report}"
        );
    }
}

/// A path under the system temp dir; validation rejects the run before the
/// database is opened, so nothing is created.
fn directory_file(name: &str) -> String {
    std::env::temp_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// A loopback URL nothing is listening on, so the transport fails deterministically.
fn unreachable_url(path: &str) -> String {
    format!("http://127.0.0.1:1{path}")
}

/// The text after `field=` up to the next space in a `key=value` report line.
fn field_after(line: &str, field: &str) -> Option<String> {
    let (_, tail) = line.split_once(field)?;
    Some(tail.split(' ').next()?.to_string())
}

/// A unique directory under the system temp dir, removed when the test ends.
struct TempPath {
    path: PathBuf,
}

impl TempPath {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("shorten-cli-{tag}-{}-{unique}", std::process::id()));
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
