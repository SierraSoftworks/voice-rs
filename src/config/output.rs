//! The two output forms a command may use — the `keys:` shorthand and the
//! `events:` explicit form — plus the shared `defaults:` block and the key-name
//! resolution which backs all three. See DESIGN.md §"Output forms".
//!
//! Key names and chords resolve during deserialization, so `leftctlr` is a
//! config-load error carrying a "did you mean 'leftctrl'?" hint rather than a
//! key which silently never fires.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::output::{KeyCode, KeyEvent, keys};

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

/// One entry of the `keys:` shorthand: a single key (`"4"`) or a chord whose
/// key names are joined with `+` (`"leftctrl+leftalt+t"`).
///
/// The original text is kept alongside the resolved codes so that lint and
/// error messages can quote the chord exactly as it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    source: String,
    keys: Vec<KeyCode>,
}

impl Chord {
    /// Parses a chord, resolving every `+`-separated segment as a key name.
    pub fn parse(source: &str) -> Result<Self, crate::Error> {
        let mut resolved = Vec::new();

        for segment in source.split('+') {
            let name = segment.trim();
            if name.is_empty() {
                return Err(human_errors::user(
                    format!(
                        "The key sequence '{source}' has an empty segment — every '+' needs a key name on both sides."
                    ),
                    &[
                        "Write a chord as its key names joined with '+', e.g. 'leftctrl+leftalt+t', or a single key on its own, e.g. '4'.",
                    ],
                ));
            }

            resolved.push(resolve(name)?);
        }

        Ok(Self {
            source: source.to_string(),
            keys: resolved,
        })
    }

    /// The keys of this chord, in the order they were written.
    pub fn keys(&self) -> &[KeyCode] {
        &self.keys
    }

    /// The chord exactly as it was written in the profile.
    #[allow(dead_code)] // quoted by the `run` assembly's logs, which land later
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for Chord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = String::deserialize(deserializer)?;
        Chord::parse(&source).map_err(serde::de::Error::custom)
    }
}

/// One step of the `events:` explicit form, 1:1 with a [`KeyEvent`].
///
/// Written in YAML as a single-key mapping: `- down: x`, `- up: x` or
/// `- wait: 750ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEvent {
    /// Press a key and leave it held.
    Down(KeyName),
    /// Release a key.
    Up(KeyName),
    /// Wait before the next step.
    Wait(Duration),
}

/// The wire shape of a [`RawEvent`]: a mapping with exactly one of the three
/// keys set.
///
/// serde's externally-tagged enums are spelled with YAML *tags* (`!down x`) by
/// `serde_yaml`, which is not what a profile should look like, and an untagged
/// enum only ever says "data did not match any variant". Deserializing through
/// this struct instead buys `deny_unknown_fields`' "unknown field `press`,
/// expected one of `down`, `up`, `wait`" and lets us say something useful when
/// a step tries to do two things at once.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventFields {
    #[serde(default)]
    down: Option<KeyName>,
    #[serde(default)]
    up: Option<KeyName>,
    #[serde(default, deserialize_with = "duration::deserialize_option")]
    wait: Option<Duration>,
}

impl<'de> Deserialize<'de> for RawEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = RawEventFields::deserialize(deserializer)?;

        match (fields.down, fields.up, fields.wait) {
            (Some(key), None, None) => Ok(RawEvent::Down(key)),
            (None, Some(key), None) => Ok(RawEvent::Up(key)),
            (None, None, Some(duration)) => Ok(RawEvent::Wait(duration)),
            (None, None, None) => Err(serde::de::Error::custom(
                "an 'events:' step must be one of 'down: <key>', 'up: <key>' or 'wait: <duration>'",
            )),
            _ => Err(serde::de::Error::custom(
                "an 'events:' step may only do one thing — write 'down: <key>', 'up: <key>' or 'wait: <duration>' as separate list entries",
            )),
        }
    }
}

impl RawEvent {
    /// The compiled event this step corresponds to.
    pub fn to_key_event(&self) -> KeyEvent {
        match *self {
            RawEvent::Down(key) => KeyEvent::Down(key.code()),
            RawEvent::Up(key) => KeyEvent::Up(key.code()),
            RawEvent::Wait(duration) => KeyEvent::Wait(duration),
        }
    }
}

/// Timing shared by every command which uses the `keys:` shorthand, overridable
/// per command.
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

/// Compiles the `keys:` shorthand into a flat event plan.
///
/// Per chord: every key goes `Down` in the order it was written, the chord is
/// held for `duration`, then every key goes `Up` in reverse order (so modifiers
/// outlive the key they modify). Chords are separated by `interval` — and
/// **only** separated by it: there is no trailing wait after the last chord,
/// because a macro should not make the executor idle once its work is done.
pub fn compile_chords(chords: &[Chord], duration: Duration, interval: Duration) -> Vec<KeyEvent> {
    let mut plan = Vec::new();

    for (index, chord) in chords.iter().enumerate() {
        if index > 0 {
            plan.push(KeyEvent::Wait(interval));
        }

        for key in chord.keys() {
            plan.push(KeyEvent::Down(*key));
        }

        plan.push(KeyEvent::Wait(duration));

        for key in chord.keys().iter().rev() {
            plan.push(KeyEvent::Up(*key));
        }
    }

    plan
}

/// Compiles the `events:` explicit form into a flat event plan.
pub fn compile_events(events: &[RawEvent]) -> Vec<KeyEvent> {
    events.iter().map(RawEvent::to_key_event).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("a known key")
    }

    #[rstest]
    #[case("a", &["a"])]
    #[case("4", &["4"])]
    #[case("leftctrl+leftalt+t", &["leftctrl", "leftalt", "t"])]
    // Spaces around the '+' are a formatting accident, not an error.
    #[case("leftctrl + leftshift", &["leftctrl", "leftshift"])]
    fn test_chord_parse(#[case] source: &str, #[case] expected: &[&str]) {
        let chord = Chord::parse(source).expect("the chord should parse");
        let expected: Vec<KeyCode> = expected.iter().copied().map(key).collect();
        assert_eq!(chord.keys(), expected);
        assert_eq!(chord.source(), source, "the original text is preserved");
    }

    #[rstest]
    #[case("", "empty segment")]
    #[case("leftctrl+", "empty segment")]
    #[case("+t", "empty segment")]
    #[case("leftctrl++t", "empty segment")]
    fn test_chord_rejects_empty_segments(#[case] source: &str, #[case] expected: &str) {
        let error = Chord::parse(source).expect_err("the chord should be rejected");
        let message = error.to_string();
        assert!(
            message.contains(expected),
            "unexpected error for {source:?}: {message}"
        );
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
    fn test_raw_event_deserializes_from_single_key_maps() {
        let events: Vec<RawEvent> = serde_yaml::from_str("- down: x\n- wait: 750ms\n- up: x\n")
            .expect("events should load");

        assert_eq!(
            events,
            vec![
                RawEvent::Down(KeyName(key("x"))),
                RawEvent::Wait(Duration::from_millis(750)),
                RawEvent::Up(KeyName(key("x"))),
            ]
        );
    }

    #[test]
    fn test_raw_event_rejects_unknown_steps() {
        let error = serde_yaml::from_str::<Vec<RawEvent>>("- press: x\n")
            .expect_err("'press' is not a step we know");
        let message = error.to_string();
        assert!(
            message.contains("press") && message.contains("down"),
            "the error should name the bad step and the valid ones, got: {message}"
        );
    }

    #[test]
    fn test_raw_event_rejects_empty_and_ambiguous_steps() {
        let error = serde_yaml::from_str::<Vec<RawEvent>>("- {}\n")
            .expect_err("an empty step does nothing");
        assert!(
            error.to_string().contains("must be one of"),
            "unexpected error: {error}"
        );

        let error = serde_yaml::from_str::<Vec<RawEvent>>("- down: x\n  up: x\n")
            .expect_err("a step may only do one thing");
        assert!(
            error.to_string().contains("may only do one thing"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_compile_chords_single_key() {
        let chords = vec![Chord::parse("4").unwrap()];
        assert_eq!(
            compile_chords(
                &chords,
                Duration::from_millis(30),
                Duration::from_millis(25)
            ),
            vec![
                KeyEvent::Down(key("4")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("4")),
            ]
        );
    }

    #[test]
    fn test_compile_chords_releases_in_reverse_order() {
        let chords = vec![Chord::parse("leftctrl+leftalt+t").unwrap()];
        assert_eq!(
            compile_chords(
                &chords,
                Duration::from_millis(30),
                Duration::from_millis(25)
            ),
            vec![
                KeyEvent::Down(key("leftctrl")),
                KeyEvent::Down(key("leftalt")),
                KeyEvent::Down(key("t")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("t")),
                KeyEvent::Up(key("leftalt")),
                KeyEvent::Up(key("leftctrl")),
            ]
        );
    }

    #[test]
    fn test_compile_chords_separates_but_does_not_trail() {
        let chords = vec![
            Chord::parse("a").unwrap(),
            Chord::parse("b").unwrap(),
            Chord::parse("c").unwrap(),
        ];
        let plan = compile_chords(
            &chords,
            Duration::from_millis(30),
            Duration::from_millis(25),
        );

        assert_eq!(
            plan,
            vec![
                KeyEvent::Down(key("a")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("a")),
                KeyEvent::Wait(Duration::from_millis(25)),
                KeyEvent::Down(key("b")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("b")),
                KeyEvent::Wait(Duration::from_millis(25)),
                KeyEvent::Down(key("c")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("c")),
            ]
        );
        assert!(
            matches!(plan.last(), Some(KeyEvent::Up(_))),
            "the plan must not end with an interval nobody is waiting for"
        );
    }

    #[test]
    fn test_compile_events_is_one_to_one() {
        let events = vec![
            RawEvent::Down(KeyName(key("x"))),
            RawEvent::Wait(Duration::from_millis(750)),
            RawEvent::Up(KeyName(key("x"))),
        ];

        assert_eq!(
            compile_events(&events),
            vec![
                KeyEvent::Down(key("x")),
                KeyEvent::Wait(Duration::from_millis(750)),
                KeyEvent::Up(key("x")),
            ]
        );
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
