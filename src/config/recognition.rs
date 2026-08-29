//! The `recognition:` block: the profile's latency levers.
//!
//! Three independent mechanisms trade recognition latency against certainty
//! (see DESIGN.md §"Endpointing and latency"):
//!
//! - `silence` tunes the recognizer's endpointer, which decides how much
//!   trailing silence turns speech into a finalized utterance;
//! - `eager` + `eager_delay` let the matcher fire commands from *stable
//!   partial* hypotheses instead of waiting for that finalization at all;
//! - `alternatives` + `confidence_margin` ask the recognizer for its n-best
//!   list and suppress an utterance whose close runners-up would have run
//!   different commands.
//!
//! Eager firing and confidence gating are mutually exclusive per utterance —
//! alternatives only exist on *finalized* results, so an eagerly fired command
//! can never be confidence-checked. The schema makes that an explicit config
//! error rather than a silently ignored option.

use std::time::Duration;

use super::duration;

/// `recognition.silence`: the endpointer's trailing-silence threshold
/// (Vosk's own default is ~500ms).
fn default_silence() -> Duration {
    Duration::from_millis(200)
}

/// `recognition.eager_delay`: how long a partial hypothesis must hold still
/// before an unambiguous match fires from it.
fn default_eager_delay() -> Duration {
    Duration::from_millis(100)
}

/// `recognition.confidence_margin`: how close a competing alternative's
/// confidence must be to the winner's before the utterance is suppressed.
fn default_confidence_margin() -> f32 {
    3.0
}

/// The optional `recognition:` block. Absent means every default below.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionConfig {
    /// How much trailing silence finalizes an utterance (the endpointer's
    /// `t_end`).
    #[serde(
        default = "default_silence",
        deserialize_with = "duration::deserialize"
    )]
    pub silence: Duration,

    /// Whether commands may fire from stable partial hypotheses before the
    /// endpointer finalizes the utterance.
    ///
    /// Kept as the raw tri-state so that "unset" can default differently
    /// depending on `alternatives` — read it through [`Self::eager`].
    #[serde(default)]
    eager: Option<bool>,

    /// How long an unambiguous partial must stay unchanged before it fires
    /// (only meaningful with `eager` on).
    #[serde(
        default = "default_eager_delay",
        deserialize_with = "duration::deserialize"
    )]
    pub eager_delay: Duration,

    /// How many alternative transcripts to request on finalized results;
    /// `0` disables confidence gating entirely.
    #[serde(default)]
    pub alternatives: u32,

    /// Suppress the utterance when a different-command alternative's
    /// confidence is within this margin of the winner's.
    ///
    /// Vosk's confidences are unnormalized path scores, so only the *gap*
    /// between alternatives means anything — an absolute threshold does not.
    #[serde(default = "default_confidence_margin")]
    pub confidence_margin: f32,
}

impl Default for RecognitionConfig {
    fn default() -> Self {
        Self {
            silence: default_silence(),
            eager: None,
            eager_delay: default_eager_delay(),
            alternatives: 0,
            confidence_margin: default_confidence_margin(),
        }
    }
}

impl RecognitionConfig {
    /// Whether eager (partial-driven) firing is on.
    ///
    /// Defaults to `true` — the latency win is the point — *unless*
    /// `alternatives` enables confidence gating, in which case an unset
    /// `eager` defaults to `false`: gating needs the finalized n-best list,
    /// which an eagerly fired command never waits for.
    pub fn eager(&self) -> bool {
        self.eager.unwrap_or(self.alternatives == 0)
    }

    /// The recognizer-side slice of this block.
    pub fn recognizer_options(&self) -> crate::recognition::RecognizerOptions {
        crate::recognition::RecognizerOptions {
            silence: self.silence,
            alternatives: self.alternatives,
        }
    }

    /// The cross-field invariants serde cannot express.
    pub fn validate(&self) -> Result<(), crate::Error> {
        if self.eager == Some(true) && self.alternatives > 0 {
            return Err(human_errors::user(
                format!(
                    "Your 'recognition:' block sets both 'eager: true' and 'alternatives: {}', which cannot work together: alternatives (and the confidence gating they enable) only exist once the recognizer has finalized an utterance, while eager firing acts on partial hypotheses before that ever happens — so an eagerly fired command could never be confidence-checked.",
                    self.alternatives
                ),
                &[
                    "Remove 'eager: true' to use confidence gating ('alternatives' implies 'eager: false' when 'eager' is not set).",
                    "Or remove 'alternatives' to keep eager, low-latency firing.",
                ],
            ));
        }

        if !self.confidence_margin.is_finite() || self.confidence_margin < 0.0 {
            return Err(human_errors::user(
                format!(
                    "Your 'recognition.confidence_margin' is {}, but it must be a non-negative number.",
                    self.confidence_margin
                ),
                &[
                    "Set it to how close a competing alternative may score before the utterance is suppressed, e.g. 'confidence_margin: 3.0'.",
                ],
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn parse(yaml: &str) -> Result<RecognitionConfig, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    #[test]
    fn test_an_empty_block_is_all_defaults() {
        let config = parse("{}").expect("an empty block should load");

        assert_eq!(config, RecognitionConfig::default());
        assert_eq!(config.silence, Duration::from_millis(200));
        assert!(config.eager(), "eager defaults on");
        assert_eq!(config.eager_delay, Duration::from_millis(100));
        assert_eq!(config.alternatives, 0);
        assert_eq!(config.confidence_margin, 3.0);
        config.validate().expect("the defaults should validate");
    }

    #[test]
    fn test_the_defaults_agree_with_the_recognizer_side() {
        // The recognizer's own `Default` (used by tests which never load a
        // profile) must not drift from the schema's.
        assert_eq!(
            RecognitionConfig::default().recognizer_options(),
            crate::recognition::RecognizerOptions::default()
        );
    }

    #[test]
    fn test_every_field_parses() {
        let config = parse(
            "silence: 150ms\neager: false\neager_delay: 250ms\nalternatives: 5\nconfidence_margin: 1.5\n",
        )
        .expect("the block should load");

        assert_eq!(config.silence, Duration::from_millis(150));
        assert!(!config.eager());
        assert_eq!(config.eager_delay, Duration::from_millis(250));
        assert_eq!(config.alternatives, 5);
        assert_eq!(config.confidence_margin, 1.5);
        config.validate().expect("the block should validate");
    }

    #[rstest]
    // Unset eager: on without alternatives, off with them.
    #[case("{}", true)]
    #[case("alternatives: 3", false)]
    // An explicit eager: false always wins.
    #[case("eager: false", false)]
    #[case("eager: false\nalternatives: 3", false)]
    // An explicit eager: true stands alone (with alternatives it is a
    // validation error, tested separately).
    #[case("eager: true", true)]
    fn test_eager_defaults_off_when_alternatives_are_on(#[case] yaml: &str, #[case] eager: bool) {
        let config = parse(yaml).expect("the block should load");
        assert_eq!(config.eager(), eager, "for {yaml:?}");
    }

    #[test]
    fn test_eager_with_alternatives_is_a_config_error() {
        let config = parse("eager: true\nalternatives: 3\n").expect("the block should load");
        let error = config
            .validate()
            .expect_err("the combination is impossible");

        let message = error.to_string();
        assert!(
            message.contains("finalized") && message.contains("partial"),
            "the error should explain the per-utterance incompatibility, got: {message}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[rstest]
    #[case("confidence_margin: -1.0")]
    #[case("confidence_margin: .nan")]
    fn test_a_nonsensical_margin_is_rejected(#[case] yaml: &str) {
        let config = parse(yaml).expect("the block should load");
        let error = config
            .validate()
            .expect_err("the margin should be rejected");
        assert!(
            error.to_string().contains("confidence_margin"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_unknown_fields_fail_loudly() {
        let error = parse("silense: 150ms\n").expect_err("a typo should be caught");
        assert!(
            error.to_string().contains("silense"),
            "the error should name the unknown field, got: {error}"
        );
    }

    #[test]
    fn test_a_bare_number_duration_is_rejected() {
        let error = parse("silence: 200\n").expect_err("a unit-less duration should be rejected");
        assert!(
            error.to_string().contains("length of time"),
            "unexpected error: {error}"
        );
    }
}
