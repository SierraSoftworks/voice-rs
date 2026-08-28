//! Profile source resolution: local paths (with `~` expansion) and https://
//! URLs (with GitHub Gist conveniences). See DESIGN.md §"Loading profiles
//! from URLs".
//!
//! The rules, in order:
//!
//! 1. `https://…` is fetched with reqwest (rustls, redirects followed).
//! 2. `http://…` is rejected outright — profiles drive your keyboard, so we
//!    are not willing to take one off an unauthenticated transport.
//! 3. anything else is a local path, with `~` expanded against `$HOME`.
//!
//! There is deliberately **no caching**: a `run` at game-launch time should
//! fail loudly rather than silently start with a stale profile.

#![allow(dead_code)] // consumed as the wave-2 modules land

use crate::errors::HumanizableError;
use tracing_batteries::prelude::*;

/// How long we are willing to wait for a remote profile, in total.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A resolved profile source: where it came from and its raw YAML text.
///
/// `source` is the *resolved* location — a tilde-expanded path, or the final
/// URL after redirects — so error messages downstream point at the thing we
/// actually read rather than the thing the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedProfile {
    /// The resolved path or final URL, for display in messages and logs.
    pub source: String,
    /// The raw YAML text of the profile.
    pub content: String,
}

/// Resolves a profile argument (a path or an `https://` URL) to its raw YAML.
///
/// See the module documentation for the resolution rules.
pub async fn load(profile: &str) -> Result<LoadedProfile, crate::Error> {
    if has_scheme(profile, "https://") {
        fetch(profile).await
    } else if has_scheme(profile, "http://") {
        Err(human_errors::user(
            format!(
                "We only download profiles over HTTPS, but '{profile}' uses plain, unencrypted HTTP."
            ),
            &[
                "Use an 'https://' URL instead — GitHub, Gists and every other common profile host serve HTTPS.",
                "If the profile is only available over HTTP, download it yourself and pass the path to the local file instead.",
            ],
        ))
    } else {
        read_local(profile).await
    }
}

/// Rewrites a `gist.github.com` URL to its `/raw` equivalent.
///
/// Pasting the address bar of a Gist is the obvious thing to do and gets you
/// an HTML page, so a `gist.github.com` URL whose path has no `raw` segment
/// gains one (which GitHub resolves to the first file of the latest revision).
/// URLs on `gist.githubusercontent.com` / `raw.githubusercontent.com`, URLs
/// which already name a `raw` segment, and every non-Gist URL pass through
/// unchanged.
pub fn rewrite_gist_url(url: &str) -> String {
    let Some((host, path)) = split_host(url) else {
        return url.to_string();
    };

    if !host.eq_ignore_ascii_case("gist.github.com") {
        return url.to_string();
    }

    if path.split('/').any(|segment| segment == "raw") {
        return url.trim_end_matches('/').to_string();
    }

    format!("{}/raw", url.trim_end_matches('/'))
}

/// Downloads a profile over HTTP(S).
///
/// Split out from [`load`] so that tests (and wiremock, which only speaks
/// plain HTTP) can exercise the fetch path without the https-only guard.
async fn fetch(url: &str) -> Result<LoadedProfile, crate::Error> {
    let url = rewrite_gist_url(url);
    debug!("Downloading profile from {url}");

    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(format!("voice-orders/{}", version!()))
        .build()
        .map_err(|e| e.to_human_error())?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_human_error())?;

    // Redirects have already been followed by this point, so this is the URL
    // the bytes actually came from.
    let source = response.url().to_string();
    let status = response.status();

    if !status.is_success() {
        return Err(human_errors::user(
            format!(
                "We received a '{status}' response when downloading the profile from '{source}'."
            ),
            &[
                "Check that the URL is correct and that the profile is publicly accessible — we do not send any credentials, so private Gists and repositories will not work.",
                "If you are pointing at a GitHub Gist, use the 'Raw' button on the file to get its raw URL.",
            ],
        ));
    }

    let content = response.text().await.map_err(|e| e.to_human_error())?;

    if looks_like_html(&content) {
        return Err(human_errors::user(
            format!(
                "The profile we downloaded from '{source}' looks like a web page, not a YAML profile."
            ),
            &[
                "Use the raw file URL rather than the page you see in your browser — on a GitHub Gist, the 'Raw' button gives you it.",
                "Open the URL in your browser to check that it serves the profile itself and not a login or error page.",
            ],
        ));
    }

    debug!(
        "Downloaded {} bytes of profile from {source}",
        content.len()
    );
    Ok(LoadedProfile { source, content })
}

/// Reads a profile from the local filesystem, expanding a leading `~`.
async fn read_local(path: &str) -> Result<LoadedProfile, crate::Error> {
    let expanded = shellexpand::tilde(path).into_owned();
    debug!("Reading profile from {expanded}");

    match tokio::fs::read_to_string(&expanded).await {
        Ok(content) => Ok(LoadedProfile {
            source: expanded,
            content,
        }),
        Err(e) => {
            // The shared io impl explains *what* went wrong; we wrap it so the
            // message also names the file we were trying to open.
            let actionable = matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            );

            let message = format!("We could not read the profile at '{expanded}'.");
            let inner = e.to_human_error();

            Err(if actionable {
                human_errors::wrap_user(
                    inner,
                    message,
                    &[
                        "Check that the path is spelled correctly and that the file exists.",
                        "You can also pass an 'https://' URL to load a profile which is published online.",
                    ],
                )
            } else {
                human_errors::wrap_system(
                    inner,
                    message,
                    &["Please report this issue on GitHub so that we can investigate."],
                )
            })
        }
    }
}

/// Case-insensitively checks whether `url` starts with the given URL scheme.
fn has_scheme(url: &str, scheme: &str) -> bool {
    url.get(..scheme.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
}

/// Splits a URL into its host and its path (including the leading `/`, and any
/// query or fragment). Returns `None` when the URL has no `://` scheme.
fn split_host(url: &str) -> Option<(&str, &str)> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some((&rest[..end], &rest[end..]))
}

/// Detects the classic "I pasted the pretty Gist page" mistake.
fn looks_like_html(body: &str) -> bool {
    let prefix: String = body
        .trim_start()
        .chars()
        .take(9)
        .flat_map(char::to_lowercase)
        .collect();

    prefix.starts_with("<!doctype") || prefix.starts_with("<html")
}

impl HumanizableError for reqwest::Error {
    fn to_human_error(self) -> crate::Error {
        if self.is_connect() {
            human_errors::wrap_user(
                self,
                "We could not connect to the remote server to download the profile.",
                &[
                    "Make sure that your internet connection is working correctly and that the server is not blocked by your firewall.",
                    "If the network is unreliable, download the profile to a local file and pass its path instead.",
                ],
            )
        } else if self.is_timeout() {
            human_errors::wrap_user(
                self,
                "We timed out while making a web request.",
                &[
                    "This is usually caused by a slow or unreliable network connection, or a remote server which is temporarily overloaded. Please try again later.",
                    "If the network is unreliable, download the profile to a local file and pass its path instead.",
                ],
            )
        } else {
            human_errors::wrap_system(
                self,
                "An unexpected error occurred while making a web request.",
                &[
                    "Please read the error above and decide whether there is something you can do to fix the problem, or report it to us on GitHub.",
                ],
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[rstest]
    // A bare Gist page URL gains its /raw segment...
    #[case(
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db",
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/raw"
    )]
    // ...and a trailing slash does not produce a doubled one.
    #[case(
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/",
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/raw"
    )]
    // Already-raw URLs are left alone rather than gaining a second /raw.
    #[case(
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/raw",
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/raw"
    )]
    #[case(
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/raw/1f0e3d/profile.yaml",
        "https://gist.github.com/octocat/aa5a315d61ae9438b18d0912c4e075db/raw/1f0e3d/profile.yaml"
    )]
    // The raw content hosts already serve the file itself.
    #[case(
        "https://gist.githubusercontent.com/octocat/aa5a315d/raw/1f0e3d/profile.yaml",
        "https://gist.githubusercontent.com/octocat/aa5a315d/raw/1f0e3d/profile.yaml"
    )]
    #[case(
        "https://raw.githubusercontent.com/octocat/profiles/main/drg.yaml",
        "https://raw.githubusercontent.com/octocat/profiles/main/drg.yaml"
    )]
    // Everything else is none of our business.
    #[case(
        "https://example.com/profiles/drg.yaml",
        "https://example.com/profiles/drg.yaml"
    )]
    fn rewrite_gist_url_cases(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(rewrite_gist_url(input), expected);
    }

    #[tokio::test]
    async fn fetch_returns_the_response_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/profile.yaml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("name: Deep Rock Galactic\n"))
            .mount(&server)
            .await;

        let url = format!("{}/profile.yaml", server.uri());
        let loaded = fetch(&url).await.expect("the profile should download");

        assert_eq!(loaded.content, "name: Deep Rock Galactic\n");
        assert_eq!(loaded.source, url, "the source should be the fetched URL");
    }

    #[tokio::test]
    async fn fetch_reports_the_status_and_url_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.yaml"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let url = format!("{}/missing.yaml", server.uri());
        let err = fetch(&url).await.expect_err("a 404 should be an error");

        let message = err.to_string();
        assert!(
            message.contains("404"),
            "the error should name the status code, got: {message}"
        );
        assert!(
            message.contains(&url),
            "the error should name the URL, got: {message}"
        );
        assert!(
            err.is(human_errors::Kind::User),
            "a bad URL is the user's to fix, got: {message}"
        );
    }

    #[tokio::test]
    async fn fetch_rejects_html_responses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pretty"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "\n<!DOCTYPE html>\n<html lang=\"en\"><body>a gist page</body></html>\n",
            ))
            .mount(&server)
            .await;

        let url = format!("{}/pretty", server.uri());
        let err = fetch(&url)
            .await
            .expect_err("an HTML body should be rejected");

        let message = err.to_string();
        assert!(
            message.contains("web page"),
            "the error should explain that this is a web page, got: {message}"
        );
        assert!(
            message.contains("Raw"),
            "the advice should point at the raw URL, got: {message}"
        );
        assert!(err.is(human_errors::Kind::User));
    }

    #[rstest]
    #[case("<html><body>hi</body></html>")]
    #[case("  \n\t<!doctype html>")]
    #[case("<!DOCTYPE HTML PUBLIC>")]
    #[case("<HTML>")]
    fn looks_like_html_detects_pages(#[case] body: &str) {
        assert!(looks_like_html(body), "expected {body:?} to look like HTML");
    }

    #[rstest]
    #[case("name: Deep Rock Galactic\n")]
    #[case("")]
    #[case("# <html> in a comment\nname: x\n")]
    fn looks_like_html_ignores_yaml(#[case] body: &str) {
        assert!(
            !looks_like_html(body),
            "expected {body:?} to look like YAML"
        );
    }

    #[tokio::test]
    async fn load_refuses_plain_http_urls() {
        // Note that this must not make a request at all — the guard is about
        // the transport, not about what the server happens to answer.
        let err = load("http://example.com/profile.yaml")
            .await
            .expect_err("plain HTTP should be refused");

        let message = err.to_string();
        assert!(
            message.contains("HTTPS"),
            "the error should mention HTTPS, got: {message}"
        );
        assert!(
            message.contains("http://example.com/profile.yaml"),
            "the error should name the URL, got: {message}"
        );
        assert!(err.is(human_errors::Kind::User));
    }

    #[tokio::test]
    async fn load_reads_a_local_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let file = dir.path().join("profile.yaml");
        tokio::fs::write(&file, "name: Deep Rock Galactic\n")
            .await
            .expect("the profile should be written");

        let path = file.to_str().expect("a UTF-8 path");
        let loaded = load(path).await.expect("the profile should load");

        assert_eq!(loaded.content, "name: Deep Rock Galactic\n");
        assert_eq!(loaded.source, path);
    }

    #[tokio::test]
    async fn load_names_the_path_when_a_local_file_is_missing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let file = dir.path().join("nope.yaml");
        let path = file.to_str().expect("a UTF-8 path");

        let err = load(path).await.expect_err("a missing file should error");

        let message = err.to_string();
        assert!(
            message.contains(path),
            "the error should name the missing path, got: {message}"
        );
        assert!(
            err.is(human_errors::Kind::User),
            "a missing profile is the user's to fix, got: {message}"
        );
    }

    #[tokio::test]
    async fn load_expands_a_leading_tilde() {
        // We deliberately point at a file which will not exist so that this
        // test depends on nothing but the *expansion*: the error names the
        // resolved path, which proves the '~' was replaced with $HOME.
        let Ok(home) = std::env::var("HOME") else {
            return;
        };

        let err = load("~/voice-orders-no-such-profile-9f2a.yaml")
            .await
            .expect_err("a missing file should error");

        let message = err.to_string();
        assert!(
            message.contains(&format!("{home}/voice-orders-no-such-profile-9f2a.yaml")),
            "the tilde should have been expanded against $HOME ({home}), got: {message}"
        );
    }

    #[tokio::test]
    async fn connection_failures_are_user_errors() {
        // Port 1 on localhost is (essentially) never listening, so the request
        // fails at connect time — a transient network problem the user can act
        // on, not a bug worth paging anybody about.
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connecting to a closed port should fail");

        let human = err.to_human_error();
        assert!(
            human.is(human_errors::Kind::User),
            "connectivity failures should be user errors, got: {human}"
        );
    }

    #[tokio::test]
    async fn timeouts_are_user_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
            .mount(&server)
            .await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .expect("a reqwest client");

        let err = client
            .get(server.uri())
            .send()
            .await
            .expect_err("the request should time out");

        assert!(err.is_timeout(), "expected a timeout, got: {err}");
        assert!(
            err.to_human_error().is(human_errors::Kind::User),
            "timeouts should be user errors"
        );
    }
}
