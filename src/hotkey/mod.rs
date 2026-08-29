//! The global listen hotkey: evdev device discovery, the async event stream,
//! and the toggle / push-to-talk / push-to-mute listening logic.
//! See DESIGN.md §"Runtime pipeline".
//!
//! [`ListenMode`], [`transition`] and [`initial_listening`] are the whole of
//! the *logic*, and are shared by every platform; how the key events reach them
//! is not. [`watch`] is the seam between the two: on Linux it is evdev
//! discovery plus an `EventStream` ([`discovery`], [`task`]), and on Windows a
//! `WH_KEYBOARD_LL` hook ([`win`]).
//!
//! **Privacy:** on Linux this module reads `/dev/input/event*`, which technically
//! carries every keystroke on the machine. We deliberately look at nothing but
//! the single configured hotkey: every event whose type is not `EV_KEY`, or
//! whose code is not the configured key, is discarded without its value ever
//! being inspected or logged. See DESIGN.md §"Risks" 3.

#![allow(dead_code)] // consumed as the wave-2 `run` assembly lands

#[cfg(target_os = "linux")]
mod discovery;
#[cfg(target_os = "linux")]
mod task;
// Compiled everywhere, like the key table's Windows column: the hook thread
// inside it is `cfg(windows)`, but the logic above it — which events are ours,
// which key an event is, and how Windows' unlabelled key repeats become the
// values [`transition`] expects — is pure, and a rule only Windows could see
// would be a rule only Windows could test.
mod win;

// Re-exported for the `run` assembly, which lands in a later wave.
#[allow(unused_imports)]
#[cfg(target_os = "linux")]
pub use discovery::discover_device;
// The ranking and the enumeration behind it, so `voice-orders devices` can
// show the same answer `device: auto` would arrive at. `Rank` travels inside
// `ListedDevice`, so the listing rarely has to name it.
#[allow(unused_imports)]
#[cfg(target_os = "linux")]
pub use discovery::{ListedDevice, Rank, auto_choice, list_devices};
#[allow(unused_imports)]
#[cfg(target_os = "linux")]
pub use task::hotkey_task;

/// Starts watching for the listen hotkey, returning the task which does it.
///
/// This is the seam the `run` assembly sits on, and the only part of the
/// hotkey module which is not platform-specific. Everything a caller has to
/// know is in the signature: *which* device (the profile's `hotkey.device`
/// hint), which key, in which mode, where to publish listening changes, and
/// when to stop.
///
/// Resolving a device is part of *starting*, not part of running: an
/// unresolvable device is an error the assembly reports before it spins
/// anything up, exactly as it did when `run` called `discover_device` itself.
/// The returned future is what gets spawned.
///
/// On Linux this is evdev discovery plus the `/dev/input/event*` event stream.
/// On Windows it is a `WH_KEYBOARD_LL` hook on a thread of its own ([`win`]),
/// where the fallible part of starting is installing the hook rather than
/// resolving a device.
#[cfg(target_os = "linux")]
pub fn watch(
    device_hint: &str,
    key: crate::output::KeyCode,
    mode: ListenMode,
    listening: tokio::sync::watch::Sender<bool>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<
    impl std::future::Future<Output = Result<(), crate::Error>> + Send + 'static,
    crate::Error,
> {
    let device = discovery::discover_device(device_hint, key)?;

    Ok(task::hotkey_task(device, key, mode, listening, cancel))
}

#[cfg(not(target_os = "linux"))]
pub use win::watch;
// `doctor`'s equivalent of creating a throwaway virtual keyboard: it installs a
// real hook rather than inferring that one would install.
#[allow(unused_imports)]
#[cfg(not(target_os = "linux"))]
pub use win::probe_hook;

/// How the listen hotkey affects the listening state.
///
/// Deserialized from the `hotkey.mode` profile field (DESIGN.md
/// §"Profile schema") as `toggle`, `push-to-talk` or `push-to-mute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListenMode {
    /// Each press of the hotkey flips the listening state.
    #[default]
    Toggle,
    /// Listening only while the hotkey is held down.
    PushToTalk,
    /// Listening except while the hotkey is held down.
    PushToMute,
}

impl std::fmt::Display for ListenMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ListenMode::Toggle => "toggle",
            ListenMode::PushToTalk => "push-to-talk",
            ListenMode::PushToMute => "push-to-mute",
        })
    }
}

/// The listening state the pipeline should start in for a given mode.
///
/// Push-to-mute starts listening (the hotkey silences us); the other two modes
/// start muted so that a freshly launched game never hears anything until you
/// ask it to. See DESIGN.md §"`run` assembly".
pub fn initial_listening(mode: ListenMode) -> bool {
    match mode {
        ListenMode::Toggle | ListenMode::PushToTalk => false,
        ListenMode::PushToMute => true,
    }
}

/// The pure listening-state transition for one hotkey event.
///
/// `key_value` is the raw evdev `EV_KEY` value: `0` = released, `1` = pressed,
/// `2` = auto-repeat (the kernel emits a stream of these while a key is held,
/// and every mode must ignore them — a held push-to-talk key must not flap).
///
/// Returns `None` when the event leaves the state unchanged, so callers only
/// ever publish real changes onto the watch channel.
pub fn transition(mode: ListenMode, current: bool, key_value: i32) -> Option<bool> {
    let next = match (mode, key_value) {
        // A press flips the state; the matching release does nothing.
        (ListenMode::Toggle, 1) => !current,
        (ListenMode::PushToTalk, 1) => true,
        (ListenMode::PushToTalk, 0) => false,
        (ListenMode::PushToMute, 1) => false,
        (ListenMode::PushToMute, 0) => true,
        // Auto-repeat (2), toggle releases, and any value the kernel might
        // grow in the future: no change.
        _ => return None,
    };

    (next != current).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(ListenMode::Toggle, false)]
    #[case(ListenMode::PushToTalk, false)]
    #[case(ListenMode::PushToMute, true)]
    fn test_initial_listening(#[case] mode: ListenMode, #[case] expected: bool) {
        assert_eq!(
            initial_listening(mode),
            expected,
            "{mode} should start with listening={expected}"
        );
    }

    #[rstest]
    // Toggle: a press flips, a release does nothing, auto-repeat does nothing.
    #[case(ListenMode::Toggle, false, 1, Some(true))]
    #[case(ListenMode::Toggle, true, 1, Some(false))]
    #[case(ListenMode::Toggle, false, 0, None)]
    #[case(ListenMode::Toggle, true, 0, None)]
    #[case(ListenMode::Toggle, false, 2, None)]
    #[case(ListenMode::Toggle, true, 2, None)]
    // Push-to-talk: down means listening, up means muted.
    #[case(ListenMode::PushToTalk, false, 1, Some(true))]
    #[case(ListenMode::PushToTalk, true, 1, None)]
    #[case(ListenMode::PushToTalk, true, 0, Some(false))]
    #[case(ListenMode::PushToTalk, false, 0, None)]
    #[case(ListenMode::PushToTalk, false, 2, None)]
    #[case(ListenMode::PushToTalk, true, 2, None)]
    // Push-to-mute: down means muted, up means listening.
    #[case(ListenMode::PushToMute, true, 1, Some(false))]
    #[case(ListenMode::PushToMute, false, 1, None)]
    #[case(ListenMode::PushToMute, false, 0, Some(true))]
    #[case(ListenMode::PushToMute, true, 0, None)]
    #[case(ListenMode::PushToMute, false, 2, None)]
    #[case(ListenMode::PushToMute, true, 2, None)]
    fn test_transition(
        #[case] mode: ListenMode,
        #[case] current: bool,
        #[case] key_value: i32,
        #[case] expected: Option<bool>,
    ) {
        assert_eq!(
            transition(mode, current, key_value),
            expected,
            "{mode} with listening={current} and key value {key_value}"
        );
    }

    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(0)]
    fn test_transition_is_idempotent(#[case] key_value: i32) {
        for mode in [
            ListenMode::Toggle,
            ListenMode::PushToTalk,
            ListenMode::PushToMute,
        ] {
            for current in [false, true] {
                if let Some(next) = transition(mode, current, key_value) {
                    assert_ne!(
                        next, current,
                        "{mode} reported a change which wasn't a change"
                    );
                }
            }
        }
    }

    #[rstest]
    #[case("toggle", ListenMode::Toggle)]
    #[case("push-to-talk", ListenMode::PushToTalk)]
    #[case("push-to-mute", ListenMode::PushToMute)]
    fn test_deserialize_mode(#[case] yaml: &str, #[case] expected: ListenMode) {
        let mode: ListenMode =
            serde_yaml::from_str(yaml).expect("the mode should have deserialized");
        assert_eq!(mode, expected);
    }

    #[test]
    fn test_deserialize_unknown_mode() {
        let err = serde_yaml::from_str::<ListenMode>("hold-to-yell")
            .expect_err("an unknown mode should not deserialize");
        let message = err.to_string();
        assert!(
            message.contains("push-to-talk"),
            "the serde error should list the valid modes, got: {message}"
        );
    }

    #[rstest]
    #[case(ListenMode::Toggle, "toggle")]
    #[case(ListenMode::PushToTalk, "push-to-talk")]
    #[case(ListenMode::PushToMute, "push-to-mute")]
    fn test_display_matches_yaml(#[case] mode: ListenMode, #[case] expected: &str) {
        assert_eq!(mode.to_string(), expected);
    }
}
