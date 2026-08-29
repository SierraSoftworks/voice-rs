//! `voice-orders new <path>`: writes a starting-point profile.
//!
//! The scaffold shows *every* option with its default value, commented out, so
//! that the file itself doubles as an option reference — but the parts which
//! are left active (the model path and a small worked grammar) make it a
//! profile which loads as written. A unit test parses the scaffold through the
//! real [`Profile::parse`] path so that it can never rot.
//!
//! [`Profile::parse`]: crate::config::Profile::parse

use clap::Args;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing_batteries::prelude::*;

use crate::errors::HumanizableError;

#[derive(Args, Debug)]
pub struct NewArgs {
    /// The path at which the new profile should be created.
    pub profile: PathBuf,
}

/// The scaffold written by `voice-orders new`.
///
/// Keep this loadable: the commented options are documentation, but everything
/// left uncommented must parse, and `scaffold_parses` asserts exactly that.
const SCAFFOLD: &str = r#"# A voice-orders profile: speak a phrase, press the keys.
#
# Every option is shown below with its default value; uncomment the ones you
# want to change. When you have edited it, check your work with:
#
#     voice-orders validate <this file>
#
# and then run it with:
#
#     voice-orders run <this file> -- <the game you want to play>

# A friendly name for this profile, used in logs and validation reports.
# name: My Profile

# The Vosk model used to recognize speech. Download one from
# https://alphacephei.com/vosk/models and unpack it somewhere sensible; a
# leading '~' is expanded to your home directory when the profile loads.
#
# This may also be a bare model *name* — 'vosk-model-small-en-us-0.15' — which
# is looked for in your models directory (~/.local/share/vosk, or 'models.path'
# in ~/.config/voice-orders/config.yaml). That is the form to use in a profile
# you intend to share.
model: ~/.local/share/vosk/vosk-model-small-en-us-0.15

# Which microphone to listen on. Leave it out to use the one your
# ~/.config/voice-orders/config.yaml names, or your system default.
# Run 'voice-orders devices' to list every microphone this machine can see.
# audio:
#   # "default", or any substring of the device name, e.g. "USB Microphone".
#   device: default

# The global listen hotkey. Leave the whole block out to listen all the time —
# or to use the one your ~/.config/voice-orders/config.yaml sets, which is what
# a profile you intend to share should do. Each field you do write here wins
# over the machine's.
# hotkey:
#   # "auto", a /dev/input/event* path, or a substring of the device name.
#   # 'voice-orders devices' lists them, and says which one "auto" would pick.
#   device: auto
#   # The key which controls listening; see the key reference in the docs.
#   key: rightctrl
#   # toggle       — each press flips listening on or off
#   # push-to-talk — listening only while the key is held
#   # push-to-mute — listening except while the key is held
#   mode: toggle
#   # Whether stopping listening also stops whatever is being typed: with
#   # `true`, the command in flight is cancelled where it stands (its keys are
#   # released) and anything queued behind it is thrown away.
#   interrupt: false

# How long an ambiguous phrase waits in case you carry on with a longer one:
# with both "reload" and "reload weapon" in the profile, saying "reload" waits
# this long before firing, in case "weapon" is still coming.
# completion_timeout: 500ms

# The pacing applied to every command's key presses.
# defaults:
#   # How long each press is held down.
#   duration: 30ms
#   # The gap left between one press and the next.
#   interval: 25ms

# The command grammar: a list of rules. TitleCase rules are published as
# speakable commands; lowercase rules are private building blocks other rules
# refer to. `//` comments run to the end of the line.
#
# In a rule's pattern:
#   "quoted words"        are what you say (multi-word literals are fine)
#   "word"?               may be left unsaid
#   ("either" | "or")     groups alternatives
#   other_rule            reuses a private rule — its words *and* its presses
#   thing[1..4]           repeats a term a bounded number of times
#   thing:name            captures the term's presses for `name...` below
#
# The `{ ... }` action block says what a match presses: a bare chord ("4",
# "leftctrl+leftalt+t") is a press, `wait(20ms)` an explicit pause, and
# `hold(..)` / `release(..)` the two halves of a press when they belong to
# different moments (`release(*)` lets go of everything). `...` splices in
# everything the matched words accumulated; `name...` splices one capture. A
# rule with no block passes its accumulated presses along unchanged.
grammar: |
  // "deploy the sentry", "deploy turret", ... — each alternative carries its
  // own press, and the rule passes them along implicitly.
  Deploy = "deploy" "the"? ("sentry" { 4 } | "turret" { 5 })

  // A private building block: reusable words with their presses.
  direction = ( "north" { up }
              | "south" { down }
              | "east"  { right }
              | "west"  { left } )

  // "look north", ... — the capture lets the block place the direction's
  // press exactly where it belongs: after the map key and its settle time.
  Look = "look" direction:dir { m, wait(20ms), dir... }
"#;

/// Writes the scaffold profile to `args.profile`.
pub async fn run(args: NewArgs) -> Result<(), crate::Error> {
    write_scaffold(&args.profile).await?;

    println!("Wrote a new profile to '{}'.", args.profile.display());
    println!(
        "Edit it, then check it with: voice-orders validate {}",
        args.profile.display()
    );

    Ok(())
}

/// Creates `path` and writes [`SCAFFOLD`] into it, refusing to overwrite.
///
/// The file is opened with `create_new`, so the "does it already exist?" check
/// and the creation are the same operation — we cannot lose a profile to a race
/// between the two.
async fn write_scaffold(path: &Path) -> Result<(), crate::Error> {
    debug!("Writing a new profile to {}", path.display());

    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(human_errors::user(
                format!(
                    "There is already a file at '{}', and we will not overwrite a profile you may have spent time on.",
                    path.display()
                ),
                &[
                    "Choose a path which does not exist yet, e.g. `voice-orders new my-other-profile.yaml`.",
                    "If you meant to start again from scratch, delete the existing file first.",
                ],
            ));
        }
        Err(e) => {
            let message = format!("We could not create a new profile at '{}'.", path.display());
            return Err(human_errors::wrap_user(
                e.to_human_error(),
                message,
                &["Check that the directory exists and that you are allowed to write to it."],
            ));
        }
    };

    file.write_all(SCAFFOLD.as_bytes())
        .await
        .map_err(|e| e.to_human_error())?;
    file.flush().await.map_err(|e| e.to_human_error())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LoadedProfile, Profile};
    use std::time::Duration;

    #[test]
    fn test_the_scaffold_parses_as_written() {
        let profile = Profile::parse(&LoadedProfile {
            source: "scaffold.yaml".to_string(),
            content: SCAFFOLD.to_string(),
        })
        .expect("the scaffold we write must be a profile which loads");

        // Everything else is commented out, so the scaffold exercises the
        // defaults as well as the parser.
        assert_eq!(profile.name, None);
        let model = profile.model.as_ref().expect("the scaffold names a model");
        assert!(
            model.ends_with("vosk-model-small-en-us-0.15"),
            "unexpected model path: {}",
            model.display()
        );
        assert_eq!(profile.audio.device, None);
        assert_eq!(profile.hotkey, None);
        assert_eq!(profile.completion_timeout, Duration::from_millis(500));

        // The worked grammar demonstrates published rules, a private rule and
        // a capture — and all of it is proven to compile, lint-free.
        let published: Vec<&str> = profile
            .grammar
            .published()
            .map(|rule| rule.name.as_str())
            .collect();
        assert_eq!(published, vec!["Deploy", "Look"]);
        assert!(
            profile
                .grammar
                .rule("direction")
                .is_some_and(|rule| !rule.published()),
            "the scaffold should demonstrate a private rule"
        );
        assert!(
            profile.grammar.lints().is_empty(),
            "the scaffold must be lint-free: {:?}",
            profile.grammar.lints()
        );
        crate::grammar::Automaton::compile(&profile.grammar)
            .expect("the scaffold's grammar should compile");
    }

    #[test]
    fn test_the_scaffold_documents_every_option() {
        for option in [
            "name:",
            "model:",
            "audio:",
            "device:",
            "hotkey:",
            "key:",
            "mode:",
            "interrupt:",
            "completion_timeout:",
            "defaults:",
            "duration:",
            "interval:",
            "grammar:",
        ] {
            assert!(
                SCAFFOLD.contains(option),
                "the scaffold should mention '{option}'"
            );
        }
    }

    #[tokio::test]
    async fn test_writes_the_scaffold() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("profile.yaml");

        write_scaffold(&path).await.expect("the profile is written");

        let written = tokio::fs::read_to_string(&path).await.expect("readable");
        assert_eq!(written, SCAFFOLD);

        // And what landed on disk loads, not just the constant.
        Profile::parse(&LoadedProfile {
            source: path.display().to_string(),
            content: written,
        })
        .expect("the written profile should load");
    }

    #[tokio::test]
    async fn test_refuses_to_overwrite_an_existing_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("profile.yaml");
        tokio::fs::write(&path, "name: Precious\n")
            .await
            .expect("the existing profile is written");

        let error = write_scaffold(&path)
            .await
            .expect_err("we must not clobber an existing profile");

        let message = error.to_string();
        assert!(
            message.contains(&path.display().to_string()),
            "the error should name the path, got: {message}"
        );
        assert!(
            message.contains("will not overwrite"),
            "unexpected error: {message}"
        );
        assert!(error.is(human_errors::Kind::User));

        assert_eq!(
            tokio::fs::read_to_string(&path).await.expect("readable"),
            "name: Precious\n",
            "the existing file must be untouched"
        );
    }

    #[tokio::test]
    async fn test_reports_an_unwritable_directory() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("no-such-directory").join("profile.yaml");

        let error = write_scaffold(&path)
            .await
            .expect_err("a missing parent directory should fail");

        let message = error.to_string();
        assert!(
            message.contains(&path.display().to_string()),
            "the error should name the path, got: {message}"
        );
        assert!(error.is(human_errors::Kind::User));
    }
}
