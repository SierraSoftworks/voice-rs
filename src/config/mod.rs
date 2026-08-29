//! Profile configuration: the YAML schema, parse-at-load validation, and
//! cross-field `validate_*()` checks. See DESIGN.md §"Profile schema".
//!
//! The division of labour follows grey's validation philosophy: serde and the
//! type system validate everything they possibly can *at load time* (the
//! grammar parses and analyzes, durations parse, unknown fields are refused),
//! and the handful of invariants which span fields live in explicit
//! `validate_*()` methods whose messages always name the offending piece.
//! Compiling the grammar's automaton is deliberately *not* part of
//! deserialization: it runs in the command assembly (`run`, `test`,
//! `validate`), where its errors can name the profile's source.

pub mod duration;
pub mod hotkey;
pub mod loader;
pub mod output;
pub mod recognition;
pub mod system;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing_batteries::prelude::*;

// `ResolvedHotkey` is what everything downstream of `ResolvedSettings` actually
// holds, even where it is never named.
#[allow(unused_imports)]
pub use hotkey::{HotkeyConfig, ResolvedHotkey};
pub use loader::LoadedProfile;
pub use output::OutputDefaults;
pub use recognition::RecognitionConfig;
pub use system::{ResolvedSettings, SystemConfig};

use crate::grammar::Grammar;

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
/// grammar of commands to act on.
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

    /// Accepted only to say where it went: `completion_timeout` moved into the
    /// `recognition:` block, alongside the other latency levers it trades
    /// against. Rejected by [`Self::validate_structure`].
    #[serde(default, deserialize_with = "duration::deserialize_optional")]
    completion_timeout: Option<Duration>,

    /// The latency levers: endpointer silence, eager (partial-driven) firing,
    /// the ambiguous-prefix wait, and confidence gating. Absent means every
    /// default — see DESIGN.md §"Endpointing and latency".
    #[serde(default)]
    pub recognition: RecognitionConfig,

    /// The pacing applied to every assembled key plan: how long a press is
    /// held, and the gap between consecutive presses.
    #[serde(default)]
    pub defaults: OutputDefaults,

    /// The command grammar, written inline so a profile stays a single,
    /// URL-shareable file. Parsed and statically analyzed during
    /// deserialization — a bad grammar is a config-load error carrying its
    /// rendered diagnostics, never a runtime surprise.
    pub grammar: Grammar,
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
                    "Check the profile against the error above — it names the field (or the place in the grammar) we could not understand.",
                    "The option reference in the documentation lists every field a profile may set, and `voice-orders new` writes a fully commented starting point.",
                ],
            )
        })?;

        profile.validate_structure()?;

        debug!(
            commands = profile.grammar.published().count(),
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
    pub fn validate_structure(&self) -> Result<(), crate::Error> {
        if let Some(timeout) = self.completion_timeout {
            return Err(human_errors::user(
                format!(
                    "This profile sets 'completion_timeout: {}' at the top level, where it no longer lives: it moved into the 'recognition:' block alongside the other latency levers it trades against.",
                    duration::render(timeout)
                ),
                &[
                    "Indent it under 'recognition:', next to 'silence' and 'debounce'.",
                    "The option reference in the documentation lists every field the 'recognition:' block takes.",
                ],
            ));
        }

        if self.grammar.published().next().is_none() {
            return Err(human_errors::user(
                "This profile's grammar does not publish any commands, so there would be nothing to listen for.",
                &[
                    "Publish at least one rule by giving it a TitleCase name, e.g. 'Map = \"map\" { m }' — lowercase rules are private building blocks.",
                    "Run `voice-orders new <path>` to write a profile with a worked example grammar.",
                ],
            ));
        }

        self.recognition.validate()?;

        Ok(())
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
    use crate::grammar::{Automaton, feed};
    use crate::hotkey::ListenMode;
    use crate::output::assembly::ActionItem;
    use crate::output::keys;
    use rstest::rstest;

    /// The canonical example profile, loaded through the real parse path so
    /// that the documentation cannot drift away from the code.
    const EXAMPLE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/profile.yaml"
    ));

    /// The two shipped game profiles, pinned by the same trick.
    const ARMA: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/profiles/arma3.yaml"));
    const HELLDIVERS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/profiles/helldivers2.yaml"
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

    /// A minimal loadable profile body around one command.
    fn minimal(extra: &str) -> String {
        format!("model: /models/en\n{extra}grammar: |\n  Salute = \"salute\" {{ x }}\n")
    }

    fn key(name: &str) -> crate::output::KeyCode {
        keys::from_name(name).expect("a known key")
    }

    /// A press per `+`-joined chord, for asserting walked action programs.
    fn presses(chords: &[&str]) -> Vec<ActionItem> {
        chords
            .iter()
            .map(|chord| ActionItem::Press(chord.split('+').map(key).collect()))
            .collect()
    }

    /// Walks `phrase` through a compiled profile grammar and returns the one
    /// accepting reading's action program.
    fn walk_actions(automaton: &Automaton, phrase: &str) -> Vec<ActionItem> {
        let mut walk = automaton.walk();
        for word in phrase.split_whitespace() {
            walk.step(word);
        }
        let accepts = walk.accepts();
        assert_eq!(
            accepts.len(),
            1,
            "expected exactly one reading of {phrase:?}: {accepts:?}"
        );
        accepts.into_iter().next().expect("just asserted").actions
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
        assert_eq!(
            profile.recognition.completion_timeout,
            Duration::from_millis(350)
        );
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

        let published: Vec<&str> = profile
            .grammar
            .published()
            .map(|rule| rule.name.as_str())
            .collect();
        assert_eq!(published, vec!["Autocannon", "Terminal", "Salute"]);

        // The grammar compiles, and the three commands exercise a plain press,
        // a chord, and the hold/wait/release forms.
        let automaton = Automaton::compile(&profile.grammar).expect("the grammar should compile");
        assert_eq!(
            walk_actions(&automaton, "deploy the autocannon"),
            presses(&["4"])
        );
        assert_eq!(
            walk_actions(&automaton, "auto cannon sentry"),
            presses(&["4"])
        );
        assert_eq!(
            walk_actions(&automaton, "open terminal"),
            presses(&["leftctrl+leftalt+t"])
        );
        assert_eq!(
            walk_actions(&automaton, "salute"),
            vec![
                ActionItem::Hold(vec![key("x")]),
                ActionItem::Wait(Duration::from_millis(750)),
                ActionItem::Release(vec![key("x")]),
            ]
        );
    }

    /// Every shipped profile loads, is lint-free, compiles to an automaton
    /// with zero diagnostics, and produces a non-empty recognition feed — the
    /// docs-can't-drift trick applied to all three.
    #[rstest]
    #[case::example("examples/profile.yaml", EXAMPLE)]
    #[case::arma("profiles/arma3.yaml", ARMA)]
    #[case::helldivers("profiles/helldivers2.yaml", HELLDIVERS)]
    fn test_every_shipped_profile_compiles_cleanly(#[case] source: &str, #[case] content: &str) {
        let profile = Profile::parse(&LoadedProfile {
            source: source.to_string(),
            content: content.to_string(),
        })
        .unwrap_or_else(|e| panic!("'{source}' should load: {e}"));

        assert!(
            profile.grammar.lints().is_empty(),
            "'{source}' should be lint-free:\n{}",
            profile
                .grammar
                .lints()
                .iter()
                .map(|lint| lint.render(profile.grammar.source()))
                .collect::<Vec<_>>()
                .join("\n")
        );

        let automaton = Automaton::compile(&profile.grammar).unwrap_or_else(|diagnostics| {
            panic!(
                "'{source}' should compile:\n{}",
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.render(profile.grammar.source()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
        assert!(!automaton.rule_sizes().is_empty());

        let feed = feed(&profile.grammar);
        assert!(
            !feed.phrases.is_empty(),
            "'{source}' should feed the recognizer something"
        );
    }

    /// Spot checks on the migrated Helldivers profile: an alternates group, an
    /// optional word, and a plain multi-word phrase all walk to the exact
    /// stratagem inputs the pre-migration profile compiled to.
    #[rstest]
    // `("reinforce" | "reinforcements")` — the alternates form.
    #[case("reinforce", &["up", "down", "right", "left", "up"])]
    #[case("reinforcements", &["up", "down", "right", "left", "up"])]
    // `"eagle"? "rearm"` — the optional form, with and without the option.
    #[case("eagle rearm", &["up", "up", "left", "up", "right"])]
    #[case("rearm", &["up", "up", "left", "up", "right"])]
    // A plain multi-word phrase.
    #[case("orbital laser", &["right", "down", "up", "right", "down"])]
    // The ambiguous-prefix pair the header commentary stakes its design on.
    #[case("auto cannon", &["down", "left", "down", "up", "up", "right"])]
    #[case("auto cannon sentry", &["down", "up", "right", "up", "left", "up"])]
    fn test_helldivers_commands_walk_to_their_key_plans(
        #[case] phrase: &str,
        #[case] expected: &[&str],
    ) {
        let profile = Profile::parse(&LoadedProfile {
            source: "profiles/helldivers2.yaml".to_string(),
            content: HELLDIVERS.to_string(),
        })
        .expect("the profile should load");
        let automaton = Automaton::compile(&profile.grammar).expect("the grammar should compile");

        assert_eq!(walk_actions(&automaton, phrase), presses(expected));
    }

    #[test]
    fn test_defaults_apply_to_a_minimal_profile() {
        let profile = parse(&minimal("")).expect("a minimal profile should load");

        assert_eq!(profile.name, None);
        assert_eq!(profile.display_name(), "<unnamed profile>");
        assert_eq!(profile.audio, AudioConfig::default());
        assert_eq!(
            profile.audio.device, None,
            "an unset device defers to the machine's configuration"
        );
        assert_eq!(profile.hotkey, None, "no hotkey means always listening");
        assert_eq!(
            profile.recognition.completion_timeout,
            Duration::from_millis(750)
        );
        assert_eq!(profile.recognition, RecognitionConfig::default());
        assert_eq!(profile.defaults, OutputDefaults::default());
    }

    #[test]
    fn test_the_recognition_block_loads_and_validates() {
        let profile = parse(&minimal(
            "recognition:\n  silence: 150ms\n  alternatives: 3\n  confidence_margin: 2.5\n",
        ))
        .expect("a profile with a recognition block should load");

        assert_eq!(profile.recognition.silence, Duration::from_millis(150));
        assert_eq!(profile.recognition.alternatives, 3);
        assert!(
            !profile.recognition.eager(),
            "alternatives flip the eager default off"
        );

        // The impossible combination is refused with the reason spelled out.
        let error = parse(&minimal("recognition:\n  eager: true\n  alternatives: 3\n"))
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
        let profile = parse("grammar: |\n  Salute = \"salute\" { x }\n")
            .expect("a profile without a model should still load");

        assert_eq!(profile.model, None);
    }

    #[test]
    fn test_load_errors_name_the_source() {
        let error =
            parse("model: /models/en\ngrammar: 42\n").expect_err("the grammar must be a string");
        assert!(
            error.to_string().contains("test-profile.yaml"),
            "unexpected error: {error}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[test]
    fn test_a_bad_grammar_is_a_load_error_with_its_diagnostics() {
        let error = parse("model: /models/en\ngrammar: |\n  Salute = \"salute\" { notakey }\n")
            .expect_err("an unknown key should fail the load");

        let message = error.to_string();
        assert!(
            message.contains("test-profile.yaml"),
            "the error should name the source, got: {message}"
        );
        assert!(
            message.contains("notakey"),
            "the error should carry the diagnostic, got: {message}"
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
        let yaml = format!("{prefix}grammar: |\n  Salute = \"salute\" {{ x }}\n");
        let error = parse(&yaml).expect_err("a typo should be caught");
        assert!(
            error.to_string().contains(typo),
            "the error should name the unknown field, got: {error}"
        );
    }

    #[test]
    fn test_the_removed_commands_list_is_refused_by_name() {
        // The old schema's `commands:` list is gone; a profile still carrying
        // one must hear that it is not silently ignored.
        let error = parse(
            "model: /models/en\ncommands:\n  - phrase: salute\ngrammar: |\n  Salute = \"salute\" { x }\n",
        )
        .expect_err("the old commands list should be rejected");
        assert!(
            error.to_string().contains("commands"),
            "the error should name the removed field, got: {error}"
        );
    }

    #[test]
    fn test_tilde_is_expanded_in_the_model_path() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };

        let profile = parse("model: ~/models/en\ngrammar: |\n  Salute = \"salute\" { x }\n")
            .expect("the profile should load");
        assert_eq!(
            profile.model,
            Some(PathBuf::from(format!("{home}/models/en")))
        );
    }

    #[test]
    fn test_the_command_line_model_wins() {
        let profile = parse(&minimal("")).expect("the profile should load");

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
        let profile = parse(&minimal("")).expect("the profile should load");

        let resolved = resolve_model_from(
            None,
            &profile,
            &SystemConfig::default(),
            Some(PathBuf::from("/env/model")),
        )
        .expect("the profile should resolve");

        assert_eq!(resolved, PathBuf::from("/models/en"));
    }

    #[test]
    fn test_the_environment_is_the_last_resort() {
        let profile =
            parse("grammar: |\n  Salute = \"salute\" { x }\n").expect("the profile should load");

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
        let profile = parse("name: Deep Rock\ngrammar: |\n  Salute = \"salute\" { x }\n")
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
    fn test_a_profile_needs_published_commands() {
        // A grammar of nothing but private building blocks listens for
        // nothing — the grammar-schema shape of the old empty-commands error.
        let error = parse("model: /models/en\ngrammar: |\n  salute = \"salute\" { x }\n")
            .expect_err("a profile with no published rules is useless");
        assert!(
            error.to_string().contains("does not publish any commands"),
            "unexpected error: {error}"
        );
    }
}
