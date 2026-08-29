//! The Vosk-backed recognizer: a dedicated thread owning the `Model` and the
//! grammar-constrained `Recognizer`, fed by a bounded audio channel and
//! emitting [`RecognitionEvent`]s onto the Tokio side.
//!
//! See DESIGN.md §"Recognition (dedicated thread)". `accept_waveform` is
//! CPU-bound for the whole life of the process, so it gets a real thread rather
//! than a `spawn_blocking` slot, and shutdown is tied to channel closure: drop
//! the audio sender and the loop ends, the thread exits, and
//! [`RecognizerHandle::join`] returns.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tracing_batteries::prelude::*;

use crate::{
    errors::HumanizableError,
    recognition::{
        AudioMsg, RecognitionEvent, RecognizerOptions, Utterance, Vocabulary,
        libvosk::{self, BufferTooLong, DecodingState, LogLevel, Model, Recognizer},
    },
};

/// The out-of-grammar catch-all phrase. Without it Vosk force-aligns *any*
/// speech onto the nearest grammar phrase, which turns unrelated chatter on
/// voice comms into false triggers. See DESIGN.md §"Expansion and grammar
/// compilation".
pub const UNKNOWN_PHRASE: &str = "[unk]";

/// Capacity of the audio channel: ~800 ms of 100 ms frames. The cpal callback
/// drops the newest frame when this is full (DESIGN.md §"Audio capture").
const AUDIO_CHANNEL_CAPACITY: usize = 8;

/// How much audio to process between dropped-frame reports.
const DROP_REPORT_INTERVAL_SECS: u64 = 30;

/// The endpointer's `t_start_max`: how long *initial* silence may run before
/// recognition stops waiting for an utterance to begin. vosk-api's header
/// suggests "usually around 5.0", which is also what its stock tuning uses;
/// we keep it — the profile's `recognition.silence` is about trailing
/// silence, and nothing about eager firing changes how long we wait for
/// speech to start.
const ENDPOINTER_START_MAX_SECS: f32 = 5.0;

/// The endpointer's `t_max`: the hard cap on a single utterance's length.
/// vosk-api's header suggests "usually around 20-30"; we take the top of that
/// range so a long chain of commands spoken in one breath is never chopped,
/// while a stuck decode still cannot hold an utterance open forever.
const ENDPOINTER_MAX_UTTERANCE_SECS: f32 = 30.0;

/// A handle to the recognizer thread.
///
/// The thread is *not* stopped by dropping this handle — it stops when the
/// audio [`SyncSender`](std::sync::mpsc::SyncSender) returned alongside it is
/// dropped and the channel closes. Drop the sender, then call [`join`] to wait
/// for the decoder to wind down.
///
/// [`join`]: RecognizerHandle::join
pub struct RecognizerHandle {
    thread: std::thread::JoinHandle<()>,
}

impl RecognizerHandle {
    /// Waits for the recognizer thread to exit. Returns once the audio channel
    /// has been closed and drained.
    pub fn join(self) -> Result<(), crate::Error> {
        self.thread.join().map_err(|_| {
            human_errors::system(
                "The speech recognition thread stopped unexpectedly.",
                &["Please report this issue on GitHub so that we can investigate."],
            )
        })
    }

    /// Whether the recognizer thread has already exited.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

/// Loads a Vosk model, builds a grammar-constrained recognizer for it, and
/// moves both onto a dedicated `recognizer` thread.
///
/// The model and recognizer are constructed on the calling thread so that a
/// missing model, an unreadable directory, or a model which cannot support
/// grammars surfaces immediately as a human error rather than as a dead thread.
///
/// * `grammar` — the already-expanded, deduped phrase list; [`UNKNOWN_PHRASE`]
///   is appended if the caller has not included it.
/// * `options` — the profile's `recognition:` tuning: endpointer trailing
///   silence, and how many alternatives finalized results carry.
/// * the returned sender is bounded (8 frames); the audio callback should
///   `try_send` and count drops.
pub fn spawn_recognizer(
    model_path: &Path,
    sample_rate: u32,
    grammar: &[String],
    options: RecognizerOptions,
    events: tokio::sync::mpsc::Sender<RecognitionEvent>,
) -> Result<(RecognizerHandle, std::sync::mpsc::SyncSender<AudioMsg>), crate::Error> {
    spawn_recognizer_with_drop_counter(
        model_path,
        sample_rate,
        grammar,
        options,
        events,
        Arc::new(AtomicU64::new(0)),
    )
}

/// [`spawn_recognizer`], plus the shared dropped-frame counter which the audio
/// callback increments when the bounded channel is full.
///
/// The recognizer thread logs the counter's delta as a warning roughly every
/// 30 s of processed audio — dropped audio mid-utterance should be surfaced,
/// not silently smoothed over. `run` assembly owns the `Arc` and hands the same
/// one to `audio`; [`spawn_recognizer`] simply passes a private counter which
/// never increments.
pub fn spawn_recognizer_with_drop_counter(
    model_path: &Path,
    sample_rate: u32,
    grammar: &[String],
    options: RecognizerOptions,
    events: tokio::sync::mpsc::Sender<RecognitionEvent>,
    dropped_frames: Arc<AtomicU64>,
) -> Result<(RecognizerHandle, std::sync::mpsc::SyncSender<AudioMsg>), crate::Error> {
    quiet_vosk_logging();

    let phrases = prepare_grammar(grammar)?;
    let model = load_model(model_path)?;
    let recognizer = build_recognizer(&model, sample_rate, &phrases, options)?;

    // `Recognizer` holds a raw pointer into the model, so the model has to
    // outlive it; struct fields drop in declaration order, which puts the
    // recognizer down first.
    let mut session = Session { recognizer, model };

    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);

    info!(
        model = %model_path.display(),
        sample_rate,
        phrases = phrases.len(),
        "Starting the speech recognizer."
    );

    let thread = std::thread::Builder::new()
        .name("recognizer".into())
        .spawn(move || {
            let mut drops = DropReporter::new(sample_rate, dropped_frames);
            recognition_loop(&mut session, audio_rx, &events, &mut drops);
            debug!("The speech recognizer has shut down.");
        })
        .map_err(|e| {
            human_errors::wrap_system(
                e,
                "We could not start the speech recognition thread.",
                &["Please report this issue on GitHub so that we can investigate."],
            )
        })?;

    Ok((RecognizerHandle { thread }, audio_tx))
}

/// Owns the decoder state for the lifetime of the thread.
struct Session {
    recognizer: Recognizer,
    model: Model,
}

/// The recognizer thread's main loop. Returns when the audio channel closes or
/// the event channel is closed by the Tokio side.
fn recognition_loop(
    session: &mut Session,
    audio: std::sync::mpsc::Receiver<AudioMsg>,
    events: &tokio::sync::mpsc::Sender<RecognitionEvent>,
    drops: &mut DropReporter,
) {
    let mut last_partial = String::new();
    let mut failures = FailureGate::default();

    while let Ok(msg) = audio.recv() {
        match msg {
            AudioMsg::Frame(samples) => {
                drops.observe(samples.len() as u64);

                match session.recognizer.accept_waveform(&samples) {
                    Ok(DecodingState::Finalized) => {
                        failures.decoded();
                        last_partial.clear();

                        let transcript = session.recognizer.result();
                        if transcript.text.is_empty() {
                            continue;
                        }

                        debug!(text = %transcript.text, "Recognized an utterance.");
                        let utterance = Utterance {
                            text: transcript.text,
                            alternatives: transcript.alternatives,
                        };
                        if events
                            .blocking_send(RecognitionEvent::Final(utterance))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(DecodingState::Running) => {
                        failures.decoded();

                        let partial = session.recognizer.partial_result();
                        if let Some(text) = partial_update(&mut last_partial, partial)
                            && events
                                .blocking_send(RecognitionEvent::Partial(text))
                                .is_err()
                        {
                            break;
                        }
                    }
                    Ok(DecodingState::Failed) => {
                        // Once per run of failures: a decoder which cannot
                        // decode fails on every frame, which is 50 reports a
                        // second if each one is reported.
                        if failures.failed() {
                            warn!("The speech recognizer failed to decode a frame of audio.");
                            if events.blocking_send(RecognitionEvent::Failed).is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("{}", human_errors::pretty(&e.to_human_error()));
                    }
                }
            }
            AudioMsg::Reset => {
                discard_utterance(&mut session.recognizer, &mut last_partial);

                if events.blocking_send(RecognitionEvent::Muted).is_err() {
                    break;
                }
            }
            AudioMsg::Clear => {
                discard_utterance(&mut session.recognizer, &mut last_partial);
            }
        }
    }
}

/// Rate-limits [`RecognitionEvent::Failed`] down to one report per run of
/// failures.
///
/// A decoder which cannot decode the audio it is being given fails on *every*
/// frame — roughly fifty times a second — so reporting each one would fill the
/// log with the same line and bury everything the session is actually for. The
/// first failure is reported and the rest are suppressed until a successful
/// decode intervenes, which is exactly when the condition has changed enough to
/// be worth reporting again.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FailureGate {
    /// Whether the current run of failures has already been reported.
    reported: bool,
}

impl FailureGate {
    /// Notes a failed decode, returning whether it is worth reporting.
    fn failed(&mut self) -> bool {
        let first = !self.reported;
        self.reported = true;
        first
    }

    /// Notes a successful decode: whatever was wrong is no longer wrong, so the
    /// next failure is news again.
    fn decoded(&mut self) {
        self.reported = false;
    }
}

/// Throws away whatever utterance the recognizer is holding.
///
/// `Recognizer::reset()` alone is NOT enough: empirically (libvosk 0.3.45,
/// small-en-us and lgraph models), audio decoded before a `reset()` can still
/// finalize afterwards — feed half an utterance, `reset()`, then silence, and
/// a fragment of the pre-reset speech comes back as a `Finalized` result even
/// though the partial reads empty in between. Draining `final_result()` first
/// flushes the decoder for real. The regression test
/// `reset_discards_a_half_spoken_utterance` pins this behavior with real
/// speech audio.
fn discard_utterance(recognizer: &mut Recognizer, last_partial: &mut String) {
    let _ = recognizer.final_result();
    recognizer.reset();
    last_partial.clear();
}

/// Decides whether a partial hypothesis is worth forwarding.
///
/// Vosk repeats the same partial for every frame while the speaker holds a
/// word, so we only emit when the text changes. Empty partials (emitted between
/// utterances) carry no information for the matcher, so they update the
/// dedupe memory but are not sent.
fn partial_update(last: &mut String, partial: String) -> Option<String> {
    if partial == *last {
        return None;
    }

    *last = partial;
    if last.is_empty() {
        None
    } else {
        Some(last.clone())
    }
}

/// Returns the grammar to compile: the caller's phrases plus [`UNKNOWN_PHRASE`]
/// if it is not already present.
///
/// A phrase carrying a quote or backslash cannot appear in a grammar libvosk
/// will accept; the phrase DSL cannot produce one, but we refuse it loudly
/// rather than hand the recognizer something it will mangle.
fn prepare_grammar(grammar: &[String]) -> Result<Vec<String>, crate::Error> {
    if let Some(bad) = grammar.iter().find(|p| p.contains('"') || p.contains('\\')) {
        return Err(human_errors::user(
            format!(
                "The recognition grammar contains a phrase we cannot compile: {bad:?} includes a quote or backslash."
            ),
            &[
                "Command phrases may only contain words, '[optional]' groups and '{alternate, choices}' groups.",
            ],
        ));
    }

    let mut phrases: Vec<String> = grammar.to_vec();
    if !phrases.iter().any(|p| p == UNKNOWN_PHRASE) {
        phrases.push(UNKNOWN_PHRASE.to_string());
    }

    Ok(phrases)
}

/// Loads the model, turning every failure mode into an actionable error.
fn load_model(model_path: &Path) -> Result<Model, crate::Error> {
    if !model_path.exists() {
        return Err(human_errors::user(
            format!(
                "We could not find a Vosk speech model at '{}'.",
                model_path.display()
            ),
            &[
                "Download a model from https://alphacephei.com/vosk/models (vosk-model-small-en-us is a good start).",
                "Unpack the .zip so that the directory holding 'am', 'conf' and 'graph' is the path in your profile's 'model:' option, e.g. ~/.local/share/vosk/vosk-model-small-en-us-0.15.",
            ],
        ));
    }

    if !model_path.is_dir() || !looks_like_model(model_path) {
        return Err(human_errors::user(
            format!(
                "The path '{}' does not look like a Vosk speech model — we expected a directory with 'am', 'conf' and 'graph' sub-directories inside it.",
                model_path.display()
            ),
            &[
                "Point your profile's 'model:' option at the unpacked model directory, not at the .zip file or the directory containing it.",
                "You can download a model from https://alphacephei.com/vosk/models.",
            ],
        ));
    }

    let path = model_path.to_str().ok_or_else(|| {
        human_errors::user(
            format!(
                "The model path '{}' is not valid UTF-8, which Vosk requires.",
                model_path.display()
            ),
            &["Move the model to a directory whose path contains only valid UTF-8 characters."],
        )
    })?;

    Model::open(path)?.ok_or_else(|| {
        human_errors::user(
            format!(
                "We were unable to load the Vosk speech model at '{}'.",
                model_path.display()
            ),
            &[
                "Make sure the model directory is complete and readable — unpacking the .zip again is the quickest fix.",
                "You can download a fresh model from https://alphacephei.com/vosk/models.",
            ],
        )
    })
}

/// A cheap structural sanity check so that "you pointed at the wrong
/// directory" is reported as such instead of as a model load failure.
fn looks_like_model(model_path: &Path) -> bool {
    ["am", "conf", "graph"]
        .iter()
        .any(|entry| model_path.join(entry).is_dir())
}

/// Builds the grammar-constrained recognizer, with metadata we do not use
/// turned off, the endpointer's trailing silence set from the profile, and
/// n-best alternatives requested when confidence gating is on.
fn build_recognizer(
    model: &Model,
    sample_rate: u32,
    phrases: &[String],
    options: RecognizerOptions,
) -> Result<Recognizer, crate::Error> {
    let mut recognizer = Recognizer::with_grammar(model, sample_rate as f32, phrases)
        .ok_or_else(|| {
            human_errors::user(
                "We could not build a grammar-constrained recognizer from this speech model.",
                &[
                    "Grammars need a lookahead model (one with a 'graph/Gr.fst'); precompiled HCLG models cannot be constrained.",
                    "The small models on https://alphacephei.com/vosk/models (e.g. vosk-model-small-en-us) all support grammars.",
                ],
            )
        })?;

    // No per-word timing metadata: the matcher only ever looks at the text.
    // Alternatives stay off (a single best transcript) unless the profile's
    // confidence gating asked for an n-best list.
    recognizer.set_max_alternatives(i32::try_from(options.alternatives).unwrap_or(i32::MAX));
    recognizer.set_words(false);
    recognizer.set_partial_words(false);
    recognizer.set_nlsml(false);

    // The endpointer's trailing silence is the *floor* under every command's
    // latency — measured on real speech, shortening `t_end` from vosk's ~0.5s
    // moved finalization from ~700ms to ~400ms after the last word with
    // unchanged transcripts. The other two thresholds keep vosk's own
    // suggested values (see the constants' comments).
    //
    // The entry point only exists in libvosk 0.3.46 and newer; an older build
    // (which is every published Windows one) is not broken, it simply keeps
    // vosk's stock trailing silence — worth saying once, not worth refusing to
    // start over.
    if !recognizer.set_endpointer_delays(
        ENDPOINTER_START_MAX_SECS,
        options.silence.as_secs_f32(),
        ENDPOINTER_MAX_UTTERANCE_SECS,
    ) {
        warn!(
            "This libvosk build does not support endpointer tuning; recognition.silence has no effect."
        );
    }

    Ok(recognizer)
}

/// Quiets Kaldi's chatty startup logging to warnings so that wrapping a game
/// launch doesn't spray the terminal.
fn quiet_vosk_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| libvosk::set_log_level(LogLevel::Warn));
}

/// Periodically reports frames the audio callback had to drop because the
/// bounded channel was full.
struct DropReporter {
    counter: Arc<AtomicU64>,
    reported: u64,
    samples_since_report: u64,
    samples_per_report: u64,
}

impl DropReporter {
    fn new(sample_rate: u32, counter: Arc<AtomicU64>) -> Self {
        Self {
            counter,
            reported: 0,
            samples_since_report: 0,
            samples_per_report: u64::from(sample_rate).max(1) * DROP_REPORT_INTERVAL_SECS,
        }
    }

    /// Accounts for a processed frame, reporting drops every
    /// [`DROP_REPORT_INTERVAL_SECS`] of audio.
    fn observe(&mut self, samples: u64) {
        self.samples_since_report += samples;
        if self.samples_since_report < self.samples_per_report {
            return;
        }
        self.samples_since_report = 0;

        let total = self.counter.load(Ordering::Relaxed);
        let delta = total.saturating_sub(self.reported);
        self.reported = total;

        if delta > 0 {
            warn!(
                dropped_frames = delta,
                "We dropped {delta} frames of audio in the last {DROP_REPORT_INTERVAL_SECS}s because recognition could not keep up — commands spoken during those gaps may be missed."
            );
        }
    }
}

/// The production [`Vocabulary`]: membership straight from the model, and the
/// model's word list when it ships one.
pub struct VoskVocabulary {
    model: Model,
    model_path: PathBuf,
}

impl VoskVocabulary {
    /// Loads the model at `model_path` for vocabulary checking.
    pub fn open(model_path: &Path) -> Result<Self, crate::Error> {
        quiet_vosk_logging();

        Ok(Self {
            model: load_model(model_path)?,
            model_path: model_path.to_path_buf(),
        })
    }
}

impl Vocabulary for VoskVocabulary {
    fn contains(&mut self, word: &str) -> bool {
        self.model.find_word(word).is_some()
    }

    fn words(&self) -> Option<Vec<String>> {
        read_word_list(&self.model_path)
    }
}

/// Reads `<model>/graph/words.txt` — the FST symbol table, one
/// `<word> <id>` pair per line — returning [`None`] when the model does not
/// ship one or it cannot be read. Small models generally omit it, in which case
/// `validate` simply skips nearest-word suggestions.
fn read_word_list(model_path: &Path) -> Option<Vec<String>> {
    let contents = std::fs::read_to_string(model_path.join("graph").join("words.txt")).ok()?;

    Some(
        contents
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_owned)
            .collect(),
    )
}

impl HumanizableError for BufferTooLong {
    fn to_human_error(self) -> crate::Error {
        human_errors::wrap_system(
            self,
            "We could not hand a frame of audio to the speech recognizer.",
            &["Please report this issue on GitHub so that we can investigate."],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_reported_once_until_a_decode_succeeds() {
        let mut gate = FailureGate::default();

        // The first failure of a run is news; the rest of the run is not.
        assert!(gate.failed(), "the first failure should be reported");
        assert!(!gate.failed(), "a repeat failure should be suppressed");
        assert!(!gate.failed());

        // A successful decode means whatever was wrong is no longer wrong, so
        // the next failure is a new problem worth saying out loud.
        gate.decoded();
        assert!(gate.failed(), "a failure after a good decode is news again");
        assert!(!gate.failed());

        // Successes on their own report nothing at all.
        gate.decoded();
        gate.decoded();
        assert_eq!(gate, FailureGate { reported: false });
    }

    #[test]
    fn prepare_grammar_appends_unknown_phrase() {
        let phrases = prepare_grammar(&["deploy the sentry".to_string()]).unwrap();

        assert_eq!(
            phrases,
            vec!["deploy the sentry".to_string(), "[unk]".to_string()]
        );
    }

    #[test]
    fn prepare_grammar_keeps_a_single_unknown_phrase() {
        let phrases = prepare_grammar(&[
            "deploy the sentry".to_string(),
            UNKNOWN_PHRASE.to_string(),
            "salute".to_string(),
        ])
        .unwrap();

        assert_eq!(
            phrases.iter().filter(|p| *p == UNKNOWN_PHRASE).count(),
            1,
            "[unk] should appear exactly once: {phrases:?}"
        );
        assert_eq!(phrases.len(), 3);
    }

    #[test]
    fn prepare_grammar_rejects_unquotable_phrases() {
        let err = prepare_grammar(&["say \"hello\"".to_string()]).unwrap_err();

        assert!(
            format!("{err}").contains("quote or backslash"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn partial_update_emits_only_on_change() {
        let mut last = String::new();

        assert_eq!(
            partial_update(&mut last, "deploy".to_string()),
            Some("deploy".to_string())
        );
        assert_eq!(partial_update(&mut last, "deploy".to_string()), None);
        assert_eq!(
            partial_update(&mut last, "deploy the".to_string()),
            Some("deploy the".to_string())
        );
    }

    #[test]
    fn partial_update_skips_empty_but_still_forgets() {
        let mut last = "deploy".to_string();

        // The empty partial between utterances is not worth an event...
        assert_eq!(partial_update(&mut last, String::new()), None);
        // ...but the same text later is a genuinely new hypothesis.
        assert_eq!(
            partial_update(&mut last, "deploy".to_string()),
            Some("deploy".to_string())
        );
    }

    #[test]
    fn read_word_list_parses_the_first_column() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph");
        std::fs::create_dir_all(&graph).unwrap();
        std::fs::write(
            graph.join("words.txt"),
            "<eps> 0\ndeploy 1\nsentry 2\nautocannon 3\n",
        )
        .unwrap();

        assert_eq!(
            read_word_list(dir.path()),
            Some(vec![
                "<eps>".to_string(),
                "deploy".to_string(),
                "sentry".to_string(),
                "autocannon".to_string(),
            ])
        );
    }

    #[test]
    fn read_word_list_is_none_without_a_word_list() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(read_word_list(dir.path()), None);
    }

    #[test]
    fn looks_like_model_rejects_an_unrelated_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!looks_like_model(dir.path()));

        std::fs::create_dir(dir.path().join("conf")).unwrap();
        assert!(looks_like_model(dir.path()));
    }

    /// `Model` is not `Debug`, so `unwrap_err()` is unavailable on its results.
    fn expect_error<T>(result: Result<T, crate::Error>) -> crate::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    #[test]
    fn load_model_reports_a_missing_model() {
        let err = expect_error(load_model(Path::new("/definitely/not/a/vosk/model")));
        let rendered = human_errors::pretty(&err).to_string();

        assert!(
            rendered.contains("alphacephei.com/vosk/models"),
            "the advice should point at the model downloads: {rendered}"
        );
    }

    #[test]
    fn load_model_reports_a_directory_which_is_not_a_model() {
        let dir = tempfile::tempdir().unwrap();
        let err = expect_error(load_model(dir.path()));

        assert!(
            format!("{err}").contains("does not look like a Vosk speech model"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn drop_reporter_reports_only_at_the_interval() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut reporter = DropReporter::new(16_000, counter.clone());

        assert_eq!(reporter.samples_per_report, 16_000 * 30);

        counter.store(3, Ordering::Relaxed);
        reporter.observe(16_000);
        assert_eq!(reporter.reported, 0, "not yet a full reporting interval");

        reporter.observe(16_000 * 30);
        assert_eq!(reporter.reported, 3);
        assert_eq!(reporter.samples_since_report, 0);
    }

    // --- Tests which need the real model and libvosk ---------------------

    /// The model used by the gated tests: `$VOSK_MODEL_PATH`, or the small
    /// English model in the XDG cache.
    fn model_path() -> PathBuf {
        let path = std::env::var_os("VOSK_MODEL_PATH").map_or_else(
            || {
                PathBuf::from(std::env::var_os("HOME").expect("HOME must be set"))
                    .join(".cache/vosk/vosk-model-small-en-us-0.15")
            },
            PathBuf::from,
        );

        assert!(
            path.is_dir(),
            "no Vosk model at '{}' — download one from https://alphacephei.com/vosk/models and set VOSK_MODEL_PATH, or run with --features pure_tests to skip this test",
            path.display()
        );

        path
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_model_loads_and_answers_vocabulary_questions() {
        let mut vocabulary = VoskVocabulary::open(&model_path()).unwrap();

        assert!(vocabulary.contains("hello"));
        assert!(!vocabulary.contains("zzzxqqjv"));
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_model_word_list_matches_what_it_ships() {
        let path = model_path();
        let vocabulary = VoskVocabulary::open(&path).unwrap();

        // vosk-model-small-en-us-0.15 ships a compiled graph without the
        // symbol table, so nearest-word suggestions are unavailable for it.
        let ships_word_list = path.join("graph").join("words.txt").is_file();
        assert_eq!(vocabulary.words().is_some(), ships_word_list);
    }

    #[test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    fn real_model_builds_a_grammar_constrained_recognizer() {
        let model = load_model(&model_path()).unwrap();
        let phrases = prepare_grammar(&[
            "deploy the sentry".to_string(),
            "open the terminal".to_string(),
        ])
        .unwrap();

        build_recognizer(&model, 16_000, &phrases, RecognizerOptions::default()).unwrap();

        // The alternatives shape builds too — same model, n-best requested.
        build_recognizer(
            &model,
            16_000,
            &phrases,
            RecognizerOptions {
                alternatives: 3,
                ..RecognizerOptions::default()
            },
        )
        .unwrap();
    }

    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn reset_discards_a_half_spoken_utterance() {
        // The push-to-talk leak: speak, mute BEFORE the endpointer fires,
        // then unmute into silence. `Recognizer::reset()` alone leaves decoder
        // state behind, and the stale utterance finalizes off the incoming
        // silence — this is why `discard_utterance` drains `final_result()`
        // first. Real recorded speech (spoken digits, 16 kHz mono s16le), cut
        // off mid-utterance with no trailing silence.
        let speech: Vec<i16> = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data/speech-digits-16k-mono.raw"),
        )
        .expect("the speech fixture should exist")
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();

        let grammar: Vec<String> = ["one", "zero", "nine", "oh", "two", "eight", "three"]
            .into_iter()
            .map(String::from)
            .collect();

        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
        // The endpointer keeps vosk's stock ~500ms trailing silence here: this
        // test pins *reset* behavior, and a shorter `t_end` could let a pause
        // between the recorded digits finalize an utterance before the Reset
        // ever arrives, which is not the leak being tested.
        let options = RecognizerOptions {
            silence: std::time::Duration::from_millis(500),
            alternatives: 0,
        };
        let (handle, audio) =
            spawn_recognizer(&model_path(), 16_000, &grammar, options, events_tx).unwrap();

        // Speak — the decoder builds up an utterance ("one zero zero ...").
        for chunk in speech.chunks(1_600) {
            audio.send(AudioMsg::Frame(chunk.to_vec())).unwrap();
        }
        // Mute mid-utterance, then unmute (the bridge's Clear), then listen
        // to two seconds of silence — plenty for the endpointer to fire on
        // any state the reset failed to discard.
        audio.send(AudioMsg::Reset).unwrap();
        audio.send(AudioMsg::Clear).unwrap();
        audio.send(AudioMsg::Frame(vec![0i16; 32_000])).unwrap();
        drop(audio);

        let mut finals = Vec::new();
        while let Some(event) = events_rx.recv().await {
            if let RecognitionEvent::Final(utterance) = event {
                finals.push(utterance.text);
            }
        }

        assert!(
            finals.is_empty(),
            "a muted half-spoken utterance must never finalize, but got: {finals:?}"
        );

        tokio::task::spawn_blocking(move || handle.join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn real_recognizer_thread_runs_and_shuts_down_with_the_channel() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
        let (handle, audio) = spawn_recognizer(
            &model_path(),
            16_000,
            &["deploy the sentry".to_string()],
            RecognizerOptions::default(),
            events_tx,
        )
        .unwrap();

        // A second of silence: no utterance, but the loop must survive it.
        audio.send(AudioMsg::Frame(vec![0i16; 16_000])).unwrap();
        audio.send(AudioMsg::Reset).unwrap();

        assert_eq!(events_rx.recv().await, Some(RecognitionEvent::Muted));

        drop(audio);
        assert_eq!(events_rx.recv().await, None, "the events channel closes");

        tokio::task::spawn_blocking(move || handle.join())
            .await
            .unwrap()
            .unwrap();
    }
}
