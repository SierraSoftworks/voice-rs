//! `voice-orders update [version]`: replace this binary with a release from
//! GitHub. See DESIGN.md §"Self-update".
//!
//! The three-phase download/replace/relaunch machinery is update-rs'; this
//! command is the part which decides *what* to do and says what happened.
//! Between phases the updater relaunches us as
//! `voice-orders update --state <json>`, which is the hidden `--state` argument
//! below: it hands the serialized state straight back to the manager and
//! returns. Everything else is the ordinary path — list the releases, pick one,
//! start the update.
//!
//! The selection itself lives in [`crate::update::choose`] rather than here, so
//! that "which release should this version move to?" is a pure function with
//! tests of its own rather than something tangled up with argument parsing and
//! printing.

use clap::Args;
use tracing_batteries::prelude::*;

use crate::update::{Action, Release};

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// The version to update to, as a tag (`v1.2.3`) or a bare version
    /// (`1.2.3`). Defaults to the latest release newer than this one; naming an
    /// older version rolls back to it.
    pub version: Option<String>,

    /// Print the available releases instead of installing one, marking the one
    /// which is running.
    #[arg(long)]
    pub list: bool,

    /// Consider pre-release versions as well as stable ones.
    #[arg(long)]
    pub prerelease: bool,

    /// Serialized state used to resume an in-progress update. Set automatically
    /// when the updater relaunches voice-orders between phases.
    #[arg(long, hide = true)]
    pub state: Option<String>,
}

/// Updates voice-orders, returning the exit code to leave with.
///
/// Nothing here is a failure the user has to act on except a version which does
/// not exist: being up to date, or being a development build, are both
/// perfectly good outcomes and exit 0.
pub async fn run(args: UpdateArgs) -> Result<i32, crate::Error> {
    let manager = crate::update::manager();

    // When the updater relaunches us between phases it invokes
    // `voice-orders update --state <json>`; hand that straight back to the
    // updater to continue the in-progress update.
    if let Some(state) = args.state.as_deref() {
        info!("Resuming an in-progress update.");
        manager.resume_from_arg(state).await?;
        return Ok(0);
    }

    if args.list {
        // Listing works in a development build too — "what is out there?" is a
        // reasonable question whatever this binary is — it simply marks nothing
        // as installed.
        print!(
            "{}",
            format_release_list(&manager.get_releases().await?, version!())
        );
        return Ok(0);
    }

    // Checked before the request rather than after it, so a development build
    // gets this answer even with no network — and never spends a round trip on
    // a question we already know the answer to.
    if args.version.is_none() && crate::update::is_development_build() {
        report(Action::DevelopmentBuild);
        return Ok(0);
    }

    let releases = manager.get_releases().await?;

    match crate::update::choose(
        &releases,
        version!(),
        args.version.as_deref(),
        args.prerelease,
    ) {
        Action::Install(release) => {
            println!("Downloading update {}...", release.id);

            // `update` returns true once a later phase has been launched in its
            // own process: it needs this one to exit so the binary can be
            // replaced, which returning from here does.
            if manager.update(release).await? {
                println!("Shutting down to complete the update operation.");
            }
        }
        other => report(other),
    }

    Ok(0)
}

/// Says what happened when there was nothing to download.
fn report(action: Action<'_>) {
    match action {
        Action::UpToDate => {
            println!("voice-orders {} is already the newest release.", version!());
            println!(
                "If you would like to roll back to a specific version, you can do so with `voice-orders update v{}`.",
                version!()
            );
        }
        Action::Unavailable => {
            println!(
                "We could not find a release for your platform matching the version you asked for."
            );
            println!("Run `voice-orders update --list` to see what is available.");
        }
        Action::DevelopmentBuild => {
            println!("This is a development build ({}).", version!());
            println!("{}", crate::update::DEVELOPMENT_BUILD_ADVICE);
        }
        // The caller acts on this one rather than reporting it.
        Action::Install(_) => unreachable!("an installable release is not a report"),
    }
}

/// Renders `--list`: one line per release, prefixed with a status marker — `*`
/// for the version which is running, `!` for a release with no asset for this
/// platform, and a space otherwise — with a `(pre-release)` suffix where it
/// applies.
///
/// A development build matches no release, so nothing is marked with `*` there.
fn format_release_list(releases: &[Release], installed: &str) -> String {
    use std::fmt::Write;

    let mut output = String::new();
    for release in releases {
        let marker = if crate::update::is_installed(release, installed) {
            "*"
        } else if release.get_variant().is_none() {
            "!"
        } else {
            " "
        };

        let suffix = if release.prerelease {
            " (pre-release)"
        } else {
            ""
        };

        let _ = writeln!(output, "{marker} {}{suffix}", release.id);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use update_rs::ReleaseVariant;

    fn release(tag: &str, prerelease: bool, has_asset: bool) -> Release {
        Release {
            id: tag.to_string(),
            changelog: String::new(),
            version: tag
                .trim_start_matches('v')
                .parse()
                .expect("the test tag should be a semantic version"),
            prerelease,
            variant: has_asset.then(|| ReleaseVariant {
                name: "voice-orders-linux-amd64".to_string(),
                sha256: None,
            }),
        }
    }

    #[test]
    fn test_the_listing_marks_the_installed_unavailable_and_prerelease_entries() {
        let releases = vec![
            release("v1.2.0", false, true),
            release("v1.1.0", false, true),
            release("v1.0.0", false, false),
            release("v1.3.0-rc.1", true, true),
        ];

        let listing = format_release_list(&releases, "1.1.0");
        let lines: Vec<&str> = listing.lines().collect();

        assert_eq!(lines[0], "  v1.2.0", "an available release is unmarked");
        assert_eq!(lines[1], "* v1.1.0", "the running version is marked");
        assert_eq!(
            lines[2], "! v1.0.0",
            "a release with no asset for this platform is marked"
        );
        assert_eq!(
            lines[3], "  v1.3.0-rc.1 (pre-release)",
            "a pre-release is labelled"
        );
    }

    #[test]
    fn test_a_development_build_marks_nothing_as_installed() {
        let listing = format_release_list(
            &[release("v1.2.0", false, true)],
            crate::update::DEVELOPMENT_VERSION,
        );

        assert_eq!(listing, "  v1.2.0\n");
    }

    // --- Arguments ---------------------------------------------------------

    /// Parses the subcommand's arguments the way the real CLI does.
    fn parse(args: &[&str]) -> UpdateArgs {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            update: UpdateArgs,
        }

        Wrapper::parse_from(std::iter::once("update").chain(args.iter().copied())).update
    }

    #[test]
    fn test_the_resume_argument_matches_what_the_launcher_relaunches_us_with() {
        // update-rs relaunches `voice-orders update --state <json>` (see
        // `crate::update`'s launcher); this is the other half of that contract.
        let args = parse(&["--state", r#"{"phase":"replace"}"#]);

        assert_eq!(args.state.as_deref(), Some(r#"{"phase":"replace"}"#));
        assert!(args.version.is_none());
        assert!(!args.list);
    }

    #[test]
    fn test_a_bare_version_is_the_positional_argument() {
        let args = parse(&["v1.2.3"]);

        assert_eq!(args.version.as_deref(), Some("v1.2.3"));
        assert!(!args.prerelease);

        let args = parse(&["--prerelease"]);
        assert!(args.prerelease);
        assert!(args.version.is_none());
    }

    #[test]
    fn test_list_is_a_flag_of_its_own() {
        let args = parse(&["--list"]);

        assert!(args.list);
        assert!(args.version.is_none());
    }

    // --- Behaviour ---------------------------------------------------------

    #[tokio::test]
    async fn test_a_development_build_is_told_so_without_touching_the_network() {
        // These tests run from a `cargo build` binary, so this exercises the
        // real guard: if it made a request it would either hang here or fail on
        // a machine with no network, and it does neither.

        let code = run(parse(&[]))
            .await
            .expect("a development build is not a failure");

        assert_eq!(code, 0, "there being nothing to do is a success");
    }

    #[tokio::test]
    async fn test_resuming_an_unusable_state_is_reported_as_an_error() {
        // The one genuinely fallible path which needs no network: a cleanup
        // phase with no temporary application to remove.
        let err = run(parse(&["--state", r#"{"phase":"cleanup"}"#]))
            .await
            .expect_err("resuming a cleanup phase without a temporary path should fail");

        assert!(
            err.is(human_errors::Kind::System),
            "an unusable update state is ours to fix, got: {err}"
        );
    }
}
