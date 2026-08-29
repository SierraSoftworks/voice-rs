//! The matcher: walks recognized utterances through the grammar's automaton
//! and resolves ambiguous prefixes with the completion-timeout state machine.
//! The machinery itself lives in [`engine`]; this module holds the vocabulary
//! the engine shares with the pipeline assembly — the options it runs under,
//! the actions it produces, and where its warnings go.
//!
//! With eager matching off ([`MatcherOptions::eager`]), commands fire on
//! `Final` results only and partials are used solely to hold a pending timer
//! open. With it on (the default), commands additionally fire from *stable
//! partial hypotheses*: a command the greedy walk has resynced past fires
//! immediately, an unambiguous resting match fires once the hypothesis holds
//! still for `eager_delay`, and an ambiguous resting match starts its
//! completion wait at the partial rather than at finalization. The eventual
//! `Final` is reconciled against what already fired.

pub mod engine;

use std::sync::Arc;
use std::time::Duration;

use crate::output::CompiledOutput;

/// Where the matcher's user-facing warnings go (eager mismatches, suppressed
/// utterances). The pipeline adapts this onto the session's event sink so the
/// warnings reach the terminal UI and plain reports as `warning:` lines; the
/// matcher itself stays ignorant of any UI.
pub type WarningSink = Arc<dyn Fn(String) + Send + Sync>;

/// How the matcher resolves what it hears — the profile's `recognition:`
/// block plus `completion_timeout`, threaded through as one struct so the
/// eager escape hatch is a field rather than a bool soup.
#[derive(Clone)]
pub struct MatcherOptions {
    /// How long an ambiguous match waits for the speaker to continue.
    pub completion_timeout: Duration,
    /// Whether commands may fire from stable partial hypotheses at all. With
    /// this off the matcher behaves exactly as it originally did: `Final`-only
    /// firing, partials only ever extending a pending deadline.
    pub eager: bool,
    /// How long an unambiguous partial must hold still before it fires.
    pub eager_delay: Duration,
    /// Suppress an utterance whose n-best list contains a different-command
    /// alternative within this confidence margin of the winner.
    pub confidence_margin: f32,
    /// Where user-facing warnings are reported.
    pub warn: WarningSink,
}

impl MatcherOptions {
    /// The baseline configuration: `Final`-only firing with the given
    /// completion timeout, no eager path, no confidence gating, warnings
    /// only logged. What most tests mean.
    pub fn with_timeout(completion_timeout: Duration) -> Self {
        Self {
            completion_timeout,
            eager: false,
            eager_delay: Duration::from_millis(100),
            confidence_margin: 3.0,
            warn: Arc::new(|_| {}),
        }
    }

    /// The options a profile's `completion_timeout` and `recognition:` block
    /// add up to.
    pub fn from_profile(profile: &crate::config::Profile, warn: WarningSink) -> Self {
        Self {
            eager: profile.recognition.eager(),
            eager_delay: profile.recognition.eager_delay,
            confidence_margin: profile.recognition.confidence_margin,
            warn,
            ..Self::with_timeout(profile.completion_timeout)
        }
    }
}

/// A recognized command ready for execution, flowing through the command
/// queue from the matcher to the executor.
#[derive(Debug, Clone)]
pub struct CommandAction {
    /// The command's display name — the matched rule plus its captures'
    /// words, e.g. `Watch(two, east)` — for logging.
    pub command: String,
    /// The assembled output plan to execute.
    pub output: CompiledOutput,
}
