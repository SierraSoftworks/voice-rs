//! Speech recognition: the seam between the audio pipeline and the matcher.
//!
//! The Vosk-backed implementation lives in `vosk.rs` (a dedicated thread which
//! owns the model and grammar-constrained recognizer); everything downstream
//! depends only on the contract types in this module so it can be tested
//! without libvosk. See DESIGN.md §"Recognition (dedicated thread)".
//!
//! `libvosk.rs` sits underneath it and holds the FFI itself: the library is
//! `dlopen`ed on first use rather than linked, so a machine without it can
//! still start voice-orders and be told what to install.

#![allow(dead_code)] // consumed as the wave-1 modules land

pub mod libvosk;
pub mod vosk;

/// An event emitted by the recognizer towards the matcher.
#[derive(Debug, Clone, PartialEq)]
pub enum RecognitionEvent {
    /// An in-progress hypothesis; unstable and may be revised. Emitted only
    /// when the hypothesis text changes.
    Partial(String),
    /// An utterance finalized by Vosk's silence endpointer.
    Final(Utterance),
    /// Listening was turned off; the matcher must clear all pending state.
    Muted,
    /// The recognizer could not decode the audio it was given.
    ///
    /// Emitted once per run of failures rather than once per frame (see
    /// [`vosk::FailureGate`]): a decoder which cannot decode fails on *every*
    /// frame, and a report per frame would bury everything else. The matcher
    /// ignores it — it says nothing about what was said — but a session's UI
    /// shows it, because "the recognizer is failing" is the difference between
    /// a profile which does not match and a machine which cannot listen.
    Failed,
}

/// A finalized utterance, with the n-best list when the profile asked for one.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// The 1-best transcript — what the matcher walks, and what every report
    /// prints.
    pub text: String,
    /// Every alternative with its **unnormalized** confidence, best first;
    /// empty unless `recognition.alternatives` is non-zero. Only the *margin*
    /// between two entries of the same utterance carries meaning.
    pub alternatives: Vec<(String, f32)>,
}

impl Utterance {
    /// An utterance carrying only its text — the single-best (default) shape.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            alternatives: Vec::new(),
        }
    }
}

/// Recognizer tuning from the profile's `recognition:` block — the slice of it
/// the decoder thread needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecognizerOptions {
    /// The endpointer's trailing-silence threshold (`t_end`): how much silence
    /// after speech finalizes an utterance.
    pub silence: std::time::Duration,
    /// How many alternative transcripts finalized results carry; `0` keeps the
    /// single-best shape.
    pub alternatives: u32,
}

impl Default for RecognizerOptions {
    /// Mirrors `config::RecognitionConfig`'s defaults; a config test pins the
    /// two together so they cannot drift.
    fn default() -> Self {
        Self {
            silence: std::time::Duration::from_millis(200),
            alternatives: 0,
        }
    }
}

/// A message on the audio channel into the recognizer thread.
pub enum AudioMsg {
    /// A frame of 16-bit mono PCM at the recognizer's sample rate.
    Frame(Vec<i16>),
    /// Listening was turned off; the recognizer resets its decoder state so a
    /// half-spoken phrase cannot leak across a mute boundary, and emits
    /// [`RecognitionEvent::Muted`] so the matcher clears its state too.
    Reset,
    /// Listening was turned back on; the recognizer discards anything it may
    /// have accumulated while muted (e.g. a frame that raced the mute), so
    /// listening always starts from a clean decoder. No event is emitted.
    Clear,
}

/// Vocabulary membership for a speech model. Object-safe so that `validate`
/// and its tests can run against a fake without touching libvosk.
pub trait Vocabulary {
    /// Whether the model can recognize this word (`Model::find_word`).
    fn contains(&mut self, word: &str) -> bool;

    /// The model's full word list (`<model>/graph/words.txt`) when it ships
    /// one in readable form; used for nearest-word suggestions.
    fn words(&self) -> Option<Vec<String>>;
}
