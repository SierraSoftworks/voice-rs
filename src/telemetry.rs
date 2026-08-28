use tracing_batteries::{OpenTelemetry, Session};

/// Builds the telemetry session for this process. OpenTelemetry export is
/// configured entirely through the standard `OTEL_*` environment variables and
/// is disabled when no endpoint is set.
///
/// `console` controls the stdout logging layer: `true` for line-oriented
/// commands (logs are user-facing output there), `false` when a full-screen
/// TUI owns the terminal — a tracing line printed mid-frame would corrupt the
/// display. Export to an OTLP endpoint is independent of this flag.
pub fn setup(console: bool) -> Session {
    let session = Session::new("voice-orders", version!())
        // In console mode, keep logging to the terminal even when an OTLP
        // endpoint is configured: for a CLI, info/warn logs are user-facing
        // output, not just telemetry.
        .with_battery(OpenTelemetry::new("").with_stdout(console));

    // tracing-batteries disables the session by default in debug builds, and
    // its enabled flag gates the *whole* tracing registry — including the
    // stdout logging layer. For a CLI whose logs are part of the user
    // interface, that would silently swallow every `info!`/`warn!`/`error!`
    // in development, so we enable the session unconditionally. Development
    // runs are still distinguishable in telemetry by their "0.0.0-dev"
    // version, and nothing exports unless `OTEL_EXPORTER_OTLP_ENDPOINT` is
    // set.
    session
        .enable()
        .store(true, std::sync::atomic::Ordering::Relaxed);

    session
}
