//! Profile configuration: the YAML schema, parse-at-load validation, and
//! cross-field `validate_*()` checks. See DESIGN.md §"Profile schema".
//!
//! The division of labour follows grey's validation philosophy: serde and the
//! type system validate everything they possibly can *at load time* (phrases
//! parse, key names resolve, durations parse, unknown fields are refused), and
//! the handful of invariants which span fields live in explicit `validate_*()`
//! methods whose messages always name the offending command.

pub mod command;
pub mod duration;
pub mod hotkey;
pub mod loader;
pub mod output;
pub mod recognition;
pub mod system;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing_batteries::prelude::*;

pub use command::CommandConfig;
// `ResolvedHotkey` is what everything downstream of `ResolvedSettings` actually
// holds, even where it is never named.
#[allow(unused_imports)]
pub use hotkey::{HotkeyConfig, ResolvedHotkey};
pub use loader::LoadedProfile;
pub use recognition::RecognitionConfig;
pub use system::{ResolvedSettings, SystemConfig};
// `Chord`, `KeyName` and `RawEvent` are the vocabulary the `run` assembly and
// the docs generator speak; nothing in the binary reaches them until that lands.
#[allow(unused_imports)]
pub use output::{Chord, KeyName, OutputDefaults, RawEvent};

use crate::grammar::expansion;

/// Default values for the schema, in one place so that the documentation, the
/// `new` scaffold and the code cannot disagree about them.
mod default {
    use std::time::Duration;

    /// `audio.device`: let cpal pick the system default input.
    pub const AUDIO_DEVICE: &str = "default";

    /// `hotkey.device`: search every readable `/dev/input/event*`.
    pub fn hotkey_device() -> String {
        "auto".to_string()
    }

    /// `completion_timeout`: how long an ambiguous prefix waits for more words.
    pub fn completion_timeout() -> Duration {
        Duration::from_millis(300)
    }

    /// `defaults.duration`: how long each chord is held down.
    pub fn key_duration() -> Duration {
        Duration::from_millis(30)
    }

    /// `defaults.interval`: the gap left between chords.
    pub fn key_interval() -> Duration {
        Duration::from_millis(25)
    }
}

/// A complete profile: the model to recognize with, how to listen, and the
/// commands to act on.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// A friendly name for the profile, used in logs and reports.
    #[serde(default)]
    pub name: Option<String>,

    /// The Vosk model: either a path to a model directory (with a leading `~`
    /// expanded at load time), or the bare *name* of a model inside the
    /// machine's models directory.
    ///
    /// Optional so that a profile can be shared without hard-coding somebody
    /// else's filesystem: see [`resolve_model`] for the order in which the
    /// `--model` override, this field and `$VOSK_MODEL_PATH` are consulted.
    #[serde(default, deserialize_with = "deserialize_tilde_path")]
    pub model: Option<PathBuf>,

    /// Which microphone to listen on. Resolved against the machine-level
    /// configuration by [`ResolvedSettings`], never read directly.
    #[serde(default)]
    pub audio: AudioConfig,

    /// The global listen hotkey. Merged field by field with the machine-level
    /// one by [`ResolvedSettings`]; absent in both places means "always
    /// listening".
    #[serde(default)]
    pub hotkey: Option<HotkeyConfig>,

    /// How long an ambiguous phrase waits in case you continue with a longer
    /// one. See DESIGN.md §"The completion-timeout state machine".
    #[serde(
        default = "default::completion_timeout",
        deserialize_with = "duration::deserialize"
    )]
    pub completion_timeout: Duration,

    /// The latency levers: endpointer silence, eager (partial-driven) firing,
    /// and confidence gating. Absent means every default — see DESIGN.md
    /// §"Endpointing and latency".
    #[serde(default)]
    pub recognition: RecognitionConfig,

    /// Timing shared by every command which uses the `keys:` shorthand.
    #[serde(default)]
    pub defaults: OutputDefaults,

    /// The commands this profile listens for.
    pub commands: Vec<CommandConfig>,
}

/// Which microphone to capture from.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// `default`, or any substring of the cpal device name.
    ///
    /// Optional so that a shared profile need not name one person's
    /// microphone: when it is absent the machine's `audio.device` applies, and
    /// failing that `default` (see [`ResolvedSettings::resolve`]).
    #[serde(default)]
    pub device: Option<String>,
}

impl Profile {
    /// Parses a loaded profile, naming its source in any load failure, then
    /// runs the cross-field checks.
    pub fn parse(loaded: &LoadedProfile) -> Result<Self, crate::Error> {
        let source = loaded.source.clone();

        let profile: Profile = serde_yaml::from_str(&loaded.content).map_err(|e| {
            human_errors::wrap_user(
                e,
                format!("We could not read the profile at '{source}'."),
                &[
                    "Check the profile against the error above — it names the field which we could not understand.",
                    "The option reference in the documentation lists every field a profile may set, and `voice-orders new` writes a fully commented starting point.",
                ],
            )
        })?;

        profile.validate_structure()?;

        debug!(
            commands = profile.commands.len(),
            "Loaded profile '{}' from {source}.",
            profile.display_name()
        );

        Ok(profile)
    }

    /// The name to use when talking about this profile.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("<unnamed profile>")
    }

    /// The cross-field invariants which serde cannot express.
    ///
    /// Every message names the command it is about, so a profile with fifty
    /// commands still tells you which one to go and fix.
    pub fn validate_structure(&self) -> Result<(), crate::Error> {
        if self.commands.is_empty() {
            return Err(human_errors::user(
                "This profile does not define any commands, so there would be nothing to listen for.",
                &[
                    "Add at least one entry under 'commands:', each with a 'phrase:' and either a 'keys:' or an 'events:' list.",
                    "Run `voice-orders new <path>` to write a profile with worked examples of both output forms.",
                ],
            ));
        }

        self.recognition.validate()?;

        for command in &self.commands {
            command.validate_output()?;
            self.validate_expansion_volume(command)?;
        }

        Ok(())
    }

    /// Rejects a command whose phrase would expand past the grammar cap.
    ///
    /// The count is multiplicative and never materializes the phrases, so an
    /// explosive phrase fails fast (DESIGN.md §"Expansion and grammar
    /// compilation").
    fn validate_expansion_volume(&self, command: &CommandConfig) -> Result<(), crate::Error> {
        let count = expansion::count(command.phrase.expr());
        if count <= expansion::MAX_EXPANSIONS_PER_COMMAND {
            return Ok(());
        }

        let name = command.display_name();
        let max = expansion::MAX_EXPANSIONS_PER_COMMAND;
        Err(human_errors::user(
            format!(
                "The command '{name}' expands into {count} concrete phrases, which is more than the {max} a single command may use."
            ),
            &[
                "Split the command into several smaller commands, or remove some of its '[optional]' and '{alternate, choices}' groups.",
            ],
        ))
    }
}

/// Expands a leading `~` in a path against `$HOME`, so a config file can be
/// shared between machines without hard-coding somebody's home directory.
///
/// Shared by the profile's `model:` and the system config's `models.path`, so
/// that the two cannot disagree about what a `~` means.
pub(crate) fn deserialize_tilde_path<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PathBuf>, D::Error> {
    use serde::Deserialize;

    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.map(|raw| PathBuf::from(shellexpand::tilde(&raw).into_owned())))
}

/// The environment variable which supplies a model path when neither the
/// command line nor the profile does.
pub const MODEL_PATH_ENV: &str = "VOSK_MODEL_PATH";

/// Decides which Vosk model to recognize with, in the order DESIGN.md
/// §"Model selection" lays down:
///
/// 1. the `--model <path-or-name>` override on `run`, `test` and `validate`;
/// 2. the profile's `model:` field (with `~` already expanded at load);
/// 3. the `VOSK_MODEL_PATH` environment variable.
///
/// The first two may be a bare model *name* rather than a path, in which case
/// it is resolved inside the machine's models directory (`models.path` in the
/// system configuration, `~/.local/share/vosk` by default) — which is what
/// lets a shared profile say `model: vosk-model-en-us-0.22-lgraph` and have it
/// mean the right thing on every machine.
///
/// Nothing here touches the filesystem — whether the path *is* a usable model
/// is the recognizer's question to answer, and it answers it with a much better
/// error than we could.
pub fn resolve_model(
    cli: Option<&Path>,
    profile: &Profile,
    system: &SystemConfig,
) -> Result<PathBuf, crate::Error> {
    resolve_model_from(
        cli,
        profile,
        system,
        std::env::var_os(MODEL_PATH_ENV).map(PathBuf::from),
    )
}

/// [`resolve_model`] with the environment lookup lifted into a parameter, so
/// the resolution order can be tested without mutating the process environment
/// (which is `unsafe` in edition 2024, and racy besides).
fn resolve_model_from(
    cli: Option<&Path>,
    profile: &Profile,
    system: &SystemConfig,
    env: Option<PathBuf>,
) -> Result<PathBuf, crate::Error> {
    if let Some(path) = cli {
        debug!(model = %path.display(), "Using the model given on the command line.");
        return Ok(system::expand_model(path, system));
    }

    if let Some(path) = &profile.model {
        debug!(model = %path.display(), "Using the model named by the profile.");
        return Ok(system::expand_model(path, system));
    }

    // The environment variable is a path by construction — it is set by
    // whoever runs the tool, on the machine it is running on, so there is no
    // sharing problem for a name to solve.
    if let Some(path) = env {
        debug!(model = %path.display(), "Using the model named by ${MODEL_PATH_ENV}.");
        return Ok(path);
    }

    Err(human_errors::user(
        format!(
            "We do not know which speech model to use for the profile '{}': it was not given with '--model', the profile has no 'model:' field, and the {MODEL_PATH_ENV} environment variable is not set. A 'model:' may be a path, or the name of a model inside '{}'.",
            profile.display_name(),
            system.models_path().display()
        ),
        &[
            "Download vosk-model-en-us-0.22-lgraph from https://alphacephei.com/vosk/models, unpack it, and point at it with '--model <path>', with a 'model:' line in your profile, or by setting VOSK_MODEL_PATH.",
            "Unpacking it into your models directory (~/.local/share/vosk, or 'models.path' in ~/.config/voice-orders/config.yaml) lets a profile name it without a path, e.g. 'model: vosk-model-en-us-0.22-lgraph'.",
            "It must be a model with a dynamic graph (one containing 'graph/Gr.fst'): voice-orders constrains recognition to your profile's phrases, which the large precompiled models such as vosk-model-en-us-0.22 cannot do.",
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::ListenMode;
    use crate::output::{CompiledOutput, KeyEvent, keys};
    use rstest::rstest;

    /// The canonical example profile, loaded through the real parse path so
    /// that the documentation cannot drift away from the code.
    const EXAMPLE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/profile.yaml"
    ));

    pub(crate) fn loaded(content: &str) -> LoadedProfile {
        LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: content.to_string(),
        }
    }

    pub(crate) fn parse(content: &str) -> Result<Profile, crate::Error> {
        Profile::parse(&loaded(content))
    }

    fn key(name: &str) -> crate::output::KeyCode {
        keys::from_name(name).expect("a known key")
    }

    #[test]
    fn test_the_example_profile_loads() {
        let profile = Profile::parse(&LoadedProfile {
            source: "examples/profile.yaml".to_string(),
            content: EXAMPLE.to_string(),
        })
        .expect("the example profile should load");

        assert_eq!(profile.name.as_deref(), Some("Deep Rock Galactic"));
        let model = profile.model.as_ref().expect("the example names a model");
        assert!(
            model.ends_with("vosk-model-small-en-us-0.15"),
            "unexpected model path: {}",
            model.display()
        );
        assert!(
            !model.starts_with("~"),
            "the '~' should have been expanded, got: {}",
            model.display()
        );

        assert_eq!(profile.audio.device.as_deref(), Some("default"));
        assert_eq!(profile.completion_timeout, Duration::from_millis(350));
        assert_eq!(
            profile.defaults,
            OutputDefaults {
                duration: Duration::from_millis(30),
                interval: Duration::from_millis(25),
            }
        );

        let hotkey = hotkey::resolve(profile.hotkey.as_ref(), None)
            .expect("the example's hotkey should resolve")
            .expect("the example has a hotkey");
        assert_eq!(hotkey.device, "auto");
        assert_eq!(hotkey.key.code(), key("rightctrl"));
        assert_eq!(hotkey.mode, ListenMode::Toggle);

        let phrases: Vec<&str> = profile.commands.iter().map(|c| c.phrase.source()).collect();
        assert_eq!(
            phrases,
            vec![
                "deploy [the] {autocannon, auto cannon} [sentry]",
                "open [the] terminal",
                "salute",
            ]
        );

        // The three commands exercise both output forms, and every one of them
        // compiles.
        let compiled: Vec<CompiledOutput> = profile
            .commands
            .iter()
            .map(|c| c.compile(&profile.defaults).expect("it should compile"))
            .collect();

        assert_eq!(
            compiled[0],
            CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("4")),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(key("4")),
            ])
        );
        assert_eq!(
            compiled[1],
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
        assert_eq!(
            compiled[2],
            CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("x")),
                KeyEvent::Wait(Duration::from_millis(750)),
                KeyEvent::Up(key("x")),
            ])
        );
    }

    #[test]
    fn test_defaults_apply_to_a_minimal_profile() {
        let profile =
            parse("model: /models/en\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n")
                .expect("a minimal profile should load");

        assert_eq!(profile.name, None);
        assert_eq!(profile.display_name(), "<unnamed profile>");
        assert_eq!(profile.audio, AudioConfig::default());
        assert_eq!(
            profile.audio.device, None,
            "an unset device defers to the machine's configuration"
        );
        assert_eq!(profile.hotkey, None, "no hotkey means always listening");
        assert_eq!(profile.completion_timeout, Duration::from_millis(300));
        assert_eq!(profile.recognition, RecognitionConfig::default());
        assert_eq!(profile.defaults, OutputDefaults::default());
    }

    #[test]
    fn test_the_recognition_block_loads_and_validates() {
        let profile = parse(
            "model: /models/en\nrecognition:\n  silence: 150ms\n  alternatives: 3\n  confidence_margin: 2.5\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n",
        )
        .expect("a profile with a recognition block should load");

        assert_eq!(profile.recognition.silence, Duration::from_millis(150));
        assert_eq!(profile.recognition.alternatives, 3);
        assert!(
            !profile.recognition.eager(),
            "alternatives flip the eager default off"
        );

        // The impossible combination is refused with the reason spelled out.
        let error = parse(
            "model: /models/en\nrecognition:\n  eager: true\n  alternatives: 3\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n",
        )
        .expect_err("eager + alternatives cannot work together");
        assert!(
            error.to_string().contains("finalized"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_the_model_is_optional() {
        // A shared profile may leave the model to whoever runs it, via
        // `--model` or $VOSK_MODEL_PATH — see `resolve_model`.
        let profile = parse("commands:\n  - phrase: salute\n    keys: [\"x\"]\n")
            .expect("a profile without a model should still load");

        assert_eq!(profile.model, None);
    }

    #[test]
    fn test_load_errors_name_the_source() {
        let error = parse("model: /models/en\ncommands: not-a-list\n")
            .expect_err("commands must be a list");
        assert!(
            error.to_string().contains("test-profile.yaml"),
            "unexpected error: {error}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[rstest]
    // A typo at the top level...
    #[case("modle: /models/en\n", "modle")]
    #[case("model: /models/en\ncompletion_timout: 300ms\n", "completion_timout")]
    // ...and one nested inside a block.
    #[case("model: /models/en\naudio:\n  devise: default\n", "devise")]
    fn test_unknown_fields_fail_loudly(#[case] prefix: &str, #[case] typo: &str) {
        let yaml = format!("{prefix}commands:\n  - phrase: salute\n    keys: [\"x\"]\n");
        let error = parse(&yaml).expect_err("a typo should be caught");
        assert!(
            error.to_string().contains(typo),
            "the error should name the unknown field, got: {error}"
        );
    }

    #[test]
    fn test_tilde_is_expanded_in_the_model_path() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };

        let profile =
            parse("model: ~/models/en\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n")
                .expect("the profile should load");
        assert_eq!(
            profile.model,
            Some(PathBuf::from(format!("{home}/models/en")))
        );
    }

    #[test]
    fn test_the_command_line_model_wins() {
        let profile =
            parse("model: /profile/model\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n")
                .expect("the profile should load");

        let resolved = resolve_model_from(
            Some(Path::new("/cli/model")),
            &profile,
            &SystemConfig::default(),
            Some(PathBuf::from("/env/model")),
        )
        .expect("the command line should resolve");

        assert_eq!(resolved, PathBuf::from("/cli/model"));
    }

    #[test]
    fn test_the_profile_model_beats_the_environment() {
        let profile =
            parse("model: /profile/model\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n")
                .expect("the profile should load");

        let resolved = resolve_model_from(
            None,
            &profile,
            &SystemConfig::default(),
            Some(PathBuf::from("/env/model")),
        )
        .expect("the profile should resolve");

        assert_eq!(resolved, PathBuf::from("/profile/model"));
    }

    #[test]
    fn test_the_environment_is_the_last_resort() {
        let profile = parse("commands:\n  - phrase: salute\n    keys: [\"x\"]\n")
            .expect("the profile should load");

        let resolved = resolve_model_from(
            None,
            &profile,
            &SystemConfig::default(),
            Some(PathBuf::from("/env/model")),
        )
        .expect("the environment should resolve");

        assert_eq!(resolved, PathBuf::from("/env/model"));
    }

    #[test]
    fn test_no_model_anywhere_names_every_mechanism() {
        let profile = parse("name: Deep Rock\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n")
            .expect("the profile should load");

        let error = resolve_model_from(None, &profile, &SystemConfig::default(), None)
            .expect_err("there is no model to recognize with");

        let message = error.to_string();
        assert!(
            message.contains("'Deep Rock'"),
            "the error should name the profile, got: {message}"
        );
        for mechanism in ["--model", "model:", MODEL_PATH_ENV] {
            assert!(
                message.contains(mechanism),
                "the error should mention '{mechanism}', got: {message}"
            );
        }

        // The pretty renderer word-wraps advice (splitting URLs and paths), so
        // assert on fragments which survive wrapping.
        let rendered = human_errors::pretty(&error).to_string();
        assert!(
            rendered.contains("vosk-model-en-us-0.22-lgraph") && rendered.contains("alphacephei"),
            "the advice should recommend a model to download, got: {rendered}"
        );
        assert!(
            rendered.contains("dynamic graph"),
            "the advice should explain why grammar mode needs a dynamic graph, got: {rendered}"
        );
    }

    #[test]
    fn test_a_profile_needs_commands() {
        let error =
            parse("model: /models/en\ncommands: []\n").expect_err("an empty profile is useless");
        assert!(
            error.to_string().contains("does not define any commands"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_structural_errors_name_the_command() {
        let error = parse(
            "model: /models/en\ncommands:\n  - name: Salute\n    phrase: salute\n    keys: [\"x\"]\n    events:\n      - down: x\n",
        )
        .expect_err("both output forms at once should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("'Salute'"),
            "the error must name the command, got: {message}"
        );
        assert!(
            message.contains("only use one output form"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_an_explosive_phrase_is_rejected_by_name() {
        // Ten chained four-way alternates would expand to 4^10 phrases.
        let phrase = ["{a, b, c, d}"; 10].join(" ");
        let error = parse(&format!(
            "model: /models/en\ncommands:\n  - name: Explosive\n    phrase: \"{phrase}\"\n    keys: [\"x\"]\n"
        ))
        .expect_err("the expansion cap should reject this");

        let message = error.to_string();
        assert!(
            message.contains("'Explosive'") && message.contains("1048576"),
            "unexpected error: {message}"
        );
        assert!(message.contains("512"), "unexpected error: {message}");
    }

    #[test]
    fn test_a_phrase_at_the_cap_is_accepted() {
        // Nine chained two-way alternates expand to exactly 512.
        let phrase = ["{a, b}"; 9].join(" ");
        parse(&format!(
            "model: /models/en\ncommands:\n  - phrase: \"{phrase}\"\n    keys: [\"x\"]\n"
        ))
        .expect("512 phrases is within the cap");
    }
}
