//! Key-name resolution and the shared `defaults:` pacing block.
//!
//! Key names resolve during deserialization, so `leftctlr` in a config file is
//! a load error carrying a "did you mean 'leftctrl'?" hint rather than a key
//! which silently never fires. The grammar's action blocks resolve their own
//! chords through the same key table during static analysis, so the two
//! surfaces cannot disagree about what a name means.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::output::{KeyCode, keys};

use super::{default, duration};

/// A key referred to by its friendly name, resolved at load time.
///
/// The friendly names are the lowercase evdev names with `KEY_` stripped —
/// `a`, `4`, `f5`, `space`, `leftctrl`, `kp1` — and [`crate::output::keys`] is
/// the single source of truth for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyName(pub KeyCode);

impl KeyName {
    /// The evdev code this name resolved to.
    pub const fn code(self) -> KeyCode {
        self.0
    }
}

impl std::fmt::Display for KeyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `KeyCode`'s Display renders the friendly name.
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for KeyName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        resolve(&name)
            .map(KeyName)
            .map_err(serde::de::Error::custom)
    }
}

/// Resolves a friendly key name, with a `strsim`-ranked "did you mean" hint.
///
/// The hint lives in the *message* rather than the advice because
/// `human_errors` advice must be `&'static`, so nothing dynamic can go there.
pub fn resolve(name: &str) -> Result<KeyCode, crate::Error> {
    keys::from_name(name).ok_or_else(|| {
        let hint = keys::suggest(name)
            .map(|suggestion| format!(" Did you mean '{suggestion}'?"))
            .unwrap_or_default();

        human_errors::user(
            format!("We don't recognize '{name}' as a key name.{hint}"),
            &[
                "Key names are the lowercase evdev key names with their 'KEY_' prefix removed, e.g. 'a', '4', 'f5', 'space', 'enter', 'leftctrl' or 'kp1'.",
                "The key reference page in the documentation lists every name we accept.",
            ],
        )
    })
}

/// The pacing applied to every assembled key plan, overridable per profile:
/// how long a press is held, and the gap between consecutive presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDefaults {
    /// How long each chord is held down.
    #[serde(
        default = "default::key_duration",
        deserialize_with = "duration::deserialize"
    )]
    pub duration: Duration,
    /// The gap left between one chord and the next.
    #[serde(
        default = "default::key_interval",
        deserialize_with = "duration::deserialize"
    )]
    pub interval: Duration,
}

impl Default for OutputDefaults {
    fn default() -> Self {
        Self {
            duration: default::key_duration(),
            interval: default::key_interval(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("a known key")
    }

    #[rstest]
    // A plausible typo gets a did-you-mean...
    #[case("leftctlr", Some("leftctrl"))]
    #[case("entre", Some("enter"))]
    #[case("scape", Some("space"))]
    // ...and something which is not a key at all simply does not.
    #[case("zzzzzzzzzzq", None)]
    fn test_resolve_reports_unknown_names(#[case] name: &str, #[case] suggestion: Option<&str>) {
        let error = resolve(name).expect_err("the key should be rejected");
        let message = error.to_string();

        assert!(
            message.contains(&format!("'{name}'")),
            "the error should quote the bad name, got: {message}"
        );

        match suggestion {
            Some(expected) => assert!(
                message.contains(&format!("Did you mean '{expected}'?")),
                "expected a did-you-mean for '{expected}', got: {message}"
            ),
            None => assert!(
                !message.contains("Did you mean"),
                "expected no did-you-mean, got: {message}"
            ),
        }
    }

    #[test]
    fn test_unknown_key_fails_deserialization_with_the_hint() {
        let error = serde_yaml::from_str::<KeyName>("leftctlr")
            .expect_err("an unknown key name should be rejected");
        assert!(
            error.to_string().contains("Did you mean 'leftctrl'?"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_key_name_round_trips() {
        let name: KeyName = serde_yaml::from_str("rightctrl").expect("the key should resolve");
        assert_eq!(name.code(), key("rightctrl"));
        assert_eq!(name.to_string(), "rightctrl");
    }

    #[test]
    fn test_output_defaults() {
        assert_eq!(
            OutputDefaults::default(),
            OutputDefaults {
                duration: Duration::from_millis(30),
                interval: Duration::from_millis(25),
            }
        );

        // A partial block keeps the defaults for whatever it omits.
        let defaults: OutputDefaults =
            serde_yaml::from_str("duration: 50ms\n").expect("the defaults should load");
        assert_eq!(
            defaults,
            OutputDefaults {
                duration: Duration::from_millis(50),
                interval: Duration::from_millis(25),
            }
        );
    }

    #[test]
    fn test_output_defaults_rejects_unknown_fields() {
        let error = serde_yaml::from_str::<OutputDefaults>("durration: 50ms\n")
            .expect_err("a typo should be caught");
        let message = error.to_string();
        assert!(
            message.contains("durration") && message.contains("duration"),
            "the error should name the typo and the valid fields, got: {message}"
        );
    }
}
