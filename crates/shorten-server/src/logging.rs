//! Process logging, matching the previous JSON handler on stdout.

use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{filter, Registry};

/// Install the JSON log format used by every binary in this service.
///
/// The previous implementation logged with `slog.NewJSONHandler(os.Stdout, nil)`:
/// one JSON object per line with `time`, `level`, `msg` and the structured
/// attributes, written unbuffered to standard output. `RUST_LOG` narrows the
/// filter when set, exactly as the operator tuned it before.
pub fn init() {
    let filter = filter::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| filter::EnvFilter::new("info"));
    let writer = tracing_subscriber::fmt::layer()
        .json()
        .with_timer(UtcTime::rfc_3339())
        .with_target(false)
        .with_writer(std::io::stdout);
    Registry::default().with(filter).with(writer).init();
}
