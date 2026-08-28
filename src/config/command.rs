//! A single `commands:` entry — the phrase to listen for and the output to
//! emit when it is heard. See DESIGN.md §"Profile schema" and §"Output forms".
//!
//! `keys:` and `events:` are mutually exclusive, and that exclusion is checked
//! by [`CommandConfig::validate_output`] rather than by an untagged serde enum:
//! serde's untagged errors ("data did not match any variant") say nothing about
//! *which* command is at fault, and naming the offending command is the whole
//! point of the check.

use std::time::Duration;

use crate::grammar::CommandPhrase;
use crate::output::CompiledOutput;

use super::duration;
use super::output::{Chord, OutputDefaults, RawEvent, compile_chords, compile_events};

/// One voice command: what to listen for, and what to press.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    /// A friendly name for logs and reports. Defaults to the phrase source.
    #[serde(default)]
    pub name: Option<String>,

    /// The phrase DSL source, parsed during deserialization.
    pub phrase: CommandPhrase,

    /// The shorthand output form: a list of single keys or `+`-joined chords.
    #[serde(default)]
    pub keys: Option<Vec<Chord>>,

    /// The explicit output form: `down` / `up` / `wait` steps, 1:1 with the
    /// compiled plan.
    #[serde(default)]
    pub events: Option<Vec<RawEvent>>,

    /// Overrides `defaults.duration` for this command's `keys:` list.
    #[serde(default, deserialize_with = "duration::deserialize_option")]
    pub duration: Option<Duration>,

    /// Overrides `defaults.interval` for this command's `keys:` list.
    #[serde(default, deserialize_with = "duration::deserialize_option")]
    pub interval: Option<Duration>,
}

impl CommandConfig {
    /// The name to use when talking about this command: its `name:` if it has
    /// one, otherwise the phrase exactly as it was written.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.phrase.source())
    }

    /// Checks that exactly one output form is present and non-empty.
    ///
    /// Called by [`Profile::validate_structure`], so a broken command is a
    /// config-load error rather than a command which silently does nothing.
    ///
    /// [`Profile::validate_structure`]: super::Profile::validate_structure
    pub fn validate_output(&self) -> Result<(), crate::Error> {
        let name = self.display_name();

        match (&self.keys, &self.events) {
            (Some(_), Some(_)) => Err(human_errors::user(
                format!(
                    "The command '{name}' has both a 'keys:' list and an 'events:' list, but a command may only use one output form."
                ),
                &[
                    "Keep 'keys:' for simple key presses and chords, or 'events:' when you need explicit 'down'/'up'/'wait' control — then remove the other one.",
                ],
            )),
            (None, None) => Err(human_errors::user(
                format!("The command '{name}' does not say which keys to press."),
                &[
                    "Add a 'keys:' list for simple presses, e.g. keys: [\"leftctrl+leftalt+t\"], or an 'events:' list for explicit 'down'/'up'/'wait' control.",
                ],
            )),
            (Some(chords), None) if chords.is_empty() => Err(human_errors::user(
                format!(
                    "The command '{name}' has an empty 'keys:' list, so it would press nothing."
                ),
                &["List at least one key or chord, e.g. keys: [\"4\"]."],
            )),
            (None, Some(events)) if events.is_empty() => Err(human_errors::user(
                format!(
                    "The command '{name}' has an empty 'events:' list, so it would press nothing."
                ),
                &["List at least one step, e.g. '- down: x' followed by '- up: x'."],
            )),
            _ => Ok(()),
        }
    }

    /// Compiles this command's output into a flat event plan.
    ///
    /// `defaults` supplies the hold duration and inter-chord interval for the
    /// `keys:` shorthand; the command's own `duration:` / `interval:` override
    /// them when present.
    pub fn compile(&self, defaults: &OutputDefaults) -> Result<CompiledOutput, crate::Error> {
        self.validate_output()?;

        let events = match (&self.keys, &self.events) {
            (Some(chords), _) => compile_chords(
                chords,
                self.duration.unwrap_or(defaults.duration),
                self.interval.unwrap_or(defaults.interval),
            ),
            (_, Some(events)) => compile_events(events),
            // `validate_output` above has already rejected this combination.
            (None, None) => unreachable!("validate_output rejects a command with no output"),
        };

        Ok(CompiledOutput::Keyboard(events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{KeyEvent, keys};
    use rstest::rstest;

    fn key(name: &str) -> crate::output::KeyCode {
        keys::from_name(name).expect("a known key")
    }

    fn load(yaml: &str) -> CommandConfig {
        serde_yaml::from_str(yaml).expect("the command should load")
    }

    #[test]
    fn test_display_name_defaults_to_the_phrase_source() {
        let command = load("phrase: deploy [the] sentry\nkeys: [\"4\"]\n");
        assert_eq!(command.display_name(), "deploy [the] sentry");

        let command = load("name: Deploy\nphrase: deploy [the] sentry\nkeys: [\"4\"]\n");
        assert_eq!(command.display_name(), "Deploy");
    }

    #[test]
    fn test_keys_form_compiles_to_the_expected_plan() {
        let command = load("phrase: open [the] terminal\nkeys: [\"leftctrl+leftalt+t\"]\n");
        let output = command
            .compile(&OutputDefaults::default())
            .expect("the command should compile");

        assert_eq!(
            output,
            CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("leftctrl")),
                KeyEvent::Down(key("leftalt")),
                KeyEvent::Down(key("t")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("t")),
                KeyEvent::Up(key("leftalt")),
                KeyEvent::Up(key("leftctrl")),
            ])
        );
    }

    #[test]
    fn test_events_form_compiles_one_to_one() {
        let command = load("phrase: salute\nevents:\n  - down: x\n  - wait: 750ms\n  - up: x\n");
        let output = command
            .compile(&OutputDefaults::default())
            .expect("the command should compile");

        assert_eq!(
            output,
            CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("x")),
                KeyEvent::Wait(Duration::from_millis(750)),
                KeyEvent::Up(key("x")),
            ])
        );
    }

    #[test]
    fn test_per_command_timing_overrides_the_defaults() {
        let command =
            load("phrase: salute\nkeys: [\"a\", \"b\"]\nduration: 100ms\ninterval: 5ms\n");
        let output = command
            .compile(&OutputDefaults::default())
            .expect("the command should compile");

        assert_eq!(
            output,
            CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("a")),
                KeyEvent::Wait(Duration::from_millis(100)),
                KeyEvent::Up(key("a")),
                KeyEvent::Wait(Duration::from_millis(5)),
                KeyEvent::Down(key("b")),
                KeyEvent::Wait(Duration::from_millis(100)),
                KeyEvent::Up(key("b")),
            ])
        );
    }

    #[rstest]
    // Both forms at once.
    #[case(
        "name: Salute\nphrase: salute\nkeys: [\"x\"]\nevents:\n  - down: x\n",
        "both a 'keys:' list and an 'events:' list"
    )]
    // Neither form.
    #[case("name: Salute\nphrase: salute\n", "does not say which keys to press")]
    // Present but empty.
    #[case("name: Salute\nphrase: salute\nkeys: []\n", "empty 'keys:' list")]
    #[case("name: Salute\nphrase: salute\nevents: []\n", "empty 'events:' list")]
    fn test_output_form_errors_name_the_command(#[case] yaml: &str, #[case] expected: &str) {
        let command = load(yaml);
        let error = command
            .validate_output()
            .expect_err("the command should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("'Salute'"),
            "the error must name the offending command, got: {message}"
        );
        assert!(message.contains(expected), "unexpected error: {message}");
        assert!(error.is(human_errors::Kind::User));
    }

    #[test]
    fn test_output_form_errors_fall_back_to_the_phrase_source() {
        let command = load("phrase: deploy [the] sentry\n");
        let error = command.validate_output().expect_err("no output form");
        assert!(
            error.to_string().contains("'deploy [the] sentry'"),
            "an unnamed command is identified by its phrase, got: {error}"
        );
    }

    #[test]
    fn test_unknown_fields_are_rejected() {
        let error = serde_yaml::from_str::<CommandConfig>("phrase: salute\nkeyz: [\"x\"]\n")
            .expect_err("a typo should be caught");
        let message = error.to_string();
        assert!(
            message.contains("keyz") && message.contains("keys"),
            "the error should name the typo and the valid fields, got: {message}"
        );
    }

    #[test]
    fn test_a_bad_phrase_is_a_load_error_with_a_location() {
        let error = serde_yaml::from_str::<CommandConfig>("phrase: \"deploy [the sentry\"\n")
            .expect_err("an unclosed group should be caught");
        assert!(
            error
                .to_string()
                .contains("You have an unclosed '[' at line 1, column 8"),
            "unexpected error: {error}"
        );
    }
}
