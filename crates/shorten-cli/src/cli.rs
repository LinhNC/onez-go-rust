//! Command-line plumbing shared by every binary.
//!
//! The binaries are ports of Go programs built with the standard `flag`
//! package, so this module reproduces that package's grammar and its exact
//! messages instead of delegating to an argument parser that would phrase them
//! differently:
//!
//! * long flags are spelled with one hyphen (`-database`) and the two-hyphen
//!   form (`--database`) is equivalent;
//! * a value may be attached with `=` (`-database=path`) or follow as the next
//! * argument (`-database path`), the latter not allowed for bool flags;
//! * parsing stops before the first non-flag argument and after `--`;
//! * `-h` / `-help` prints the usage to standard error and exits 0, every other
//!   parse failure prints the message, then the usage, and exits 2;
//! * the usage text is `flag.FlagSet.PrintDefaults` layout: `Usage of <argv0>:`,
//!   then one flag header line per flag in sorted order and the usage text, with
//!   the parenthesised default only when it differs from the type's zero value.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::exit;

/// The exit status Go's `flag.Parse` used for a parse failure.
pub const EXIT_PARSE: i32 = 2;

/// The kind of a flag: it decides the type name Go prints in the usage header
/// line and the way the value text is decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `flag.String`
    String,
    /// `flag.Int`
    Int,
    /// `flag.Bool`
    Bool,
    /// `flag.Duration`
    Duration,
}

impl Kind {
    /// The type name `flag.UnquoteUsage` guesses for this kind; empty for bool
    /// flags, which Go prints without a type.
    const fn type_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Int => "int",
            Self::Bool => "",
            Self::Duration => "duration",
        }
    }

    /// The text of the type's zero value: `flag.isZeroValue` prints the
    /// parenthesised default only when the default differs from it.
    const fn zero_text(self) -> &'static str {
        match self {
            Self::String => "",
            Self::Int => "0",
            Self::Bool => "false",
            Self::Duration => "0s",
        }
    }
}

/// One flag of a program's flag set, exactly as Go registered it.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// The flag name as it appears on the command line.
    pub name: &'static str,
    /// How Go declared the flag.
    pub kind: Kind,
    /// The default value text, as Go stored it in `Flag.DefValue`.
    pub default: &'static str,
    /// The help text Go printed after the flag header line.
    pub usage: &'static str,
}

/// A program's flag set: the registered flags and the program name Go prints in
/// the usage header.
#[derive(Clone, Debug)]
pub struct FlagSet {
    specs: Vec<Spec>,
    program: String,
}

/// The parsed values of a flag set, keeping the defaults for flags that were
/// absent from the command line.
#[derive(Clone, Debug)]
pub struct Values {
    specs: Vec<Spec>,
    set: HashMap<String, String>,
}

impl FlagSet {
    /// Build the flag set of `program` (the invoked path, as Go used
    /// `os.Args[0]`) from the flags the Go program registered.
    #[must_use]
    pub fn new(program: impl Into<String>, specs: &[Spec]) -> Self {
        Self {
            specs: specs.to_vec(),
            program: program.into(),
        }
    }

    /// Parse `args` the way `flag.FlagSet.Parse` did.
    ///
    /// Help and parse failures are terminal: the usage, and for a failure the
    /// message before it, is written to standard error and the process exits
    /// with Go's status (0 for `-h`/`-help`, 2 otherwise).
    #[must_use]
    pub fn parse(&self, args: &[String]) -> Values {
        let mut set: HashMap<String, String> = HashMap::new();
        let args: Vec<String> = args.to_vec();
        let mut index = 0_usize;
        while index < args.len() {
            let argument = args[index].clone();
            if !is_flag_argument(&argument) {
                break;
            }
            index += 1;
            let mut minuses = 1_usize;
            if argument.as_bytes()[1] == b'-' {
                minuses += 1;
                if argument.len() == 2 {
                    // "--" terminates the flags; the rest of the arguments are
                    // ignored, exactly as Go's `parseOne` reported.
                    break;
                }
            }
            let body = &argument[minuses..];
            if body.is_empty() || body.starts_with('-') || body.starts_with('=') {
                self.fail(&format!("bad flag syntax: {argument}"));
            }
            let (name, value) = split_flag_value(body);
            let Some(spec) = self.specs.iter().find(|spec| spec.name == name) else {
                if name == "help" || name == "h" {
                    eprint!("{}", self.usage());
                    exit(0);
                }
                self.fail(&format!("flag provided but not defined: -{name}"));
            };
            match spec.kind {
                Kind::Bool => {
                    let text = value.unwrap_or("true");
                    if parse_go_bool(text).is_none() {
                        // Go's flag package special-cased bool flags, and
                        // `strconv.ParseBool`'s own cause is `parsing bool: <text>`.
                        self.fail(&format!(
                            "invalid boolean value {} for -{}: parsing bool: {text}",
                            go_quote(text),
                            spec.name
                        ));
                    }
                    set.insert(spec.name.to_string(), text.to_string());
                }
                Kind::String | Kind::Int | Kind::Duration => {
                    let text = match value {
                        Some(value) => value.to_string(),
                        None => {
                            if index < args.len() {
                                let value = args[index].clone();
                                index += 1;
                                value
                            } else {
                                self.fail(&format!("flag needs an argument: -{}", spec.name));
                            }
                        }
                    };
                    if let Err(reason) = check_kind(spec.kind, &text) {
                        self.fail(&format!(
                            "invalid value {} for flag -{}: {reason}",
                            go_quote(&text),
                            spec.name
                        ));
                    }
                    set.insert(spec.name.to_string(), text);
                }
            }
        }
        Values {
            specs: self.specs.clone(),
            set,
        }
    }

    /// The usage text `defaultUsage` printed: the header line plus
    /// `PrintDefaults`, ending with a newline.
    #[must_use]
    pub fn usage(&self) -> String {
        let mut text = format!("Usage of {}:\n", self.program);
        let mut specs = self.specs.clone();
        specs.sort_by_key(|spec| spec.name);
        for spec in specs {
            let mut line = format!("  -{}", spec.name);
            let type_name = spec.kind.type_name();
            if !type_name.is_empty() {
                line.push(' ');
                line.push_str(type_name);
            }
            // A bool flag named with one ASCII letter keeps its usage on the
            // header line; everything else moves to the indented continuation.
            if line.len() <= 4 {
                line.push('\t');
            } else {
                line.push_str("\n    \t");
            }
            line.push_str(&spec.usage.replace('\n', "\n    \t"));
            if spec.default != spec.kind.zero_text() {
                if spec.kind == Kind::String {
                    let _ = write!(line, " (default {})", go_quote(spec.default));
                } else {
                    let _ = write!(line, " (default {})", spec.default);
                }
            }
            line.push('\n');
            text.push_str(&line);
        }
        text
    }

    /// Print `message` and the usage to standard error, then exit 2, the way
    /// `flag.FlagSet.failf` did.
    fn fail(&self, message: &str) -> ! {
        eprintln!("{message}");
        eprint!("{}", self.usage());
        exit(EXIT_PARSE);
    }
}

impl Values {
    /// The text of a `flag.String` value.
    #[must_use]
    pub fn string(&self, name: &str) -> String {
        self.text(name).unwrap_or_else(|| self.default(name))
    }

    /// The value of a `flag.Int` flag, parsed with Go's base-detecting integer
    /// grammar.
    #[must_use]
    pub fn int(&self, name: &str) -> i64 {
        let text = self.text(name).unwrap_or_else(|| self.default(name));
        parse_go_int(&text).unwrap_or(0)
    }

    /// The value of a `flag.Bool` flag.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        let text = self.text(name).unwrap_or_else(|| self.default(name));
        parse_go_bool(&text).unwrap_or(false)
    }

    /// The value of a `flag.Duration` flag in nanoseconds, which keeps Go's
    /// signed duration so a program can reject `-5s` with its own check.
    #[must_use]
    pub fn duration_nanos(&self, name: &str) -> i128 {
        let text = self.text(name).unwrap_or_else(|| self.default(name));
        crate::go_duration::parse_go_duration(&text).unwrap_or(0)
    }

    fn text(&self, name: &str) -> Option<String> {
        self.set.get(name).cloned()
    }

    fn default(&self, name: &str) -> String {
        self.specs
            .iter()
            .find(|spec| spec.name == name)
            .map(|spec| spec.default.to_string())
            .unwrap_or_default()
    }
}

/// `os.Args[0]`, which Go used as the flag set name and printed in the usage
/// header. Falls back to the crate name when the operating system handed us an
/// empty program name.
#[must_use]
pub fn program_name(fallback: &str) -> String {
    std::env::args()
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Parse the process arguments for `specs`, using the invoked path as the
/// program name in the usage text.
#[must_use]
pub fn parse(specs: &[Spec], fallback: &str) -> Values {
    FlagSet::new(program_name(fallback), specs).parse(&std::env::args().skip(1).collect::<Vec<_>>())
}

/// Report an operational failure exactly as Go's `fatal` helper did: the
/// message on standard error and exit status 1.
pub fn fatal(message: impl std::fmt::Display) -> ! {
    eprintln!("{message}");
    exit(1);
}

/// Report a validation failure exactly as the Go programs' own checks did: the
/// message on standard error and exit status 2.
pub fn fatal_usage(message: impl std::fmt::Display) -> ! {
    eprintln!("{message}");
    exit(EXIT_PARSE);
}

/// Go's `json.Marshal` / `json.Encoder` HTML escaping: `<`, `>` and `&` are
/// written as `\u003c`, `\u003e` and `\u0026`. They can only appear inside JSON
/// string literals, so a plain pass over the encoded text reproduces Go's bytes.
#[must_use]
pub fn go_escaped_json(data: &str) -> String {
    data.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// Go's `parseOne` gate: an argument is a flag only when it is at least two
/// bytes long and starts with a hyphen. A bare `-` — standard input in these
/// programs — is a non-flag argument, which stops flag parsing.
fn is_flag_argument(argument: &str) -> bool {
    argument.len() >= 2 && argument.starts_with('-')
}

/// Split `-name=value` at the first `=` after the name. Go scans bytes from
/// index 1, so `=` as the first byte of the body is a syntax error (caught by
/// the caller) and a value may itself contain `=`.
fn split_flag_value(body: &str) -> (&str, Option<&str>) {
    match body.as_bytes()[1..].iter().position(|byte| *byte == b'=') {
        Some(offset) => {
            let at = offset + 1;
            (&body[..at], Some(&body[at + 1..]))
        }
        None => (body, None),
    }
}

/// `flag.isZeroValue`-style validation of a value text for this kind. The
/// reason text matches the message Go appended to `invalid value ...`.
fn check_kind(kind: Kind, text: &str) -> Result<(), String> {
    match kind {
        Kind::String => Ok(()),
        Kind::Int => parse_go_int(text).map(|_| ()),
        Kind::Bool => parse_go_bool(text)
            .map(|_| ())
            .ok_or_else(|| format!("parsing bool: {text}")),
        // `flag.Duration` wrapped `time.ParseDuration`, so Go printed that
        // package's message verbatim after `invalid value ... for flag -name:`.
        Kind::Duration => crate::go_duration::parse_go_duration(text).map(|_| ()),
    }
}

/// `strconv.ParseBool`'s accepted spellings.
#[must_use]
pub fn parse_go_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// `strconv.ParseInt(s, 0, 64)`: a leading sign, then `0x`/`0o`/`0b` prefixed
/// digits, a leading zero for octal, plain decimal, and underscores allowed only
/// beside an explicit base prefix. The reason text matches the message Go's
/// `flag` package appended.
///
/// # Errors
///
/// Returns `"parse error"` for a syntax failure and `"value out of range"` when
/// the magnitude exceeds `int64`.
pub fn parse_go_int(text: &str) -> Result<i64, String> {
    let syntax = || "parse error".to_string();
    let bytes = text.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'+') => (false, &bytes[1..]),
        Some(b'-') => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return Err(syntax());
    }
    let mut base: u32 = 10;
    let mut explicit = false;
    let digits = match (digits.first(), digits.get(1)) {
        (Some(b'0'), Some(second)) => match second {
            b'x' | b'X' => {
                base = 16;
                explicit = true;
                &digits[2..]
            }
            b'o' | b'O' => {
                base = 8;
                explicit = true;
                &digits[2..]
            }
            b'b' | b'B' => {
                base = 2;
                explicit = true;
                &digits[2..]
            }
            // A leading zero with no base prefix is octal, as in Go's
            // `baseForBase`; underscores stay disallowed without a prefix.
            _ => {
                base = 8;
                digits
            }
        },
        _ => digits,
    };
    if digits.is_empty() {
        return Err(syntax());
    }
    let limit: u128 = if negative { 1 << 63 } else { (1 << 63) - 1 };
    let mut value: u128 = 0;
    let mut seen_digit = false;
    let mut previous_underscore = false;
    for (index, byte) in digits.iter().enumerate() {
        if *byte == b'_' {
            if !explicit || index == 0 || index + 1 == digits.len() || previous_underscore {
                return Err(syntax());
            }
            previous_underscore = true;
            continue;
        }
        previous_underscore = false;
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'z' => u32::from(byte - b'a') + 10,
            b'A'..=b'Z' => u32::from(byte - b'A') + 10,
            _ => return Err(syntax()),
        };
        if digit >= base {
            return Err(syntax());
        }
        let magnitude = u128::from(base);
        value = value
            .checked_mul(magnitude)
            .and_then(|value| value.checked_add(u128::from(digit)))
            .unwrap_or(limit + 1);
        seen_digit = true;
        if value > limit {
            return Err("value out of range".to_string());
        }
    }
    if !seen_digit {
        return Err(syntax());
    }
    let magnitude = i128::try_from(value).unwrap_or(i128::MAX);
    let signed = if negative { -magnitude } else { magnitude };
    Ok(i64::try_from(signed).unwrap_or(i64::MIN))
}

/// `strconv.Quote` as Go's `%q` verb used it: ASCII control characters become
/// `\a`, `\b`, `\f`, `\n`, `\r`, `\t`, `\v` or `\xNN`, the quote and backslash
/// are escaped, printable non-ASCII text is kept, and other runes are written
/// with `\uXXXX` / `\UXXXXXXXX`.
#[must_use]
pub fn go_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\u{7}' => quoted.push_str("\\a"),
            '\u{8}' => quoted.push_str("\\b"),
            '\u{c}' => quoted.push_str("\\f"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{b}' => quoted.push_str("\\v"),
            c if c.is_ascii_graphic() || c == ' ' => quoted.push(c),
            c if c.is_ascii() => {
                let _ = write!(quoted, "\\x{:02x}", u32::from(c));
            }
            c if is_printable(c) => quoted.push(c),
            c if u32::from(c) <= 0xFFFF => {
                let _ = write!(quoted, "\\u{:04x}", u32::from(c));
            }
            c => {
                let _ = write!(quoted, "\\U{:08x}", u32::from(c));
            }
        }
    }
    quoted.push('"');
    quoted
}

/// `time.quote`, which Go used inside the `time.ParseDuration` messages: it
/// keeps printable ASCII and writes every non-ASCII or control rune as one `?`
/// per UTF-8 byte.
#[must_use]
pub fn go_time_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        if character.is_ascii_graphic() || character == ' ' {
            quoted.push(character);
        } else {
            quoted.push_str(&"?".repeat(character.len_utf8()));
        }
    }
    quoted.push('"');
    quoted
}

/// `strconv.Quote`'s printability test approximated for the runes these
/// programs can see in a flag value.
fn is_printable(character: char) -> bool {
    !character.is_control() && !matches!(character, '\u{ad}' | '\u{a0}')
}

#[cfg(test)]
mod tests {
    use super::{
        check_kind, go_quote, go_time_quote, is_flag_argument, parse_go_bool, parse_go_int,
        split_flag_value, FlagSet, Kind, Spec,
    };

    const SEED: [Spec; 3] = [
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

    fn seed_set() -> FlagSet {
        FlagSet::new("seed", &SEED)
    }

    #[test]
    fn usage_matches_go_printdefaults() {
        let usage = seed_set().usage();
        assert_eq!(
            usage,
            "Usage of seed:\n  -batch int\n    \ttransaction batch size (default 1000)\n  \
             -count int\n    \tnumber of deterministic links (default 1000000)\n  \
             -database string\n    \tSQLite database (default \".data/benchmark.db\")\n"
        );
    }

    #[test]
    fn usage_omits_zero_defaults_and_keeps_bool_typeless() {
        let flags = FlagSet::new(
            "kvimport",
            &[
                Spec {
                    name: "commit",
                    kind: Kind::Bool,
                    default: "false",
                    usage: "write accepted records",
                },
                Spec {
                    name: "input",
                    kind: Kind::String,
                    default: "-",
                    usage: "NDJSON KV export path, or - for stdin",
                },
            ],
        );
        assert_eq!(
            flags.usage(),
            "Usage of kvimport:\n  -commit\n    \twrite accepted records\n  -input string\n    \
             \tNDJSON KV export path, or - for stdin (default \"-\")\n"
        );
    }

    #[test]
    fn accepts_go_value_spellings() {
        let flags = seed_set();
        let values = flags.parse(&[
            "-count".to_string(),
            "12".to_string(),
            "-database=/tmp/a.db".to_string(),
            "--batch".to_string(),
            "5".to_string(),
        ]);
        assert_eq!(values.int("count"), 12);
        assert_eq!(values.string("database"), "/tmp/a.db");
        assert_eq!(values.int("batch"), 5);
    }

    #[test]
    fn keeps_defaults_for_absent_flags() {
        let values = seed_set().parse(&[]);
        assert_eq!(values.int("count"), 1_000_000);
        assert_eq!(values.int("batch"), 1_000);
        assert_eq!(values.string("database"), ".data/benchmark.db");
    }

    #[test]
    fn last_repeated_flag_wins() {
        let values = seed_set().parse(&[
            "-count".to_string(),
            "1".to_string(),
            "-count".to_string(),
            "2".to_string(),
        ]);
        assert_eq!(values.int("count"), 2);
    }

    #[test]
    fn stops_at_a_bare_dash_and_double_dash() {
        let values = seed_set().parse(&["-".to_string(), "-count".to_string(), "7".to_string()]);
        assert_eq!(
            values.int("count"),
            1_000_000,
            "a bare hyphen is a non-flag argument, so parsing stopped"
        );
        let values = seed_set().parse(&["--".to_string(), "-count".to_string(), "7".to_string()]);
        assert_eq!(values.int("count"), 1_000_000, "-- terminates the flags");
    }

    #[test]
    fn splits_values_like_parseone() {
        assert_eq!(split_flag_value("count=12"), ("count", Some("12")));
        assert_eq!(split_flag_value("api-key=a=b"), ("api-key", Some("a=b")));
        assert_eq!(split_flag_value("commit"), ("commit", None));
        assert_eq!(split_flag_value("empty="), ("empty", Some("")));
    }

    #[test]
    fn treats_bare_hyphen_as_an_argument() {
        assert!(!is_flag_argument("-"));
        assert!(is_flag_argument("-x"));
        assert!(is_flag_argument("--"));
        assert!(!is_flag_argument("file.json"));
    }

    #[test]
    fn parses_go_integer_spellings() {
        assert_eq!(parse_go_int("12"), Ok(12));
        assert_eq!(parse_go_int("-5"), Ok(-5));
        assert_eq!(parse_go_int("+7"), Ok(7));
        assert_eq!(parse_go_int("0x10"), Ok(16), "base prefix is detected");
        assert_eq!(parse_go_int("010"), Ok(8), "a leading zero is octal");
        assert_eq!(parse_go_int("0b101"), Ok(5));
        assert_eq!(parse_go_int("1_000").err(), Some("parse error".to_string()));
        assert_eq!(parse_go_int("0x1_0"), Ok(16));
        assert_eq!(parse_go_int("").err(), Some("parse error".to_string()));
        assert_eq!(parse_go_int("1.5").err(), Some("parse error".to_string()));
        assert_eq!(parse_go_int("abc").err(), Some("parse error".to_string()));
        assert_eq!(
            parse_go_int("9999999999999999999999").err(),
            Some("value out of range".to_string())
        );
        assert_eq!(parse_go_int(&i64::MIN.to_string()), Ok(i64::MIN));
    }

    #[test]
    fn value_errors_carry_the_go_causes() {
        // The flag package appended each parser's own message after
        // `invalid value ...`, so the cause is not a generic text.
        assert_eq!(
            check_kind(Kind::Bool, "yes").err(),
            Some("parsing bool: yes".to_string())
        );
        assert_eq!(check_kind(Kind::Bool, "true"), Ok(()));
        assert_eq!(
            check_kind(Kind::Int, "abc").err(),
            Some("parse error".to_string())
        );
        assert_eq!(
            check_kind(Kind::Int, "9223372036854775808").err(),
            Some("value out of range".to_string())
        );
        assert_eq!(
            check_kind(Kind::Duration, "1.5").err(),
            Some("time: missing unit in duration \"1.5\"".to_string())
        );
        assert_eq!(check_kind(Kind::Duration, "2m30s"), Ok(()));
        assert_eq!(check_kind(Kind::String, "anything"), Ok(()));
    }

    #[test]
    fn parses_go_boolean_spellings() {
        assert_eq!(parse_go_bool("true"), Some(true));
        assert_eq!(parse_go_bool("T"), Some(true));
        assert_eq!(parse_go_bool("1"), Some(true));
        assert_eq!(parse_go_bool("False"), Some(false));
        assert_eq!(parse_go_bool("0"), Some(false));
        assert_eq!(parse_go_bool("yes"), None);
        assert_eq!(parse_go_bool(""), None);
    }

    #[test]
    fn quotes_values_like_go_percent_q() {
        assert_eq!(go_quote("1x"), "\"1x\"");
        assert_eq!(go_quote("5 minutes"), "\"5 minutes\"");
        assert_eq!(go_quote("a\nb"), "\"a\\nb\"");
        assert_eq!(go_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(go_quote(""), "\"\"");
        assert_eq!(go_quote("café"), "\"café\"", "printable text is kept");
        assert_eq!(go_quote("\u{1}"), "\"\\x01\"");
        assert_eq!(go_time_quote("1x"), "\"1x\"");
        assert_eq!(go_time_quote("a\nb"), "\"a?b\"");
    }
}
