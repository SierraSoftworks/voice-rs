//! Conversions from foreign error types into `human_errors::Error`.
//!
//! Following the github-backup convention, `human_errors` is the only error
//! type in this crate: every fallible function returns `crate::Error`, and
//! foreign errors are converted at the boundary via [`HumanizableError`] so
//! call sites read `.map_err(|e| e.to_human_error())`.
//!
//! `User`-kind errors are actionable by the person running the tool (bad
//! config, missing permissions, network hiccups) and are only logged;
//! `System`-kind errors are unexpected and are reported to telemetry.
//!
//! NOTE for module implementers: impls for foreign error types that only one
//! module cares about (e.g. cpal or vosk errors) belong in that module's own
//! files, not here — this file only hosts the trait and broadly-shared impls.

use crate::Error;

/// Converts a foreign error into a `human_errors::Error` carrying a clear
/// message and actionable advice.
#[allow(dead_code)] // consumed as the wave-1 modules land
pub trait HumanizableError {
    fn to_human_error(self) -> Error;
}

impl HumanizableError for std::io::Error {
    fn to_human_error(self) -> Error {
        match self.kind() {
            std::io::ErrorKind::NotFound => human_errors::wrap_user(
                self,
                "We could not find one of the files we needed.",
                &["Make sure that the path you provided exists and is spelled correctly."],
            ),
            std::io::ErrorKind::PermissionDenied => human_errors::wrap_user(
                self,
                "We were not allowed to access one of the files we needed.",
                &[
                    "Make sure that the file is readable by your user.",
                    "If this involves /dev/uinput or /dev/input, see the permissions guide in the documentation.",
                ],
            ),
            _ => human_errors::wrap_system(
                self,
                "An unexpected filesystem error occurred which we could not recover from.",
                &["Please report this issue on GitHub so that we can investigate."],
            ),
        }
    }
}
