//! Self-update support, built on the [`update-rs`](https://docs.rs/update-rs)
//! crate. See DESIGN.md §"Self-update".
//!
//! This module configures the updater for voice-orders' GitHub releases and
//! owns the *policy* — which release, if any, a given installed version should
//! move to — while all of the three-phase download/replace/relaunch machinery
//! lives in the crate. `commands/update.rs` is the user-facing half.
//!
//! Two things are worth knowing before reading on:
//!
//! - **The asset name is pinned to what CI publishes.** `.github/workflows/rust.yml`
//!   stages each build as `voice-orders-{os}-{arch}` (`voice-orders-linux-amd64`,
//!   `voice-orders-linux-arm64`), which is exactly [`naming::go`]'s Go-style
//!   convention, so no custom pattern is needed. The pattern is matched against
//!   the *whole* asset name, which is what keeps the `libvosk-linux-amd64.so`
//!   asset published alongside it out of the updater's way: updates replace the
//!   voice-orders binary and never touch libvosk.
//! - **`update_rs::Error` *is* [`crate::Error`].** The crate re-exports
//!   `human_errors::Error` as its own error type, so there is no conversion
//!   boundary here and nothing for [`crate::errors::HumanizableError`] to do —
//!   updater failures already carry a description and advice and propagate with
//!   a bare `?`. The one foreign error this module does have to humanize is a
//!   version which will not parse, and that is handled as policy (see
//!   [`Action::DevelopmentBuild`]) rather than as an error at all.

pub use update_rs::Release;

use std::ffi::OsString;
use std::time::Duration;
use tracing_batteries::prelude::*;
use update_rs::{GitHubSource, Launcher, UpdateManager, naming};

/// The GitHub repository voice-orders' releases are published to. Note that the
/// repository is `voice-rs` while the binary (and therefore the release asset)
/// is `voice-orders`.
const REPO: &str = "SierraSoftworks/voice-rs";

/// The release asset prefix, which is the binary's name rather than the
/// repository's. Pinned against the workflow by a test.
const ASSET_PREFIX: &str = "voice-orders";

/// Releases are tagged `vX.Y.Z`; the prefix is stripped before the tag is
/// parsed as a semantic version.
const TAG_PREFIX: &str = "v";

/// What `version!()` reports in a debug build.
///
/// A `cargo build` binary has no published release to compare itself against,
/// so both the `update` command and the background check treat it as "there is
/// nothing here to update" rather than as a version which happens to be very
/// old.
pub(crate) const DEVELOPMENT_VERSION: &str = "0.0.0-dev";

/// What we tell somebody who asks a development build to update itself.
pub(crate) const DEVELOPMENT_BUILD_ADVICE: &str = "Self-updates are only available in released builds — this one was built locally, so there is nothing for an update to replace. Install a release from https://github.com/SierraSoftworks/voice-rs/releases if you would like updates.";

/// How long the background update check is willing to spend on GitHub, in
/// total.
///
/// This runs at game-launch time behind the terminal UI, so it is bounded hard:
/// an unreachable GitHub must cost a session a few seconds of a background task
/// and nothing else.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the `update` command is willing to spend *connecting* to GitHub.
///
/// Deliberately a connect timeout rather than a total one: the download of a
/// release binary is the request which takes the longest, and a slow link is
/// not a reason to abandon it half-way.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Forces the update check to report a version, so the terminal UI's indicator
/// can be exercised without waiting for a release to exist.
///
/// Deliberately undocumented and deliberately checked *before* the development
/// build skip, which is what makes it usable from a `cargo build` binary. It
/// only ever affects what the indicator says; nothing is downloaded.
const FORCE_UPDATE_CHECK: &str = "VOICE_ORDERS_FORCE_UPDATE_CHECK";

/// Relaunches voice-orders between update phases via its `update --state <json>`
/// sub-command, the same convention Git-Tool uses.
///
/// A sub-command rather than update-rs' default [`RESUME_FLAG`] because
/// voice-orders parses its arguments with clap-derive: a bare flag would have to
/// be intercepted before `Args::parse()` runs, whereas a sub-command is simply
/// another arm.
///
/// [`RESUME_FLAG`]: update_rs::RESUME_FLAG
struct VoiceOrdersLauncher;

impl Launcher for VoiceOrdersLauncher {
    fn resume_args(&self, state_json: &str) -> Vec<OsString> {
        vec!["update".into(), "--state".into(), state_json.into()]
    }

    // No `extra_envs`: unlike Git-Tool there is no session identifier to carry
    // across the relaunch, and the phases are separate processes anyway.
}

/// The release asset this platform's binary is published as, e.g.
/// `voice-orders-linux-amd64`.
///
/// This must match `.github/workflows/rust.yml`'s "Stage release artifacts"
/// step exactly; a test pins it against a literal of what that workflow
/// produces, so renaming the asset breaks the build rather than everybody's
/// updater.
fn asset_name() -> String {
    naming::go(ASSET_PREFIX)
}

/// The HTTP client the updater talks to GitHub with.
///
/// voice-orders has no shared client to reuse — the profile loader builds its
/// own per fetch — so one is built here to the same recipe: rustls-only (our
/// `reqwest` has `default-features = false`), and a `User-Agent` naming us,
/// which the GitHub API requires on every request and which update-rs leaves
/// entirely to a caller-supplied client.
///
/// Built through update-rs' own re-export so it is, by construction, the
/// `reqwest::Client` type [`GitHubSource::with_client`] expects.
fn client(
    configure: impl FnOnce(update_rs::reqwest::ClientBuilder) -> update_rs::reqwest::ClientBuilder,
) -> update_rs::reqwest::Client {
    configure(
        update_rs::reqwest::Client::builder().user_agent(format!("voice-orders/{}", version!())),
    )
    .build()
    // A client builder only fails on a broken TLS backend, which is not
    // something we can act on and not a reason to lose the update: the default
    // client still works, it just sends no User-Agent of ours.
    .unwrap_or_default()
}

/// The GitHub release source, wired to `voice-rs`' releases and this platform's
/// asset.
fn source(client: update_rs::reqwest::Client) -> GitHubSource {
    GitHubSource::new(REPO, asset_name())
        .with_release_tag_prefix(TAG_PREFIX)
        .with_client(client)
}

/// Wraps a source in a manager which relaunches through `update --state`.
fn manager_for(source: GitHubSource) -> UpdateManager<GitHubSource> {
    UpdateManager::new(source).with_launcher(Box::new(VoiceOrdersLauncher))
}

/// Build an [`UpdateManager`] configured for voice-orders' releases.
///
/// It downloads the Go-style `voice-orders-<os>-<arch>` asset for the current
/// platform from the project's GitHub releases, whose tags are `vX.Y.Z`, and
/// relaunches through the `update --state <json>` sub-command.
pub(crate) fn manager() -> UpdateManager<GitHubSource> {
    manager_for(source(client(|builder| {
        builder.connect_timeout(CONNECT_TIMEOUT)
    })))
}

/// Whether this build is a local `cargo build` rather than a published release.
pub(crate) fn is_development_build() -> bool {
    version!() == DEVELOPMENT_VERSION
}

/// What an `update` invocation should do, given what is installed and what
/// GitHub offers.
#[derive(Debug, PartialEq)]
pub(crate) enum Action<'a> {
    /// Download and install this release.
    Install(&'a Release),
    /// Nothing to do: what is installed is already the newest thing we would
    /// install.
    UpToDate,
    /// The version asked for is not one we can install — either no release
    /// carries that tag, or the one which does has no asset for this platform.
    Unavailable,
    /// This build does not report a version releases can be compared against,
    /// so there is nothing to update.
    DevelopmentBuild,
}

/// Chooses what to do, given the releases GitHub offers and the version which
/// is installed.
///
/// Pure, and therefore the one place the whole selection policy lives and the
/// only place it has to be tested:
///
/// - an explicit `target` (`v1.2.3` or `1.2.3`) matches exactly that release,
///   whether it is newer or older than what is installed — a rollback is as
///   legitimate as a roll-forward — provided it has an asset for this platform;
/// - otherwise the newest release which has an asset for this platform and is
///   *strictly* newer than `installed` wins, with pre-releases excluded unless
///   they were asked for; and
/// - a version which is [`DEVELOPMENT_VERSION`], or which is not a semantic
///   version at all, compares against nothing, so it is reported as a
///   development build rather than treated as "very old indeed".
pub(crate) fn choose<'a>(
    releases: &'a [Release],
    installed: &str,
    target: Option<&str>,
    prerelease: bool,
) -> Action<'a> {
    if let Some(target) = target {
        let bare = target.strip_prefix(TAG_PREFIX).unwrap_or(target);

        return match releases.iter().find(|r| {
            r.get_variant().is_some() && (r.id == target || r.version.to_string() == bare)
        }) {
            Some(release) => Action::Install(release),
            None => Action::Unavailable,
        };
    }

    if installed == DEVELOPMENT_VERSION {
        return Action::DevelopmentBuild;
    }

    // The version type is `semver::Version`, which we reach through inference
    // from the comparison below rather than by path: `semver` is update-rs'
    // dependency, not ours, and the crate does not re-export it.
    let Ok(installed) = installed.parse() else {
        return Action::DevelopmentBuild;
    };

    match Release::get_latest(releases.iter().filter(|r| {
        r.get_variant().is_some() && r.version > installed && (!r.prerelease || prerelease)
    })) {
        Some(release) => Action::Install(release),
        None => Action::UpToDate,
    }
}

/// Whether a release is the one which is currently installed, for the `--list`
/// marker.
///
/// Compared as text rather than as a parsed version so that a development
/// build (whose `0.0.0-dev` matches no release) needs no special case.
pub(crate) fn is_installed(release: &Release, installed: &str) -> bool {
    release.version.to_string() == installed || release.id == format!("{TAG_PREFIX}{installed}")
}

/// The version [`FORCE_UPDATE_CHECK`] forces the check to report, if it is set
/// to anything at all.
///
/// Split out from [`check_for_update`] so the hook is tested without mutating
/// this process' environment, which the rest of the suite shares.
fn forced_update(value: Option<&str>) -> Option<String> {
    value
        .filter(|forced| !forced.is_empty())
        .map(ToString::to_string)
}

/// Looks for a release newer than the one running, returning its version.
///
/// This is the check the terminal UI runs in the background when a session
/// starts, and everything about it is shaped by that: a game launch must never
/// stall, warn, or fail because GitHub is unreachable, so **every** failure is
/// swallowed at `debug!` level, the request is bounded by [`CHECK_TIMEOUT`],
/// and a development build does not make the request at all.
///
/// Plain (non-TTY) sessions never call this — see DESIGN.md §"Self-update".
pub(crate) async fn check_for_update() -> Option<String> {
    // A forced version short-circuits everything, including the development
    // build skip, so the indicator can be exercised from a `cargo build`.
    if let Some(forced) = forced_update(std::env::var(FORCE_UPDATE_CHECK).ok().as_deref()) {
        debug!("Reporting a forced update to {forced} ({FORCE_UPDATE_CHECK} is set).");
        return Some(forced);
    }

    if is_development_build() {
        debug!("Skipping the update check: this is a development build.");
        return None;
    }

    let manager = manager_for(source(client(|builder| builder.timeout(CHECK_TIMEOUT))));

    let releases = match manager.get_releases().await {
        Ok(releases) => releases,
        Err(e) => {
            // Deliberately not a warning: the user asked to play a game, not to
            // hear about GitHub.
            debug!("Could not check for a newer release: {e}");
            return None;
        }
    };

    match choose(&releases, version!(), None, false) {
        Action::Install(release) => {
            debug!("A newer release is available: {}.", release.id);
            Some(release.version.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use update_rs::ReleaseVariant;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A release, with or without an asset for this platform.
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
                name: asset_name(),
                sha256: None,
            }),
        }
    }

    /// The releases the tests choose between, deliberately out of order (the
    /// GitHub API returns newest-first, and nothing may depend on that): two
    /// stable releases, a release candidate ahead of both, and an old release
    /// which was never published for this platform.
    fn releases() -> Vec<Release> {
        vec![
            release("v1.1.0", false, true),
            release("v1.3.0-rc.1", true, true),
            release("v1.0.0", false, false),
            release("v1.2.0", false, true),
        ]
    }

    // --- Asset naming ------------------------------------------------------

    #[test]
    fn test_the_asset_name_is_the_one_ci_publishes() {
        // Pinned against a literal of what `.github/workflows/rust.yml`'s
        // "Stage release artifacts" step copies the binary to:
        //
        //   cp target/<triple>/release/voice-orders \
        //      voice-orders-${{ matrix.os }}-${{ matrix.arch }}
        //
        // with `os: linux` and `arch: amd64` / `arm64`. If the workflow ever
        // renames its assets, this test fails rather than everybody's updater
        // silently finding nothing to install.
        let expected = if cfg!(target_arch = "x86_64") {
            "voice-orders-linux-amd64"
        } else {
            "voice-orders-linux-arm64"
        };

        assert_eq!(asset_name(), expected);
    }

    #[test]
    fn test_the_asset_pattern_does_not_match_the_libvosk_asset() {
        // Every release also carries `libvosk-linux-<arch>.so`, which the
        // updater must ignore: an update replaces the voice-orders binary and
        // nothing else. update-rs anchors its glob at both ends, so an exact
        // name cannot match a differently-named sibling — but the two names
        // share a `-linux-<arch>` tail, so it is worth pinning.
        let name = asset_name();
        for libvosk in ["libvosk-linux-amd64.so", "libvosk-linux-arm64.so"] {
            assert_ne!(name, libvosk);
            assert!(!libvosk.starts_with(&name), "{libvosk} vs {name}");
        }
    }

    // --- The manager -------------------------------------------------------

    #[test]
    fn test_the_manager_targets_this_repository_and_this_platforms_asset() {
        // `GitHubSource`'s Debug is "GitHub - <repo> (<pattern>)", which is the
        // only window onto how it was configured.
        let manager = manager();

        assert_eq!(
            format!("{manager:?}"),
            format!("GitHub - {REPO} ({})", asset_name())
        );
        assert_eq!(
            manager.target_application,
            std::env::current_exe().unwrap_or_default(),
            "the manager should replace the binary which is running"
        );
    }

    #[test]
    fn test_the_launcher_resumes_through_the_update_subcommand() {
        // The relaunch convention the hidden `update --state` argument exists
        // for: change one and the other has to change with it.
        let args = VoiceOrdersLauncher.resume_args(r#"{"phase":"replace"}"#);

        assert_eq!(
            args,
            vec![
                OsString::from("update"),
                OsString::from("--state"),
                OsString::from(r#"{"phase":"replace"}"#),
            ]
        );
        assert!(
            VoiceOrdersLauncher.extra_envs().is_empty(),
            "there is no session to carry across the relaunch"
        );
    }

    /// A releases payload shaped like GitHub's, carrying the assets a real
    /// voice-orders release carries: the binary for each architecture and the
    /// libvosk shared object which travels beside it.
    fn github_releases_json() -> String {
        format!(
            r#"[
                {{
                    "name": "Version 1.2.0",
                    "tag_name": "v1.2.0",
                    "body": "Example release",
                    "prerelease": false,
                    "assets": [
                        {{ "name": "libvosk-linux-amd64.so" }},
                        {{ "name": "libvosk-linux-arm64.so" }},
                        {{ "name": "voice-orders-linux-amd64" }},
                        {{ "name": "voice-orders-linux-arm64" }}
                    ]
                }},
                {{
                    "name": "Version 1.0.0",
                    "tag_name": "v1.0.0",
                    "body": "Before this platform was built",
                    "prerelease": false,
                    "assets": [
                        {{ "name": "libvosk-linux-{other}.so" }}
                    ]
                }},
                {{
                    "name": "Nightly",
                    "tag_name": "nightly",
                    "body": "Not a version at all",
                    "prerelease": false,
                    "assets": []
                }}
            ]"#,
            // Whichever architecture this is not, so the second release has no
            // asset we could ever select.
            other = if cfg!(target_arch = "x86_64") {
                "arm64"
            } else {
                "amd64"
            }
        )
    }

    #[tokio::test]
    async fn test_the_source_reads_github_and_selects_the_binary_over_libvosk() {
        // The whole release-listing path against a mock GitHub: tag parsing,
        // the `v` prefix, and — the point of this test — the asset glob, which
        // must pick the voice-orders binary out of a release which also
        // publishes libvosk for both architectures. An update replaces the
        // binary and nothing else.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/releases")))
            .respond_with(ResponseTemplate::new(200).set_body_string(github_releases_json()))
            .mount(&server)
            .await;

        let manager = manager_for(
            source(client(|builder| builder)).with_github_endpoints(&server.uri(), &server.uri()),
        );

        let releases = manager
            .get_releases()
            .await
            .expect("the mock GitHub should list the releases");

        assert_eq!(
            releases.len(),
            2,
            "the `nightly` tag is not a version, so it is skipped"
        );

        let latest = &releases[0];
        assert_eq!(latest.id, "v1.2.0");
        assert_eq!(
            latest.version.to_string(),
            "1.2.0",
            "the `v` prefix should have been stripped before parsing"
        );
        assert_eq!(
            latest.get_variant().map(|v| v.name.as_str()),
            Some(asset_name().as_str()),
            "the binary for this platform wins, not libvosk and not the other arch"
        );

        assert!(
            releases[1].get_variant().is_none(),
            "a release with no voice-orders asset for this platform offers nothing"
        );

        assert_eq!(
            choose(&releases, "1.1.0", None, false),
            Action::Install(latest)
        );
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "pure_tests",
        ignore = "talks to the real GitHub releases API"
    )]
    async fn test_the_real_repository_offers_a_release_for_this_platform() {
        // The smoke test behind `voice-orders update --list`: the repository,
        // the tag prefix and the asset name all have to be right at once, and
        // nothing but GitHub can tell us whether they are.
        let releases = manager()
            .get_releases()
            .await
            .expect("GitHub should list this project's releases");

        assert!(!releases.is_empty(), "voice-rs has published releases");
        assert!(
            releases.iter().any(|r| r.get_variant().is_some()),
            "at least one release should publish {} — if this fails, the \
             workflow's asset names and this module have drifted apart",
            asset_name()
        );
    }

    // --- Selection ---------------------------------------------------------

    #[test]
    fn test_the_newest_stable_release_wins_by_default() {
        let releases = releases();

        assert_eq!(
            choose(&releases, "1.1.0", None, false),
            Action::Install(&releases[3]),
            "v1.2.0 is the newest stable release with an asset for this platform"
        );
    }

    #[test]
    fn test_a_prerelease_is_only_offered_when_it_is_asked_for() {
        let releases = releases();

        assert_eq!(
            choose(&releases, "1.2.0", None, false),
            Action::UpToDate,
            "the newest stable release is installed, and the rc is not offered"
        );
        assert_eq!(
            choose(&releases, "1.2.0", None, true),
            Action::Install(&releases[1]),
            "--prerelease brings v1.3.0-rc.1 into the running"
        );
        assert_eq!(
            choose(&releases, "1.1.0", None, true),
            Action::Install(&releases[1]),
            "and it beats the newest stable release, being the newer of the two"
        );
    }

    #[test]
    fn test_nothing_older_than_what_is_installed_is_offered() {
        let releases = releases();

        assert_eq!(choose(&releases, "1.2.0", None, false), Action::UpToDate);
        assert_eq!(
            choose(&releases, "2.0.0", None, false),
            Action::UpToDate,
            "a build ahead of every release has nothing to move to"
        );
    }

    #[test]
    fn test_a_release_without_an_asset_for_this_platform_is_never_offered() {
        // v1.0.0 predates this platform's builds; it must not be selected even
        // when it is the only thing newer than what is installed.
        let releases = vec![release("v1.0.0", false, false)];

        assert_eq!(choose(&releases, "0.9.0", None, false), Action::UpToDate);
        assert_eq!(
            choose(&releases, "0.9.0", Some("v1.0.0"), false),
            Action::Unavailable
        );
    }

    #[rstest]
    // The tag, as `--list` prints it...
    #[case("v1.1.0")]
    // ...and the bare version, which is what people type.
    #[case("1.1.0")]
    fn test_an_explicit_version_installs_exactly_that_release(#[case] target: &str) {
        let releases = releases();

        assert_eq!(
            choose(&releases, "1.2.0", Some(target), false),
            Action::Install(&releases[0]),
            "an explicit older version is a rollback, not a no-op"
        );
    }

    #[test]
    fn test_an_explicit_version_which_does_not_exist_is_unavailable() {
        assert_eq!(
            choose(&releases(), "1.1.0", Some("v9.9.9"), false),
            Action::Unavailable
        );
    }

    #[test]
    fn test_an_explicit_prerelease_needs_no_flag() {
        // Naming a pre-release *is* asking for it.
        let releases = releases();

        assert_eq!(
            choose(&releases, "1.1.0", Some("v1.3.0-rc.1"), false),
            Action::Install(&releases[1])
        );
    }

    // --- Development builds ------------------------------------------------

    #[test]
    fn test_a_development_build_has_nothing_to_update_to() {
        assert_eq!(
            choose(&releases(), DEVELOPMENT_VERSION, None, false),
            Action::DevelopmentBuild,
            "a cargo build binary is not an old release"
        );
    }

    #[test]
    fn test_a_version_which_is_not_semver_is_treated_as_a_development_build() {
        // Whatever went wrong, saying "this build cannot update itself" beats
        // failing with a parse error nobody can act on.
        assert_eq!(
            choose(&releases(), "not-a-version", None, false),
            Action::DevelopmentBuild
        );
    }

    #[test]
    fn test_a_development_build_can_still_install_a_named_version() {
        // Asking for a specific release by name is unambiguous, so it is
        // honoured whatever this build calls itself.
        let releases = releases();

        assert_eq!(
            choose(&releases, DEVELOPMENT_VERSION, Some("v1.2.0"), false),
            Action::Install(&releases[3])
        );
    }

    #[test]
    fn test_the_development_build_predicate_follows_the_build_profile() {
        assert_eq!(is_development_build(), cfg!(debug_assertions));
    }

    // --- The `--list` marker -----------------------------------------------

    #[test]
    fn test_a_release_is_recognised_as_installed_by_tag_or_version() {
        let release = release("v1.1.0", false, true);

        assert!(is_installed(&release, "1.1.0"));
        assert!(!is_installed(&release, "1.2.0"));
        assert!(
            !is_installed(&release, DEVELOPMENT_VERSION),
            "a development build matches no release, so nothing is marked"
        );
    }

    // --- The background check ----------------------------------------------

    #[tokio::test]
    async fn test_the_background_check_is_silent_in_a_development_build() {
        // The important half is that this makes no request at all: these tests
        // run from a `cargo build` binary, so a network call here would be a
        // network call at every game launch too.
        assert_eq!(check_for_update().await, None);
    }

    #[rstest]
    // The hook the terminal UI's indicator is smoke-tested through.
    #[case(Some("9.9.9"), Some("9.9.9"))]
    // An empty value is how you turn it off without unsetting it.
    #[case(Some(""), None)]
    #[case(None, None)]
    fn test_the_forced_update_hook(#[case] value: Option<&str>, #[case] expected: Option<&str>) {
        assert_eq!(forced_update(value).as_deref(), expected);
    }
}
