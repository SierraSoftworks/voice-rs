//! The machine-level configuration file: `~/.config/voice-orders/config.yaml`.
//! See DESIGN.md §"System configuration".
//!
//! A profile describes *what to listen for*; this file describes *the machine
//! it is being listened for on*. The split exists so that a profile can be
//! shared — published in a repository, pasted into a Gist — without carrying
//! one person's microphone name, one person's keyboard, and one person's model
//! directory along with it: each machine supplies those once, here, and every
//! profile picks them up.
//!
//! Everything in the file is optional, and so is the file itself: an absent
//! config is exactly [`SystemConfig::default`], which reproduces the behaviour
//! voice-orders had before it existed. The parsing conventions are the
//! profile's (`deny_unknown_fields`, the same `KeyName` deserializer, `~`
//! expanded at load), because there is no reason for two config files in one
//! tool to disagree about what a typo looks like.
//!
//! Resolution — which value actually wins when the profile and this file both
//! have an opinion — lives in [`ResolvedSettings`], and *only* there: `run`,
//! `test` and `doctor` all ask it rather than each reaching into the two
//! structs themselves.

use std::path::{Path, PathBuf};

use tracing_batteries::prelude::*;

use super::hotkey::{HotkeyConfig, ResolvedHotkey};
use super::{Profile, default};

/// The directory we own inside `$XDG_CONFIG_HOME` (or `~/.config`).
const CONFIG_DIR: &str = "voice-orders";

/// The file itself.
const CONFIG_FILE: &str = "config.yaml";

/// Where models are looked for when `models.path` does not say otherwise, and
/// what the documentation tells people to unpack a model into.
const DEFAULT_MODELS_PATH: &str = "~/.local/share/vosk";

/// The files the system configuration is read from.
///
/// A struct rather than a constant for the same reason `setup` has one: a test
/// must never be one typo away from reading (or, worse, writing) the real
/// `~/.config`. Everything below takes the paths as a parameter, and only
/// [`SystemConfig::load`] fills in the real ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The machine-level config file.
    pub config: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            config: config_path(),
        }
    }
}

/// Where the machine-level config file lives: `$XDG_CONFIG_HOME/voice-orders/
/// config.yaml` when that variable is set, and `~/.config/voice-orders/
/// config.yaml` otherwise.
pub fn config_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(shellexpand::tilde("~/.config").into_owned()),
    };

    base.join(CONFIG_DIR).join(CONFIG_FILE)
}

/// The machine-level configuration: the defaults a profile inherits when it
/// does not speak for itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemConfig {
    /// The default microphone for profiles which do not name one.
    #[serde(default)]
    pub audio: SystemAudioConfig,

    /// The default listen hotkey, merged field by field with the profile's.
    #[serde(default)]
    pub hotkey: Option<HotkeyConfig>,

    /// Where models named by name (rather than by path) are looked for.
    #[serde(default)]
    pub models: ModelsConfig,

    /// The file this was read from, when there was one. Never deserialized —
    /// it is how `doctor` can say whether a config was found at all.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

/// The system-wide `audio:` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemAudioConfig {
    /// The microphone profiles use when they do not name one: `default`, or a
    /// case-insensitive substring of the cpal device name.
    #[serde(default)]
    pub device: Option<String>,
}

/// The system-wide `models:` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    /// The directory a bare model *name* is resolved against, with a leading
    /// `~` expanded at load. Defaults to [`DEFAULT_MODELS_PATH`].
    #[serde(default, deserialize_with = "super::deserialize_tilde_path")]
    pub path: Option<PathBuf>,
}

impl SystemConfig {
    /// Reads the machine-level configuration from its real location.
    pub fn load() -> Result<Self, crate::Error> {
        Self::load_from(&Paths::default())
    }

    /// [`SystemConfig::load`] against an injected set of paths.
    ///
    /// An absent file is not a failure — it is the overwhelmingly common case,
    /// and it means "all defaults". Anything else (a file we may not read, a
    /// file which is not valid YAML, a field we do not recognize) is a user
    /// error which names the path, because a config file which is silently
    /// ignored is worse than no config file at all.
    pub fn load_from(paths: &Paths) -> Result<Self, crate::Error> {
        let path = &paths.config;

        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    "There is no system configuration at {}; using the defaults.",
                    path.display()
                );
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(human_errors::wrap_user(
                    e,
                    format!(
                        "We could not read the voice-orders configuration at '{}'.",
                        path.display()
                    ),
                    CONFIG_ADVICE,
                ));
            }
        };

        let mut config: Self = serde_yaml::from_str(&content).map_err(|e| {
            human_errors::wrap_user(
                e,
                format!(
                    "We could not understand the voice-orders configuration at '{}'.",
                    path.display()
                ),
                CONFIG_ADVICE,
            )
        })?;

        debug!("Loaded the system configuration from {}.", path.display());
        config.source = Some(path.clone());
        Ok(config)
    }

    /// The microphone a profile which does not name one should use.
    pub fn audio_device(&self) -> &str {
        self.audio
            .device
            .as_deref()
            .unwrap_or(default::AUDIO_DEVICE)
    }

    /// The directory a bare model *name* is resolved against.
    pub fn models_path(&self) -> PathBuf {
        self.models
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from(shellexpand::tilde(DEFAULT_MODELS_PATH).into_owned()))
    }
}

/// Advice for anything which goes wrong reading the config file. The path
/// itself lives in the message, because advice must be `&'static`.
const CONFIG_ADVICE: &[&str] = &[
    "Every field in the file is optional; deleting the file entirely restores the defaults.",
    "See the system configuration reference at https://sierrasoftworks.github.io/voice-rs/profiles/#system-configuration for the fields it may contain.",
];

/// What a profile and this machine, taken together, actually decided.
///
/// This is the single place the two are merged. Nothing downstream reads
/// `profile.audio.device` or `profile.hotkey` directly: `run`, `test` and
/// `doctor` resolve once, here, and then talk about the answer — which is what
/// keeps the three commands from disagreeing about which microphone you meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSettings {
    /// The microphone to capture from: `default`, or a substring of a device
    /// name.
    pub audio_device: String,

    /// The listen hotkey, or `None` for always-listening.
    pub hotkey: Option<ResolvedHotkey>,
}

impl ResolvedSettings {
    /// Merges a profile with this machine's configuration.
    ///
    /// - **audio**: the profile's device, else the system's, else `default`.
    /// - **hotkey**: a *field-level* merge — each of `device`, `key`, `mode`
    ///   and `interrupt` is taken from the profile when it sets it, from the
    ///   system config when it does not, and from the schema default when
    ///   neither does. The hotkey is active if and only if a `key` emerges,
    ///   which is what lets a shared profile omit the hotkey entirely and pick
    ///   up whichever key this machine has chosen.
    pub fn resolve(profile: &Profile, system: &SystemConfig) -> Result<Self, crate::Error> {
        let audio_device = profile
            .audio
            .device
            .as_deref()
            .unwrap_or_else(|| system.audio_device())
            .to_string();

        let hotkey = super::hotkey::resolve(profile.hotkey.as_ref(), system.hotkey.as_ref())?;

        Ok(Self {
            audio_device,
            hotkey,
        })
    }
}

/// Whether a `model:` value is a bare *name* — something to look for inside
/// [`SystemConfig::models_path`] — rather than a path to a model directory.
///
/// A name is anything with no `/` in it which does not start with `~` or `.`,
/// which is precisely the set of strings that could never be a useful path
/// anyway: `vosk-model-en-us-0.22-lgraph` is a name, `./model`, `~/models/en`
/// and `/opt/model` are paths.
pub fn is_model_name(value: &Path) -> bool {
    let Some(text) = value.to_str() else {
        return false;
    };

    !text.is_empty() && !text.contains('/') && !text.starts_with('~') && !text.starts_with('.')
}

/// Turns a `model:` value into a path: a name is joined onto the models
/// directory, and anything path-like is left exactly as it was written.
pub fn expand_model(value: &Path, system: &SystemConfig) -> PathBuf {
    if is_model_name(value) {
        let resolved = system.models_path().join(value);
        debug!(
            "Resolved the model name '{}' to {}.",
            value.display(),
            resolved.display()
        );
        return resolved;
    }

    value.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::ListenMode;
    use crate::output::keys;
    use rstest::rstest;

    fn paths(dir: &Path) -> Paths {
        Paths {
            config: dir.join("config.yaml"),
        }
    }

    fn load(content: &str) -> Result<SystemConfig, crate::Error> {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let paths = paths(dir.path());
        std::fs::write(&paths.config, content).expect("the config should be written");

        SystemConfig::load_from(&paths)
    }

    fn profile(content: &str) -> Profile {
        Profile::parse(&crate::config::LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: format!("{content}commands:\n  - phrase: salute\n    keys: [\"x\"]\n"),
        })
        .expect("the profile should load")
    }

    #[test]
    fn test_an_absent_file_is_all_defaults() {
        let dir = tempfile::tempdir().expect("a temporary directory");

        let config = SystemConfig::load_from(&paths(dir.path()))
            .expect("a machine with no config file is the normal case");

        assert_eq!(config, SystemConfig::default());
        assert_eq!(config.source, None, "nothing was read, so nothing is named");
        assert_eq!(config.audio_device(), "default");
        assert_eq!(config.hotkey, None);
    }

    #[test]
    fn test_an_empty_file_is_all_defaults() {
        let config = load("").expect("an empty config is a valid config");

        assert_eq!(config.audio, SystemAudioConfig::default());
        assert_eq!(config.hotkey, None);
        assert_eq!(config.models, ModelsConfig::default());
        assert!(config.source.is_some(), "the file was read, so it is named");
    }

    #[test]
    fn test_a_full_file_loads() {
        let config = load(
            "audio:\n  device: USB Microphone\nhotkey:\n  device: auto\n  key: rightctrl\n  mode: push-to-talk\n  interrupt: true\nmodels:\n  path: /srv/models\n",
        )
        .expect("the config should load");

        assert_eq!(config.audio.device.as_deref(), Some("USB Microphone"));
        assert_eq!(config.models.path, Some(PathBuf::from("/srv/models")));

        let hotkey = config.hotkey.as_ref().expect("the hotkey block is there");
        assert_eq!(hotkey.device.as_deref(), Some("auto"));
        assert_eq!(
            hotkey.key.map(|key| key.code()),
            Some(keys::from_name("rightctrl").unwrap())
        );
        assert_eq!(hotkey.mode, Some(ListenMode::PushToTalk));
        assert_eq!(hotkey.interrupt, Some(true));
    }

    #[test]
    fn test_a_malformed_file_names_the_path() {
        let error = load("audio: [not, a, mapping]\n").expect_err("that is not the schema");

        let message = error.to_string();
        assert!(
            message.contains("config.yaml"),
            "the error should name the file, got: {message}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[test]
    fn test_an_unknown_field_is_rejected_by_name() {
        let error = load("audio:\n  devise: default\n").expect_err("a typo should be caught");

        assert!(
            error.to_string().contains("devise"),
            "the error should name the unknown field, got: {error}"
        );
    }

    #[test]
    fn test_an_unknown_key_name_is_rejected_the_same_way_as_in_a_profile() {
        let error = load("hotkey:\n  key: rightctlr\n").expect_err("that is not a key");

        assert!(
            error.to_string().contains("Did you mean 'rightctrl'?"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_the_models_path_expands_a_tilde() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };

        let config = load("models:\n  path: ~/models\n").expect("the config should load");
        assert_eq!(
            config.models_path(),
            PathBuf::from(format!("{home}/models"))
        );
    }

    #[test]
    fn test_the_default_models_path_is_the_documented_one() {
        let config = SystemConfig::default();

        assert!(
            config.models_path().ends_with(".local/share/vosk"),
            "unexpected default: {}",
            config.models_path().display()
        );
        assert!(
            !config.models_path().starts_with("~"),
            "the '~' should have been expanded"
        );
    }

    // --- Audio resolution -------------------------------------------------

    #[rstest]
    // The profile decides, whatever the machine says.
    #[case("audio:\n  device: Yeti\n", Some("USB Microphone"), "Yeti")]
    // The profile is silent, so the machine's default applies.
    #[case("", Some("USB Microphone"), "USB Microphone")]
    // Neither has an opinion: the same 'default' as ever.
    #[case("", None, "default")]
    // A profile which explicitly says 'default' means it.
    #[case("audio:\n  device: default\n", Some("USB Microphone"), "default")]
    fn test_the_audio_device_resolves_profile_then_system_then_default(
        #[case] profile_yaml: &str,
        #[case] system_device: Option<&str>,
        #[case] expected: &str,
    ) {
        let system = SystemConfig {
            audio: SystemAudioConfig {
                device: system_device.map(str::to_string),
            },
            ..SystemConfig::default()
        };

        let settings = ResolvedSettings::resolve(&profile(profile_yaml), &system)
            .expect("the settings should resolve");

        assert_eq!(settings.audio_device, expected);
    }

    // --- Hotkey resolution ------------------------------------------------

    #[test]
    fn test_a_profile_without_a_hotkey_picks_up_the_machines() {
        // The point of the whole exercise: a shared profile says nothing about
        // hotkeys, and each machine supplies its own.
        let system = load("hotkey:\n  key: rightctrl\n  mode: push-to-talk\n")
            .expect("the config should load");

        let settings =
            ResolvedSettings::resolve(&profile(""), &system).expect("the settings should resolve");

        let hotkey = settings.hotkey.expect("the machine supplies a hotkey");
        assert_eq!(hotkey.key.code(), keys::from_name("rightctrl").unwrap());
        assert_eq!(hotkey.mode, ListenMode::PushToTalk);
        assert_eq!(hotkey.device, "auto");
        assert!(!hotkey.interrupt);
    }

    #[test]
    fn test_the_merge_is_field_by_field() {
        let system = load(
            "hotkey:\n  device: Keychron\n  key: rightctrl\n  mode: push-to-mute\n  interrupt: true\n",
        )
        .expect("the config should load");

        // The profile overrides two fields and inherits the other two.
        let settings = ResolvedSettings::resolve(
            &profile("hotkey:\n  key: leftctrl\n  mode: push-to-talk\n"),
            &system,
        )
        .expect("the settings should resolve");

        let hotkey = settings
            .hotkey
            .expect("a key emerged, so there is a hotkey");
        assert_eq!(hotkey.key.code(), keys::from_name("leftctrl").unwrap());
        assert_eq!(hotkey.mode, ListenMode::PushToTalk);
        assert_eq!(hotkey.device, "Keychron", "inherited from the machine");
        assert!(hotkey.interrupt, "inherited from the machine");
    }

    #[test]
    fn test_no_hotkey_anywhere_is_always_listening() {
        let settings = ResolvedSettings::resolve(&profile(""), &SystemConfig::default())
            .expect("the settings should resolve");

        assert_eq!(settings.hotkey, None);
    }

    #[test]
    fn test_a_system_hotkey_without_a_key_leaves_a_silent_profile_alone() {
        // The machine names a device but never says which key; a profile which
        // asked for nothing gets nothing, rather than an error about a file it
        // does not know exists.
        let system = load("hotkey:\n  device: Keychron\n").expect("the config should load");

        let settings =
            ResolvedSettings::resolve(&profile(""), &system).expect("the settings should resolve");

        assert_eq!(settings.hotkey, None);
    }

    #[test]
    fn test_a_profile_hotkey_without_a_key_anywhere_is_an_error() {
        let error = ResolvedSettings::resolve(
            &profile("hotkey:\n  device: auto\n  mode: push-to-talk\n"),
            &SystemConfig::default(),
        )
        .expect_err("a hotkey with no key can never fire");

        let message = error.to_string();
        assert!(
            message.contains("key"),
            "the error should name the missing field, got: {message}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[test]
    fn test_a_profile_hotkey_without_a_key_is_completed_by_the_machine() {
        let system = load("hotkey:\n  key: rightctrl\n").expect("the config should load");

        let settings = ResolvedSettings::resolve(
            &profile("hotkey:\n  device: Keychron\n  mode: push-to-talk\n"),
            &system,
        )
        .expect("the machine supplies the missing key");

        let hotkey = settings.hotkey.expect("a key emerged");
        assert_eq!(hotkey.key.code(), keys::from_name("rightctrl").unwrap());
        assert_eq!(hotkey.device, "Keychron");
        assert_eq!(hotkey.mode, ListenMode::PushToTalk);
    }

    // --- Model names ------------------------------------------------------

    #[rstest]
    #[case::a_name("vosk-model-en-us-0.22-lgraph", true)]
    #[case::a_name_with_dots("vosk-model-small-en-us-0.15", true)]
    #[case::absolute("/opt/models/en", false)]
    #[case::relative("./models/en", false)]
    #[case::parent("../models/en", false)]
    #[case::home("~/models/en", false)]
    #[case::nested("models/en", false)]
    #[case::empty("", false)]
    fn test_a_model_name_is_anything_which_is_not_a_path(
        #[case] value: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(is_model_name(Path::new(value)), expected);
    }

    #[test]
    fn test_a_model_name_is_resolved_against_the_models_path() {
        let system = load("models:\n  path: /srv/models\n").expect("the config should load");

        assert_eq!(
            expand_model(Path::new("vosk-model-en-us-0.22-lgraph"), &system),
            PathBuf::from("/srv/models/vosk-model-en-us-0.22-lgraph")
        );
    }

    #[test]
    fn test_a_model_path_is_left_alone() {
        let system = load("models:\n  path: /srv/models\n").expect("the config should load");

        for path in ["/opt/models/en", "./en"] {
            assert_eq!(
                expand_model(Path::new(path), &system),
                PathBuf::from(path),
                "a path must survive resolution untouched"
            );
        }
    }

    #[test]
    fn test_a_model_name_falls_back_to_the_default_models_path() {
        let resolved = expand_model(
            Path::new("vosk-model-small-en-us-0.15"),
            &SystemConfig::default(),
        );

        assert!(
            resolved.ends_with(".local/share/vosk/vosk-model-small-en-us-0.15"),
            "unexpected resolution: {}",
            resolved.display()
        );
    }

    #[test]
    fn test_the_config_path_follows_xdg() {
        // The real environment decides which branch we can assert on, and
        // mutating it is `unsafe` in edition 2024 (and racy besides).
        let path = config_path();

        assert!(path.ends_with("voice-orders/config.yaml"), "{path:?}");

        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(base) if !base.is_empty() => {
                assert!(path.starts_with(PathBuf::from(base)), "{path:?}");
            }
            _ => assert!(path.to_string_lossy().contains("/.config/"), "{path:?}"),
        }
    }
}
