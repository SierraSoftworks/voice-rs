//! The optional `hotkey:` block: which device to watch, which key to watch for,
//! and how that key affects listening. See DESIGN.md §"Profile schema".
//!
//! Every field here is optional, `key` included, because the same block is
//! written in two places: a profile, and the machine-level configuration file
//! (DESIGN.md §"System configuration"). The two are merged field by field in
//! [`resolve`], and the hotkey is active if and only if a `key` emerges from
//! that merge — which is what lets a shared profile leave the hotkey out
//! entirely and pick up whichever key the machine running it prefers.
//!
//! Omitting the block in both places means "always listening", which is why
//! [`crate::config::Profile::hotkey`] is an `Option` and why [`resolve`]
//! returns one.

use crate::hotkey::ListenMode;

use super::{default, output::KeyName};

/// The global listen hotkey, as written in a profile or in the machine-level
/// configuration file. Every field may be left out; see [`resolve`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyConfig {
    /// `auto`, a `/dev/input/event*` path, or a substring of the device name.
    #[serde(default)]
    pub device: Option<String>,

    /// The key which controls listening, by its friendly name.
    #[serde(default)]
    pub key: Option<KeyName>,

    /// How pressing the key changes the listening state.
    #[serde(default)]
    pub mode: Option<ListenMode>,

    /// Whether stopping listening also stops whatever is being typed.
    ///
    /// `false` (the default) lets an in-flight command play out in full, which
    /// is what you want when a macro is a self-contained stratagem input.
    /// `true` cancels it the moment you stop listening and throws away
    /// anything queued behind it, which is what you want when a mis-heard
    /// command needs to be stoppable by letting go of the key.
    #[serde(default)]
    pub interrupt: Option<bool>,
}

/// A hotkey with every question answered: what the profile said, what the
/// machine said, and what the schema defaults say, resolved into one thing the
/// pipeline can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHotkey {
    /// `auto`, a `/dev/input/event*` path, or a substring of the device name.
    pub device: String,
    /// The key which controls listening.
    pub key: KeyName,
    /// How pressing the key changes the listening state.
    pub mode: ListenMode,
    /// Whether stopping listening also stops whatever is being typed.
    pub interrupt: bool,
}

/// Merges a profile's `hotkey:` block with the machine's, field by field.
///
/// For each of `device`, `key`, `mode` and `interrupt`: the profile's value if
/// it set one, else the machine's, else the schema default. The result is a
/// hotkey if and only if a `key` emerged from that merge.
///
/// The one error case is a profile which *asked* for a hotkey — it wrote the
/// block — without a key ever turning up, in which case nothing would ever
/// control listening and the profile is silently not doing what it says. A
/// machine which configures a hotkey without a key, on the other hand, is only
/// offering a default to profiles which want one, so a profile with no block
/// of its own is left listening continuously.
pub fn resolve(
    profile: Option<&HotkeyConfig>,
    system: Option<&HotkeyConfig>,
) -> Result<Option<ResolvedHotkey>, crate::Error> {
    let device = profile
        .and_then(|hotkey| hotkey.device.clone())
        .or_else(|| system.and_then(|hotkey| hotkey.device.clone()))
        .unwrap_or_else(default::hotkey_device);

    let key = profile
        .and_then(|hotkey| hotkey.key)
        .or_else(|| system.and_then(|hotkey| hotkey.key));

    let mode = profile
        .and_then(|hotkey| hotkey.mode)
        .or_else(|| system.and_then(|hotkey| hotkey.mode))
        .unwrap_or_default();

    let interrupt = profile
        .and_then(|hotkey| hotkey.interrupt)
        .or_else(|| system.and_then(|hotkey| hotkey.interrupt))
        .unwrap_or(false);

    match key {
        Some(key) => Ok(Some(ResolvedHotkey {
            device,
            key,
            mode,
            interrupt,
        })),
        None if profile.is_some() => Err(human_errors::user(
            "This profile configures a listen hotkey, but its 'key' field is missing — neither the profile nor your voice-orders configuration says which key should control listening.",
            &[
                "Add a 'key:' line to the profile's 'hotkey:' block, e.g. 'key: rightctrl'; the key reference in the documentation lists every name we accept.",
                "Or set it once for this machine, under 'hotkey:' in ~/.config/voice-orders/config.yaml, so that every profile you run picks it up.",
            ],
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::keys;

    fn config(yaml: &str) -> HotkeyConfig {
        serde_yaml::from_str(yaml).expect("the hotkey should load")
    }

    #[test]
    fn test_full_block_loads() {
        let hotkey = config("device: /dev/input/event3\nkey: rightctrl\nmode: push-to-talk\n");

        assert_eq!(hotkey.device.as_deref(), Some("/dev/input/event3"));
        assert_eq!(
            hotkey.key.map(|key| key.code()),
            Some(keys::from_name("rightctrl").unwrap())
        );
        assert_eq!(hotkey.mode, Some(ListenMode::PushToTalk));
        assert_eq!(
            hotkey.interrupt, None,
            "an in-flight command plays out unless something says otherwise"
        );
    }

    #[rstest::rstest]
    // Omitted: an in-flight command plays out in full, as it always has.
    #[case("key: rightctrl\n", false)]
    #[case("key: rightctrl\ninterrupt: false\n", false)]
    // Asked for: stopping listening also stops what is being typed.
    #[case("key: rightctrl\ninterrupt: true\n", true)]
    fn test_interrupt_defaults_to_off(#[case] yaml: &str, #[case] expected: bool) {
        let resolved = resolve(Some(&config(yaml)), None)
            .expect("the hotkey should resolve")
            .expect("a key was given");

        assert_eq!(resolved.interrupt, expected, "for: {yaml:?}");
    }

    #[test]
    fn test_interrupt_must_be_a_boolean() {
        let error = serde_yaml::from_str::<HotkeyConfig>("key: rightctrl\ninterrupt: yes please\n")
            .expect_err("only a boolean makes sense here");
        assert!(
            error.to_string().contains("interrupt"),
            "the error should name the field, got: {error}"
        );
    }

    #[test]
    fn test_only_the_key_is_needed_for_a_working_hotkey() {
        let resolved = resolve(Some(&config("key: rightctrl\n")), None)
            .expect("the hotkey should resolve")
            .expect("a key was given");

        assert_eq!(resolved.device, "auto");
        assert_eq!(
            resolved.mode,
            ListenMode::Toggle,
            "toggle is the default mode"
        );
    }

    #[test]
    fn test_a_block_without_a_key_is_a_resolution_error() {
        let error = resolve(Some(&config("device: auto\n")), None)
            .expect_err("a hotkey without a key makes no sense");

        assert!(
            error.to_string().contains("'key' field is missing"),
            "the error should name the missing field, got: {error}"
        );
    }

    #[test]
    fn test_no_block_at_all_is_always_listening() {
        assert_eq!(
            resolve(None, None).expect("nothing to resolve"),
            None,
            "a profile with no hotkey listens continuously"
        );
    }

    #[test]
    fn test_unknown_key_names_are_load_errors() {
        let error = serde_yaml::from_str::<HotkeyConfig>("key: rightctlr\n")
            .expect_err("an unknown key name should be rejected");
        assert!(
            error.to_string().contains("Did you mean 'rightctrl'?"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_unknown_modes_name_the_alternatives() {
        let error = serde_yaml::from_str::<HotkeyConfig>("key: rightctrl\nmode: push_to_talk\n")
            .expect_err("modes are kebab-case");
        let message = error.to_string();
        assert!(
            message.contains("push-to-talk"),
            "the error should list the valid modes, got: {message}"
        );
    }

    #[test]
    fn test_unknown_fields_are_rejected() {
        let error = serde_yaml::from_str::<HotkeyConfig>("key: rightctrl\nmodee: toggle\n")
            .expect_err("a typo should be caught");
        let message = error.to_string();
        assert!(
            message.contains("modee") && message.contains("mode"),
            "the error should name the typo and the valid fields, got: {message}"
        );
    }

    #[test]
    fn test_the_profile_wins_every_field_it_sets() {
        let profile =
            config("device: /dev/input/event3\nkey: leftctrl\nmode: toggle\ninterrupt: false\n");
        let system =
            config("device: Keychron\nkey: rightctrl\nmode: push-to-mute\ninterrupt: true\n");

        let resolved = resolve(Some(&profile), Some(&system))
            .expect("the hotkey should resolve")
            .expect("a key was given");

        assert_eq!(resolved.device, "/dev/input/event3");
        assert_eq!(resolved.key.code(), keys::from_name("leftctrl").unwrap());
        assert_eq!(resolved.mode, ListenMode::Toggle);
        assert!(!resolved.interrupt);
    }

    #[test]
    fn test_the_machine_fills_every_gap() {
        let system =
            config("device: Keychron\nkey: rightctrl\nmode: push-to-mute\ninterrupt: true\n");

        let resolved = resolve(Some(&HotkeyConfig::default()), Some(&system))
            .expect("the hotkey should resolve")
            .expect("the machine supplied a key");

        assert_eq!(resolved.device, "Keychron");
        assert_eq!(resolved.key.code(), keys::from_name("rightctrl").unwrap());
        assert_eq!(resolved.mode, ListenMode::PushToMute);
        assert!(resolved.interrupt);
    }

    #[test]
    fn test_a_machine_block_without_a_key_cannot_activate_a_silent_profile() {
        let system = config("device: Keychron\nmode: push-to-talk\n");

        assert_eq!(
            resolve(None, Some(&system)).expect("nothing to resolve"),
            None,
            "no key emerged, so there is no hotkey"
        );
    }
}
