//! Recording-driven pipeline tests: real microphone recordings, the real
//! recognizer, and the real engine, with every event timestamped so the
//! latency claims in DESIGN.md §"Endpointing and latency" stay measured
//! rather than remembered.
//!
//! These exist because of a field report: speaking "auto cannon sentry" (an
//! unambiguous phrase — nothing extends it) appeared to take as long as the
//! ambiguous "auto cannon", suggesting the eager path was waiting out the
//! completion timeout it should not need. The tests feed the actual recordings
//! through the pipeline at real-time pacing and assert *when* each command
//! reaches the queue relative to the utterance's `Final` — which is the fact
//! the report needed: the unambiguous phrase fires from its stable partial
//! well before the endpointer finalizes, and the delay the user saw was the
//! UI holding feedback until the `Final`, not the engine holding keys.
//!
//! Gated exactly like the other model-dependent tests (`--features
//! pure_tests` skips them; `VOSK_MODEL_PATH` overrides the model location).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::grammar::{Automaton, Grammar};
use crate::matcher::MatcherOptions;
use crate::matcher::engine::engine_task;
use crate::output::assembly::Pacing;
use crate::recognition::{AudioMsg, RecognitionEvent, RecognizerOptions, vosk::spawn_recognizer};

/// The recognizer's sample rate, which the recordings are converted down to.
const SAMPLE_RATE: u32 = 16_000;

/// How much audio each frame carries: 100ms, the cadence the real capture
/// callback delivers, so the recognizer emits partials on the schedule the
/// field session saw.
const FRAME: usize = (SAMPLE_RATE as usize) / 10;

/// The model used by the gated tests: `$VOSK_MODEL_PATH`, or the small
/// English model in the XDG cache — the same resolution the recognizer's own
/// gated tests use.
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

// --- WAV decoding ----------------------------------------------------------

/// A decoded recording: PCM samples and the rate they were captured at.
struct Recording {
    samples: Vec<i16>,
    sample_rate: u32,
}

/// Parses a RIFF/WAVE file: chunk-walks to `fmt ` and `data`, accepting
/// plain PCM and the WAVE_FORMAT_EXTENSIBLE wrapper around it (which is what
/// desktop recorders actually write), and downmixes to mono by averaging.
///
/// Hand-rolled rather than a dev-dependency because the format the recordings
/// use is a fixed target: 16-bit integer PCM in a RIFF container, nothing
/// else needs to load.
fn decode_wav(bytes: &[u8]) -> Recording {
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);

    assert!(
        bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "the fixture is not a RIFF/WAVE file"
    );

    let mut format: Option<(u16, u16, u32, u16)> = None; // (tag, channels, rate, bits)
    let mut data: Option<&[u8]> = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(at + 4) as usize;
        let body = at + 8;
        assert!(body + size <= bytes.len(), "a truncated {id:?} chunk");

        match id {
            b"fmt " => {
                assert!(size >= 16, "the fmt chunk is too short");
                let mut tag = u16_at(body);
                // WAVE_FORMAT_EXTENSIBLE: the real format is the first two
                // bytes of the SubFormat GUID at offset 24 of the chunk.
                if tag == 0xFFFE {
                    assert!(size >= 40, "an extensible fmt chunk without its GUID");
                    tag = u16_at(body + 24);
                }
                format = Some((tag, u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
            }
            b"data" => data = Some(&bytes[body..body + size]),
            // JUNK, LIST, fact, ... — padding and metadata, all skippable.
            _ => {}
        }

        // Chunks are word-aligned: an odd size is followed by a pad byte.
        at = body + size + (size % 2);
    }

    let (tag, channels, sample_rate, bits) = format.expect("the fixture has no fmt chunk");
    let data = data.expect("the fixture has no data chunk");
    assert_eq!(tag, 1, "the fixture is not integer PCM");
    assert_eq!(bits, 16, "the fixture is not 16-bit");
    assert!(channels >= 1, "the fixture has no channels");

    // Interleaved s16le frames, downmixed to mono by averaging the channels.
    let channels = channels as usize;
    let samples = data
        .chunks_exact(2 * channels)
        .map(|frame| {
            let sum: i32 = frame
                .chunks_exact(2)
                .map(|s| i32::from(i16::from_le_bytes([s[0], s[1]])))
                .sum();
            (sum / channels as i32) as i16
        })
        .collect();

    Recording {
        samples,
        sample_rate,
    }
}

/// Converts a recording to the recognizer's 16 kHz by integer decimation,
/// averaging each window so the drop does not alias badly. The recordings are
/// 48 kHz, an exact multiple; anything else fails loudly rather than firing
/// commands off subtly-wrong audio.
fn to_recognizer_rate(recording: &Recording) -> Vec<i16> {
    if recording.sample_rate == SAMPLE_RATE {
        return recording.samples.clone();
    }

    assert!(
        recording.sample_rate.is_multiple_of(SAMPLE_RATE),
        "cannot decimate {} Hz to {} Hz by an integer factor",
        recording.sample_rate,
        SAMPLE_RATE
    );
    let factor = (recording.sample_rate / SAMPLE_RATE) as usize;

    recording
        .samples
        .chunks_exact(factor)
        .map(|window| {
            let sum: i32 = window.iter().copied().map(i32::from).sum();
            (sum / factor as i32) as i16
        })
        .collect()
}

/// Loads a recording fixture as recognizer-ready 16 kHz mono s16.
fn fixture(name: &str) -> Vec<i16> {
    let bytes = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .expect("the recording fixture should exist");

    to_recognizer_rate(&decode_wav(&bytes))
}

// --- The timed pipeline ----------------------------------------------------

/// One timestamped observation, in pipeline order at each observation point.
#[derive(Debug, Clone, PartialEq)]
enum Observed {
    Partial(String),
    Final(String),
    /// A command reaching the command queue — the moment keys would press.
    Fired(String, u64),
    Warning(String),
}

/// The recorded timeline: what happened, and when, measured from the start of
/// the feed.
type Timeline = Arc<Mutex<Vec<(Duration, Observed)>>>;

/// The field profile's shape: the ambiguous-prefix pair itself.
fn field_grammar() -> Automaton {
    let source = r#"
        Autocannon = "auto cannon" { 4 }
        AutocannonSentry = "auto cannon sentry" { 5 }
    "#;
    let grammar = Grammar::parse(source).expect("the test grammar should parse");
    Automaton::compile(&grammar).expect("the test grammar should compile")
}

/// Runs one recording through the real recognizer and the real engine under
/// the shipped defaults (silence 200ms, completion_timeout 750ms, eager on
/// with a 100ms settling window), pacing frames at the capture cadence, and
/// returns the timeline of partials, finals, fires and warnings.
async fn run_recording(name: &str) -> Vec<(Duration, Observed)> {
    run_clipped(name, None).await
}

/// [`run_recording`], optionally keeping only the first `clip` seconds of the
/// recording — how a phrase the speaker never finished reaches the pipeline.
async fn run_clipped(name: &str, clip: Option<f64>) -> Vec<(Duration, Observed)> {
    run_full(
        name,
        clip,
        RecognizerOptions::default().silence,
        crate::config::RecognitionConfig::default().completion_timeout,
    )
    .await
}

async fn run_full(
    name: &str,
    clip: Option<f64>,
    t_end: Duration,
    completion: Duration,
) -> Vec<(Duration, Observed)> {
    let mut samples = fixture(name);
    if let Some(seconds) = clip {
        samples.truncate(((seconds * f64::from(SAMPLE_RATE)) as usize).min(samples.len()));
    }
    let timeline: Timeline = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let observe = {
        let timeline = timeline.clone();
        move |observed: Observed| {
            timeline.lock().unwrap().push((started.elapsed(), observed));
        }
    };

    // The real recognizer, under the shipped endpointer default.
    let (rec_events_tx, mut rec_events_rx) = mpsc::channel(64);
    let (recognizer, audio) = spawn_recognizer(
        &model_path(),
        SAMPLE_RATE,
        &["auto cannon".to_string(), "auto cannon sentry".to_string()],
        RecognizerOptions {
            silence: t_end,
            ..RecognizerOptions::default()
        },
        rec_events_tx,
    )
    .expect("the recognizer should start");

    // A tee between the recognizer and the engine, timestamping each event as
    // the engine is about to see it.
    let (engine_events_tx, engine_events_rx) = mpsc::channel(64);
    let tee = tokio::spawn({
        let observe = observe.clone();
        async move {
            while let Some(event) = rec_events_rx.recv().await {
                match &event {
                    RecognitionEvent::Partial(text) => observe(Observed::Partial(text.clone())),
                    RecognitionEvent::Final(utterance) => {
                        observe(Observed::Final(utterance.text.clone()))
                    }
                    _ => {}
                }
                if engine_events_tx.send(event).await.is_err() {
                    break;
                }
            }
        }
    });

    // The real engine, under the shipped matching defaults, its warnings and
    // its command queue timestamped the same way.
    let options = MatcherOptions {
        eager: true,
        debounce: crate::config::RecognitionConfig::default().debounce,
        warn: {
            let observe = observe.clone();
            Arc::new(move |message| observe(Observed::Warning(message)))
        },
        ..MatcherOptions::with_timeout(completion)
    };
    let (queue_tx, mut queue_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let engine = tokio::spawn(engine_task(
        field_grammar(),
        Pacing {
            duration: Duration::from_millis(30),
            interval: Duration::from_millis(25),
        },
        options,
        engine_events_rx,
        queue_tx,
        cancel.clone(),
    ));
    let drain = tokio::spawn({
        let observe = observe.clone();
        async move {
            while let Some(action) = queue_rx.recv().await {
                observe(Observed::Fired(action.command, action.utterance));
            }
        }
    });

    // Feed the recording at real-time pacing — the eager timers only mean
    // what they meant in the field if the evidence arrives on the field's
    // schedule — then two and a half seconds of silence, enough for the
    // endpointer (200ms) and the completion timeout (500ms) to do whatever
    // they are going to do before the channel closes underneath them.
    let silence = vec![0i16; FRAME];
    let frames = samples
        .chunks(FRAME)
        .map(<[i16]>::to_vec)
        .chain(std::iter::repeat_n(silence, 25));
    let mut cadence = tokio::time::interval(Duration::from_millis(100));
    for frame in frames {
        cadence.tick().await;
        let audio = audio.clone();
        tokio::task::spawn_blocking(move || audio.send(AudioMsg::Frame(frame)))
            .await
            .expect("the feeder should not panic")
            .expect("the recognizer should be listening");
    }

    // Close the pipeline down in dependency order and let every task finish
    // observing before the timeline is read.
    drop(audio);
    tokio::task::spawn_blocking(move || recognizer.join())
        .await
        .expect("the join should not panic")
        .expect("the recognizer should stop cleanly");
    tee.await.expect("the tee should not panic");
    engine
        .await
        .expect("the engine should not panic")
        .expect("the engine should stop cleanly");
    drain.await.expect("the drain should not panic");

    let timeline = timeline.lock().unwrap().clone();
    match clip {
        Some(seconds) => eprintln!("--- {name} (first {seconds}s), silence {t_end:?} ---"),
        None => eprintln!("--- {name}, silence {t_end:?} ---"),
    }
    for (at, observed) in &timeline {
        eprintln!("{:>7.3}s {observed:?}", at.as_secs_f64());
    }
    timeline
}

/// The instant of the first observation matching `pick`, if any.
fn first(timeline: &[(Duration, Observed)], pick: impl Fn(&Observed) -> bool) -> Option<Duration> {
    timeline
        .iter()
        .find(|(_, observed)| pick(observed))
        .map(|(at, _)| *at)
}

/// Every fired command, in order.
fn fired(timeline: &[(Duration, Observed)]) -> Vec<String> {
    timeline
        .iter()
        .filter_map(|(_, observed)| match observed {
            Observed::Fired(name, _) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(feature = "pure_tests", ignore)]
async fn recorded_half_spoken_phrase_must_not_fire_the_whole_command() {
    // The second field report: a phrase abandoned after its first word.
    // "auto cannon.wav" says "auto" from 0.45s to 0.85s and "cannon" from
    // 0.88s to 1.28s, so clipping at 0.85s is a speaker who said "auto" and
    // stopped — exactly `"air burst"` heard as only "air".
    //
    // Nothing was spoken which completes a command, and the recognizer agrees:
    // its `Final` reads "auto". But its *partial* hypothesis completes the
    // grammar phrase from the trailing silence — the phrase-list language
    // model makes "cannon" overwhelmingly likely after "auto" — and holds it
    // perfectly still until the `Final`, so no amount of settling catches it.
    // What keeps the keys up is that the `Final` gets there first: see
    // `recorded_completion_timeout_clears_the_finalization_lag`.
    let timeline = run_clipped("auto cannon.wav", Some(0.85)).await;

    assert_eq!(
        fired(&timeline),
        Vec::<String>::new(),
        "a phrase the speaker never completed must not press keys: {timeline:?}"
    );

    // And the recognizer's own account of it is the one thing that had to
    // arrive in time.
    assert_eq!(
        timeline
            .iter()
            .filter_map(|(_, observed)| match observed {
                Observed::Final(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["auto"],
        "the recognizer should settle on what was actually said: {timeline:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(feature = "pure_tests", ignore)]
async fn recorded_completion_timeout_clears_the_finalization_lag() {
    // The invariant the whole eager path now rests on, measured rather than
    // assumed.
    //
    // An ambiguous resting match fires `completion_timeout` after the partial
    // it rests on. That partial may be a phrase the speaker never finished —
    // the decoder completes grammar phrases out of trailing silence — and the
    // only thing which ever contradicts it is the `Final`. So the wait has to
    // outlast the endpointer, or the keys go down first.
    //
    // Shortening `recognition.silence` does *not* reliably buy that margin:
    // `set_endpointer_delays` drives several rules keyed on decode
    // confidence, and a half-spoken phrase decodes with low confidence by
    // construction. Measured here across the shipped value and a much shorter
    // one, on both a completed phrase and an abandoned one.
    let completion = crate::config::RecognitionConfig::default().completion_timeout;

    for silence in [
        Duration::from_millis(50),
        RecognizerOptions::default().silence,
    ] {
        for (label, clip) in [("abandoned", Some(0.85)), ("completed", None)] {
            let timeline = run_full("auto cannon.wav", clip, silence, completion).await;

            let final_at = first(&timeline, |o| matches!(o, Observed::Final(_)))
                .expect("the utterance should finalize");
            let partial_at = timeline
                .iter()
                .rfind(|(at, o)| matches!(o, Observed::Partial(_)) && *at < final_at)
                .map(|(at, _)| *at)
                .expect("a partial should precede the Final");
            let lag = final_at - partial_at;

            assert!(
                lag < completion,
                "the {label} phrase finalized {lag:?} after its last partial, which the \
                 {completion:?} completion timeout does not clear (silence {silence:?}) — an \
                 abandoned phrase would press keys before the recognizer could take it back: \
                 {timeline:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(feature = "pure_tests", ignore)]
async fn recorded_unambiguous_phrase_fires_before_its_final() {
    // The field question itself: "auto cannon sentry" rests on an accept
    // nothing can extend, so the eager path owes a fire `debounce` after
    // the hypothesis stabilizes — *before* the endpointer's Final — and any
    // ~500ms the user perceives after that is presentation, not matching.
    let timeline = run_recording("auto cannon sentry.wav").await;

    assert_eq!(
        fired(&timeline),
        vec!["AutocannonSentry"],
        "exactly the long command fires: {timeline:?}"
    );

    let fire = first(&timeline, |o| matches!(o, Observed::Fired(..))).unwrap();
    let final_at = first(&timeline, |o| matches!(o, Observed::Final(_)))
        .expect("the utterance should finalize");
    assert!(
        fire < final_at,
        "the unambiguous eager fire must precede the Final ({fire:?} vs {final_at:?}): {timeline:?}"
    );

    let warnings: Vec<_> = timeline
        .iter()
        .filter(|(_, o)| matches!(o, Observed::Warning(_)))
        .collect();
    assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(feature = "pure_tests", ignore)]
async fn recorded_ambiguous_prefix_waits_out_the_completion_timeout() {
    // The control: "auto cannon" rests on the ambiguous accept, so the short
    // command may only fire once the completion timeout elapses. The timeout
    // is armed the moment the partial rests on "auto cannon" — starting it at
    // the partial rather than at finalization is the eager path's latency win
    // here — so the wait is measured from that partial, not from the Final.
    //
    // Under the shipped `completion_timeout` the wait deliberately outlasts
    // the endpointer: the `Final` lands first and *confirms* the hypothesis
    // before the keys go down. That ordering is not incidental — it is the
    // whole reason a half-spoken phrase (see
    // `recorded_half_spoken_phrase_must_not_fire_the_whole_command`) gets
    // retracted instead of pressed — so it is pinned here.
    let timeline = run_recording("auto cannon.wav").await;

    assert_eq!(
        fired(&timeline),
        vec!["Autocannon"],
        "exactly the short command fires: {timeline:?}"
    );

    let completion = crate::config::RecognitionConfig::default().completion_timeout;
    let fire = first(&timeline, |o| matches!(o, Observed::Fired(..))).unwrap();
    let armed = timeline
        .iter()
        .filter(|(_, o)| matches!(o, Observed::Partial(_)))
        .map(|(at, _)| *at)
        .take_while(|at| *at < fire)
        .last()
        .expect("a partial should have armed the timeout");
    let wait = fire - armed;
    assert!(
        wait >= completion,
        "the ambiguous accept must wait out the completion timeout: waited only {wait:?}: {timeline:?}"
    );
    assert!(
        wait < completion + Duration::from_millis(500),
        "the fire should come from the armed timeout, not a stall: waited {wait:?}: {timeline:?}"
    );

    // The endpointer gets there first, and the fire is the confirmed one.
    let final_at = first(&timeline, |o| matches!(o, Observed::Final(_)))
        .expect("the utterance should finalize");
    assert!(
        final_at < fire,
        "the Final must land before the ambiguous fire, so the hypothesis is confirmed before any key goes down ({final_at:?} vs {fire:?}): {timeline:?}"
    );

    let warnings: Vec<_> = timeline
        .iter()
        .filter(|(_, o)| matches!(o, Observed::Warning(_)))
        .collect();
    assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
}
