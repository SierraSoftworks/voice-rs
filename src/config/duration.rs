//! `humantime`-backed duration parsing for the profile schema.
//!
//! Every duration in a profile (`completion_timeout`, `defaults.duration`,
//! `defaults.interval`, `wait:` steps) is written the way a person would say
//! it — `300ms`, `1s`, `1s 500ms` — and is parsed *during deserialization*, so
//! a typo is a config-load error rather than a runtime surprise.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

/// Parses a humantime duration such as `300ms`, `1s` or `1s 500ms`.
pub fn parse(text: &str) -> Result<Duration, crate::Error> {
    humantime::parse_duration(text.trim()).map_err(|e| {
        human_errors::wrap_user(
            e,
            format!("We could not understand '{text}' as a length of time."),
            &[
                "Write durations with their units, e.g. '300ms', '1s', '1s 500ms' or '2m'.",
                "A bare number is not enough — we will not guess whether you meant seconds or milliseconds.",
            ],
        )
    })
}

/// Renders a duration the way a profile would write it (`300ms`, `1s`).
///
/// Used in report and log lines so that what we print back can be pasted
/// straight into a profile.
pub fn render(duration: Duration) -> String {
    humantime::format_duration(duration).to_string()
}

/// A `serde(deserialize_with = ...)` helper for a required duration field.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse(&text).map_err(serde::de::Error::custom)
}

/// A `serde(deserialize_with = ...)` helper for an optional duration field.
///
/// Pairs with `#[serde(default)]`: an absent field is `None`, and a present one
/// is parsed exactly as [`deserialize`] would.
pub fn deserialize_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(text) => parse(&text).map(Some).map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("300ms", Duration::from_millis(300))]
    #[case("30ms", Duration::from_millis(30))]
    #[case("1s", Duration::from_secs(1))]
    #[case("1s 500ms", Duration::from_millis(1500))]
    #[case("750ms", Duration::from_millis(750))]
    #[case("2m", Duration::from_secs(120))]
    #[case("0s", Duration::ZERO)]
    // Leading and trailing whitespace is a formatting accident, not an error.
    #[case("  350ms\t", Duration::from_millis(350))]
    fn test_parse_accepts_humantime(#[case] input: &str, #[case] expected: Duration) {
        assert_eq!(parse(input).expect("the duration should parse"), expected);
    }

    #[rstest]
    // A bare number could mean anything; we refuse to guess.
    #[case("300")]
    #[case("")]
    #[case("ms")]
    #[case("300 milliseconds please")]
    #[case("-30ms")]
    fn test_parse_rejects_ambiguous_input(#[case] input: &str) {
        let error = parse(input).expect_err("the duration should be rejected");
        let message = error.to_string();
        assert!(
            message.contains(&format!("'{input}'")),
            "the error should quote the input, got: {message}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[rstest]
    #[case(Duration::from_millis(300), "300ms")]
    #[case(Duration::from_secs(1), "1s")]
    #[case(Duration::ZERO, "0s")]
    fn test_render(#[case] duration: Duration, #[case] expected: &str) {
        assert_eq!(render(duration), expected);
    }

    #[derive(Debug, serde::Deserialize)]
    struct Doc {
        #[serde(deserialize_with = "deserialize")]
        timeout: Duration,
        #[serde(default, deserialize_with = "deserialize_option")]
        hold: Option<Duration>,
    }

    #[test]
    fn test_deserialize_helpers() {
        let doc: Doc = serde_yaml::from_str("timeout: 350ms\nhold: 1s 500ms\n").unwrap();
        assert_eq!(doc.timeout, Duration::from_millis(350));
        assert_eq!(doc.hold, Some(Duration::from_millis(1500)));

        let doc: Doc = serde_yaml::from_str("timeout: 1s\n").unwrap();
        assert_eq!(doc.timeout, Duration::from_secs(1));
        assert_eq!(doc.hold, None);
    }

    #[test]
    fn test_deserialize_surfaces_the_parse_error_at_load_time() {
        let error = serde_yaml::from_str::<Doc>("timeout: 300\n")
            .expect_err("a unit-less duration should be rejected");
        assert!(
            error
                .to_string()
                .contains("We could not understand '300' as a length of time."),
            "unexpected error: {error}"
        );
    }
}
