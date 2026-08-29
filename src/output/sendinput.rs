//! The Windows keyboard sink: a [`KeySink`] which will play compiled key plans
//! through the `SendInput` API.
//!
//! **This is a compile-only stub.** Phase W1 of the Windows port makes the
//! crate build for `x86_64-pc-windows-msvc`; the injection itself — a
//! `SendInput` batch per event, using the scancode/extended-key encoding the
//! key table already carries in [`crate::output::keys::WinKey`] — lands in W2.
//! Until then this reports an honest user error at the moment `run` tries to
//! create the virtual keyboard, which is the same place a missing `/dev/uinput`
//! is reported on Linux: before any audio machinery spins up.

use crate::Error;
use crate::output::{KeyCode, KeySink};

/// What a Windows build can be told to do about keyboard output today.
const NOT_IMPLEMENTED_ADVICE: &[&str] = &[
    "Keyboard output through SendInput is being added in a later phase of the Windows port; this build can load profiles and check itself with 'voice-orders doctor', but it cannot press keys yet.",
    "On Linux, voice-orders is feature complete — see https://sierrasoftworks.github.io/voice-rs/ for the installation guide.",
];

/// The error every entry point here reports.
fn not_implemented() -> Error {
    human_errors::user(
        "Keyboard output is not implemented in this Windows build yet, so voice-orders has no way to press keys for you.",
        NOT_IMPLEMENTED_ADVICE,
    )
}

/// A [`KeySink`] backed by Windows' `SendInput`.
///
/// Carries no state yet — W2 gives it the `INPUT` buffer it batches events
/// into.
pub struct WinKeySink {
    /// Uninhabitable in practice: [`WinKeySink::new`] never returns `Ok`, so
    /// nothing can hold one. Kept as a field so that W2 can fill the struct in
    /// without changing its shape at every call site.
    _private: (),
}

impl WinKeySink {
    /// Creates the Windows key sink — or, in this build, explains that there
    /// is not one yet.
    ///
    /// `async` to match the Linux sink's signature, which is what keeps the
    /// `run` assembly platform-neutral.
    pub async fn new() -> Result<Self, Error> {
        Err(not_implemented())
    }
}

impl KeySink for WinKeySink {
    async fn press(&mut self, _key: KeyCode) -> Result<(), Error> {
        Err(not_implemented())
    }

    async fn release(&mut self, _key: KeyCode) -> Result<(), Error> {
        Err(not_implemented())
    }

    async fn synchronize(&mut self) -> Result<(), Error> {
        Err(not_implemented())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creating_a_sink_says_what_is_missing() {
        let error = WinKeySink::new()
            .await
            .err()
            .expect("this build has no keyboard output");

        assert!(
            error.description().contains("not implemented"),
            "the error must say the feature is missing, got: {}",
            error.description()
        );
        assert!(
            error.is(human_errors::Kind::User),
            "an unfinished build is something the user can act on, not a crash to report"
        );
    }
}
