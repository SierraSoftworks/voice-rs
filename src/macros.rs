/// Reports the crate version: a fixed development version in debug builds (so
/// local runs never pollute real telemetry dimensions) and the true Cargo
/// package version in release builds, where CI has substituted the tagged
/// version into Cargo.toml.
macro_rules! version {
    () => {
        if cfg!(debug_assertions) {
            "0.0.0-dev"
        } else {
            env!("CARGO_PKG_VERSION")
        }
    };
}
