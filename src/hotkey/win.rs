//! The Windows half of the listen hotkey.
//!
//! **This is a compile-only stub.** Phase W1 of the Windows port makes the
//! crate build for `x86_64-pc-windows-msvc`; the hotkey itself lands in W3 as a
//! `WH_KEYBOARD_LL` hook which *observes* rather than consumes — a hotkey the
//! game can still see, which is the closest Windows equivalent of reading
//! `/dev/input/event*` without taking the key away from anybody.
//!
//! The `hotkey.device` hint has no meaning here yet: a low-level hook is
//! system-wide rather than per-device, so device selection stays a Linux
//! concept until Raw Input replaces the hook (see DESIGN.md §"Windows
//! support").

use crate::hotkey::ListenMode;
use crate::output::KeyCode;

/// What a Windows build can be told about the listen hotkey today.
const NOT_IMPLEMENTED_ADVICE: &[&str] = &[
    "The global listen hotkey is being added in a later phase of the Windows port; leave the 'hotkey:' block out of your profile to have voice-orders listen continuously instead.",
    "On Linux, voice-orders is feature complete — see https://sierrasoftworks.github.io/voice-rs/ for the installation guide.",
];

/// Starts watching for the listen hotkey — or, in this build, explains that
/// there is not one yet.
///
/// Mirrors the Linux [`crate::hotkey::watch`] exactly, including resolving the
/// device *before* returning a future, so the `run` assembly reports the
/// failure at the point it would report an unresolvable evdev device.
pub fn watch(
    _device_hint: &str,
    _key: KeyCode,
    _mode: ListenMode,
    _listening: tokio::sync::watch::Sender<bool>,
    _cancel: tokio_util::sync::CancellationToken,
) -> Result<
    impl std::future::Future<Output = Result<(), crate::Error>> + Send + 'static,
    crate::Error,
> {
    Err::<std::future::Pending<Result<(), crate::Error>>, _>(human_errors::user(
        "The global listen hotkey is not implemented in this Windows build yet, so voice-orders cannot watch for your listen key.",
        NOT_IMPLEMENTED_ADVICE,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watching_says_what_is_missing() {
        let error = watch(
            "auto",
            KeyCode(29),
            ListenMode::Toggle,
            tokio::sync::watch::channel(false).0,
            tokio_util::sync::CancellationToken::new(),
        )
        .err()
        .expect("this build has no hotkey support");

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
