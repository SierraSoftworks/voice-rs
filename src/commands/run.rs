//! `voice-orders run <profile> [-- <app> <args...>]`: the pipeline assembly and
//! the child-process supervisor. See DESIGN.md §"`run` assembly & child
//! processes" and §"Runtime pipeline".
//!
//! Assembly is deliberately ordered so that the things which fail because of
//! *the user's machine* fail before the things which cost time or make noise:
//! the profile parses, the grammar compiles, the virtual keyboard is created
//! (the single most common first-run failure), and only then is a speech model
//! loaded and a microphone opened.
//!
//! Everything from the recognizer down to the command queue is shared with
//! `voice-orders test`, and lives here in [`Pipeline`]: the two commands differ
//! only in who consumes the command queue (a uinput keyboard or the terminal),
//! whether a virtual keyboard is created at all, and whether a child process is
//! supervised. The pure part of the assembly — profile in, compiled automaton +
//! recognition feed out — is [`build_pipeline_parts`], so it can be tested with
//! no hardware at all, and the supervisor's signal handling is [`supervise`],
//! which takes its triggers as futures for the same reason.
//!
//! **Two faces.** On an interactive terminal `run` renders the same full-screen
//! UI as `test` (`super::ui`); with stdout piped — a Steam launch, a CI job,
//! `| tee` — it stays the line-printed wrapper it has always been, down to the
//! child inheriting our stdio. That split is [`super::ui::ReportMode`], and it
//! is what the child-process semantics below hang off: under a UI the child's
//! output is piped into the log (an inherited child would draw straight over
//! the alternate screen) and raw mode turns Ctrl-C into a keystroke we have to
//! forward as a signal ourselves ([`Shutdown`]).

use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use clap::Args;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use crate::audio;
use crate::config::{Profile, ResolvedSettings, SystemConfig, loader, resolve_model};
use crate::grammar::{Automaton, Feed, feed, user_error};
use crate::hotkey::{self, ListenMode};
use crate::matcher::{CommandAction, MatcherOptions, engine::engine_task};
use crate::output::{Interrupt, PlatformSink, executor};
use crate::recognition::{AudioMsg, RecognitionEvent, vosk};

use super::ui::{EventSink, ReportMode, UiEvent, tui};

/// The sample rate we ask the microphone for and hand to the recognizer.
///
/// Every Vosk model published for English is trained at 16 kHz, and the crate
/// gives us no way to ask a model what it expects, so this is a constant rather
/// than something read from the model directory. `audio` resamples whatever the
/// device actually produces to match.
const RECOGNIZER_SAMPLE_RATE: u32 = 16_000;

/// Capacity of the recognition-event channel (DESIGN.md §"Runtime pipeline").
const EVENTS_CHANNEL_CAPACITY: usize = 64;

/// Capacity of the command queue between the matcher and its consumer.
const COMMAND_QUEUE_CAPACITY: usize = 32;

/// How long a child gets to wind itself up after we forward SIGTERM before we
/// stop waiting and shut down anyway.
const SIGTERM_GRACE: Duration = Duration::from_secs(5);

/// How long we wait for a child to disappear after a kill it cannot refuse.
///
/// Windows only: `TerminateJobObject` is unpostponable, so this is a bound on
/// the kernel reaping the process rather than on the application's cooperation,
/// and it is short for that reason.
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The profile to run: a local path or an https:// URL.
    pub profile: String,

    /// Application to launch; voice-orders exits when it exits.
    /// Steam: `voice-orders run profile.yaml -- %command%`
    #[arg(last = true)]
    pub app: Vec<String>,

    /// The Vosk model to recognize with.
    /// Overrides the profile's `model:` field and $VOSK_MODEL_PATH.
    #[arg(long)]
    pub model: Option<PathBuf>,

    /// Print recognition events (partials/finals) for debugging.
    #[arg(long, hide = true)]
    pub debug_recognition: bool,
}

/// Assembles and runs the pipeline, returning the exit code to leave with.
pub async fn run(args: RunArgs) -> Result<i32, crate::Error> {
    // 1. The profile: loaded from a path or an https:// URL, then parsed and
    //    structurally validated.
    let loaded = loader::load(&args.profile).await?;
    let profile = Profile::parse(&loaded)?;

    // 1b. This machine's own configuration, and the settings the two of them
    //     add up to: which microphone, and which hotkey (if any).
    let system = SystemConfig::load()?;
    let settings = ResolvedSettings::resolve(&profile, &system)?;

    // 2. The grammar: the automaton compiled (which is where duplicate
    //    commands become an error) and the recognition feed built.
    let parts = build_pipeline_parts(&profile, &loaded.source)?;

    // 3. The virtual keyboard, *first*: creating it is what fails when
    //    /dev/uinput is missing or unreadable, and finding that out after
    //    loading a model and opening a microphone would be needlessly slow.
    let sink = PlatformSink::new().await?;

    // 4. The model, the recognizer, the microphone and the tasks.
    let model = resolve_model(args.model.as_deref(), &profile, &system)?;

    // How this session reports itself: the full-screen UI on a terminal we
    // own, the line-printed report everywhere else. A Steam launch has no TTY,
    // so the wrapper contract is untouched by any of this.
    let mode = ReportMode::of(std::io::stdout().is_terminal());
    let (events, ui) = mode.sink();

    let options = PipelineOptions {
        narration: narration_for(mode, args.debug_recognition),
        // The UI's footer shows the listening state live, so the bridge has to
        // report it; plain `run` only logs it, exactly as before.
        announce_listening: mode == ReportMode::Tui,
        events: events.clone(),
    };
    let (mut pipeline, queue) = Pipeline::start(&profile, &settings, model, parts, options)?;

    // 5. The consumer of the command queue: for `run`, the virtual keyboard —
    //    with a reporter in front of it under the UI, so the log says what
    //    fired without the executor having to know a UI exists.
    let cancel = pipeline.cancel();
    let interrupt = pipeline.interrupt();
    let queue = match mode {
        ReportMode::Tui => {
            let (reported_tx, reported_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
            pipeline.watch(
                "command reporter",
                tokio::spawn(narrate_commands(queue, reported_tx, events.clone())),
            );
            reported_rx
        }
        ReportMode::Plain => queue,
    };
    pipeline.watch(
        "output executor",
        tokio::spawn(executor(queue, sink, cancel, interrupt)),
    );

    // 6. The child, if we were given one to wrap. Its stdio is inherited in
    //    plain mode (the wrapper contract) and piped under the UI, which draws
    //    its output into the log instead.
    let child = spawn_child(&args.app, mode)?;
    let program = program_name(&args.app);

    // 7. Supervision: whichever of the child, the keyboard, SIGINT or SIGTERM
    //    arrives first ends the session.
    let (ending, ui_failure) = match ui {
        Some(ui) => {
            let overview =
                tui::Overview::describe(&profile, &settings, &loaded.source, pipeline.summary());
            let overview = match &program {
                Some(program) => overview.wrapping(program, child.as_ref().and_then(Child::id)),
                None => overview,
            };

            run_on_screen(overview, ui, child, program, events).await?
        }
        None => (supervise(child, interrupts(), terminations()?).await, None),
    };
    debug!("{ending}; shutting the pipeline down.");

    // 8. Shutdown, in the order DESIGN.md lays down. The terminal has already
    //    been handed back by now, so anything reported from here is visible.
    //    The pipeline's own failure wins: the UI's is almost always a
    //    consequence of the terminal going away, which is not the interesting
    //    half.
    let failure = pipeline.shutdown().await.or(ui_failure);

    // The Steam wrapper contract is that we exit with the child's code, so a
    // failure on the way out is only allowed to change the exit code when the
    // child did not already have something to say.
    match failure {
        Some(e) if ending.exit_code() == 0 => Err(e),
        Some(e) => {
            warn!("{}", human_errors::pretty(&e));
            Ok(ending.exit_code())
        }
        None => Ok(ending.exit_code()),
    }
}

/// Runs the session behind the terminal UI, returning how it ended and
/// whatever the UI itself failed with.
///
/// The UI owns the screen for as long as it is up, so nothing here may print:
/// the ending is reported by the caller once the terminal has been handed back.
async fn run_on_screen(
    overview: tui::Overview,
    ui_events: mpsc::UnboundedReceiver<UiEvent>,
    mut child: Option<Child>,
    program: Option<String>,
    events: EventSink,
) -> Result<(Ending, Option<crate::Error>), crate::Error> {
    // Installed before the UI takes the terminal: a failure here has to be
    // reportable, and nothing is reportable once the alternate screen is up.
    let terminate = terminations()?;

    let quit = CancellationToken::new();
    let ui = tokio::spawn(tui::run(overview, ui_events, quit.clone()));

    // The child's stdio is piped under the UI, so somebody has to read it: each
    // line becomes a log entry rather than a scribble over the alternate
    // screen.
    let forwarders = match (child.as_mut(), program.as_deref()) {
        (Some(child), Some(program)) => forward_child_output(child, program, &events),
        _ => Vec::new(),
    };

    let has_child = child.is_some();
    let stopped = stopped_by_ui(quit.clone(), has_child);

    let ending = supervise(child, stopped, terminate).await;

    // Worth a line in the log even though the UI is about to come down: a
    // session which ended because the game exited says so.
    if let (Ending::Child(code), Some(program)) = (ending, program) {
        events.send(UiEvent::ChildExited { program, code });
    }

    quit.cancel();
    for forwarder in forwarders {
        forwarder.abort();
    }

    // Awaiting the UI is what restores the terminal, so it must happen before
    // the caller reports anything at all.
    let failure = match ui.await {
        Ok(result) => result.err(),
        Err(e) => Some(human_errors::wrap_system(
            e,
            "The session display stopped unexpectedly.",
            &["Please report this issue on GitHub so that we can investigate."],
        )),
    };

    Ok((ending, failure))
}

/// Waits for whichever comes first: the user asking the UI to stop, or a real
/// SIGINT — and says what the child needs from us as a result.
pub(super) async fn stopped_by_ui(quit: CancellationToken, has_child: bool) -> Shutdown {
    let stop = tokio::select! {
        () = quit.cancelled() => Stop::Keyboard,
        _ = interrupts() => Stop::Signal,
    };

    shutdown_for(stop, has_child)
}

/// How a session under the terminal UI was asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The user pressed `q` or Ctrl-C. Raw mode means both arrive as
    /// *keystrokes*: the terminal is not turning Ctrl-C into a SIGINT for
    /// anybody, ourselves or the child.
    Keyboard,
    /// A signal reached the process the ordinary way.
    Signal,
}

/// What a shutdown owes the wrapped application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shutdown {
    /// Nothing beyond ending the session: either there is no child, or a real
    /// SIGINT has already reached it through the process group.
    Quiet,
    /// The child never saw the interrupt, so we have to send it one: raw mode
    /// delivered the Ctrl-C to us as a key press instead of to the group as a
    /// signal, and a wrapped game which is never told to stop would be left
    /// running with no wrapper.
    ForwardSigint,
}

/// The one decision the raw-mode Ctrl-C hinges on, as a function so it can be
/// tested without signalling the test runner.
fn shutdown_for(stop: Stop, has_child: bool) -> Shutdown {
    match (stop, has_child) {
        (Stop::Keyboard, true) => Shutdown::ForwardSigint,
        // A real SIGINT was delivered to our whole process group, the child
        // included; forwarding it would double the signal.
        (Stop::Signal, _) | (Stop::Keyboard, false) => Shutdown::Quiet,
    }
}

/// How much of what it hears a session says out loud.
///
/// Plain `run` stays silent unless asked: its stdout is the wrapped
/// application's, and a Steam launch must not be narrated at it. Under the UI
/// there is a log to fill, so utterances are reported exactly as `test` reports
/// them — the log *is* the reason the UI exists.
fn narration_for(mode: ReportMode, debug_recognition: bool) -> Narration {
    match (mode, debug_recognition) {
        (_, true) => Narration::Everything,
        (ReportMode::Tui, false) => Narration::Utterances,
        (ReportMode::Plain, false) => Narration::Silent,
    }
}

// --- The shared pipeline -------------------------------------------------

/// How the recognition-event forwarder narrates what passes through it.
///
/// The forwarder sits *in* the event path rather than tapping it, so what it
/// reports is exactly what the matcher sees, in the order it sees it. It
/// reports through an [`EventSink`] rather than tracing: these lines are the
/// user-facing report, they must appear with no telemetry configured, and
/// under `test` they are what the terminal UI draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Narration {
    /// Say nothing; no forwarder is spawned at all.
    Silent,
    /// Print finalized utterances (`voice-orders test`).
    Utterances,
    /// Print every event, partials included (`--debug-recognition`).
    Everything,
}

/// What the two commands vary about the shared pipeline.
pub(super) struct PipelineOptions {
    /// Whether recognized speech is reported to the terminal.
    pub narration: Narration,
    /// Whether listening-state changes are reported to the user (`test`) or
    /// only logged (`run`).
    pub announce_listening: bool,
    /// Where everything the pipeline reports goes: straight to stdout, or to
    /// `test`'s terminal UI.
    pub events: EventSink,
}

/// What the pipeline ended up made of, for the startup summary and for `test`'s
/// header.
pub(super) struct PipelineSummary {
    pub device: String,
    pub model: PathBuf,
    pub commands: usize,
    pub phrases: usize,
    pub mode: Option<ListenMode>,
    pub listening: bool,
}

impl PipelineSummary {
    /// How the listening arrangement reads in a report line.
    pub fn listening_summary(&self) -> String {
        match self.mode {
            Some(mode) => format!("{mode} hotkey"),
            None => "always listening".to_string(),
        }
    }
}

/// A running audio → recognition → matcher pipeline, minus whoever consumes the
/// command queue.
///
/// Shared by `run` and `test` so the wiring and — just as importantly — the
/// shutdown ordering exist exactly once.
pub(super) struct Pipeline {
    cancel: CancellationToken,
    /// Dropping this stops the microphone and releases its audio sender.
    capture: audio::CaptureHandle,
    recognizer: vosk::RecognizerHandle,
    /// Holds the *other* audio sender, so it must finish before the recognizer
    /// can see the channel close.
    bridge: Option<JoinHandle<()>>,
    /// Holds the recognition-event receiver when narration is on.
    forwarder: Option<JoinHandle<()>>,
    /// Fallible tasks to join on the way out, in shutdown order.
    tasks: Vec<(&'static str, JoinHandle<Result<(), crate::Error>>)>,
    /// What the queue's consumer should do when listening stops, from the
    /// profile's `hotkey.interrupt`.
    interrupt: Interrupt,
    summary: PipelineSummary,
}

impl Pipeline {
    /// Loads the model, starts the recognizer and the microphone, and spawns
    /// the matcher, the hotkey watcher and the listening bridge.
    ///
    /// Returns the receiving end of the command queue: the caller decides what
    /// a matched command actually *does*, and registers its task with
    /// [`Pipeline::watch`].
    pub fn start(
        profile: &Profile,
        settings: &ResolvedSettings,
        model: PathBuf,
        parts: (Automaton, Feed),
        options: PipelineOptions,
    ) -> Result<(Self, mpsc::Receiver<CommandAction>), crate::Error> {
        let (automaton, feed) = parts;
        let grammar = feed.phrases;

        let dropped_frames = Arc::new(AtomicU64::new(0));
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (recognizer, audio_tx) = vosk::spawn_recognizer_with_drop_counter(
            &model,
            RECOGNIZER_SAMPLE_RATE,
            &grammar,
            profile.recognition.recognizer_options(),
            events_tx,
            dropped_frames.clone(),
        )?;

        let cancel = CancellationToken::new();
        let (queue_tx, queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);

        // With no hotkey configured we are always listening; with one, the mode
        // decides where we start. The watch channel and the atomic the audio
        // callback reads must agree from the very first frame.
        let mode = settings.hotkey.as_ref().map(|hotkey| hotkey.mode);
        let listening = mode.is_none_or(hotkey::initial_listening);
        let (listening_tx, listening_rx) = watch::channel(listening);
        let listening_flag = Arc::new(AtomicBool::new(listening));

        let interrupt = interrupt_for(settings, &listening_tx);

        let capture = audio::start_capture(
            &settings.audio_device,
            RECOGNIZER_SAMPLE_RATE,
            audio_tx.clone(),
            listening_flag.clone(),
            dropped_frames,
        )?;

        // The recognizer thread stops when the last audio sender is dropped, so
        // ours goes now: from here only the capture callback and (when there is
        // a hotkey) the listening bridge hold one.
        let bridge_audio = audio_tx.clone();
        drop(audio_tx);

        let (matcher_events, forwarder) = match options.narration {
            Narration::Silent => (events_rx, None),
            narration => {
                let (narrated_tx, narrated_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
                let task = tokio::spawn(narrate_recognition(
                    events_rx,
                    narrated_tx,
                    narration,
                    options.events.clone(),
                ));
                (narrated_rx, Some(task))
            }
        };

        // The matcher's warnings (eager mismatches, suppressed utterances) go
        // through the same sink as everything else the session reports: a
        // yellow `warning:` entry under the UI, a plain `warning:` line
        // otherwise.
        let matcher_options = MatcherOptions::from_profile(profile, {
            let events = options.events.clone();
            Arc::new(move |message| events.send(UiEvent::Warning(message)))
        });

        let mut tasks = vec![(
            "matcher",
            tokio::spawn(engine_task(
                automaton,
                profile.defaults,
                matcher_options,
                matcher_events,
                queue_tx,
                cancel.clone(),
            )),
        )];

        // The hotkey, when there is one: the device, the task which watches it,
        // and the bridge which turns its state changes into a mute of the audio
        // path.
        let bridge = match (&settings.hotkey, mode) {
            (Some(config), Some(mode)) => {
                // Resolving the device is part of starting: an unresolvable
                // one fails here, before any audio machinery spins up.
                let watcher = hotkey::watch(
                    &config.device,
                    config.key.code(),
                    mode,
                    listening_tx,
                    cancel.clone(),
                )?;
                tasks.push(("hotkey watcher", tokio::spawn(watcher)));

                Some(tokio::spawn(listening_bridge(
                    listening_rx,
                    listening_flag,
                    bridge_audio,
                    cancel.clone(),
                    options.announce_listening.then_some(options.events),
                )))
            }
            // Always listening: the watch never changes, so there is nothing to
            // bridge — and nothing may keep a spare audio sender alive.
            _ => {
                drop((listening_tx, listening_rx, bridge_audio));
                None
            }
        };

        let summary = PipelineSummary {
            device: capture.device_name().to_string(),
            model,
            commands: profile.grammar.published().count(),
            phrases: grammar.len(),
            mode,
            listening,
        };

        info!(
            profile = profile.display_name(),
            device = summary.device.as_str(),
            model = %summary.model.display(),
            commands = summary.commands,
            phrases = summary.phrases,
            listening = %summary.listening_summary(),
            "Listening for {} command(s) ({} phrase(s)) from '{}' as profile '{}', {}.",
            summary.commands,
            summary.phrases,
            summary.device,
            profile.display_name(),
            summary.listening_summary(),
        );

        Ok((
            Self {
                cancel,
                capture,
                recognizer,
                bridge,
                forwarder,
                tasks,
                interrupt,
                summary,
            },
            queue_rx,
        ))
    }

    /// The token every task in the pipeline shuts down on.
    pub fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// What the command-queue consumer should do when listening stops.
    ///
    /// [`Interrupt::Never`] unless the profile has a hotkey which sets
    /// `interrupt: true`, in which case this is a subscription to the same
    /// listening state the bridge mutes the microphone from.
    pub fn interrupt(&self) -> Interrupt {
        self.interrupt.clone()
    }

    /// What the pipeline ended up made of.
    pub fn summary(&self) -> &PipelineSummary {
        &self.summary
    }

    /// Registers the caller's command-queue consumer so it is joined with
    /// everything else on the way out.
    pub fn watch(&mut self, what: &'static str, task: JoinHandle<Result<(), crate::Error>>) {
        self.tasks.push((what, task));
    }

    /// Winds the pipeline down in the order DESIGN.md lays down: cancel, stop
    /// the microphone, close the audio channel, join the recognizer, then let
    /// the matcher and the queue consumer drain (the uinput executor releases
    /// anything it is still holding down on its way out).
    ///
    /// Returns the first failure anything reported, if any.
    pub async fn shutdown(self) -> Option<crate::Error> {
        self.cancel.cancel();
        drop(self.capture);

        if let Some(bridge) = self.bridge {
            // Awaiting it also drops its audio sender, which is the last one:
            // the recognizer's channel is closed from here.
            let _ = bridge.await;
        }

        let joined = tokio::task::spawn_blocking(move || self.recognizer.join()).await;
        let mut failure = match joined {
            Ok(result) => result.err(),
            Err(e) => Some(human_errors::wrap_system(
                e,
                "We lost track of the speech recognition thread while shutting down.",
                &["Please report this issue on GitHub so that we can investigate."],
            )),
        };

        // The recognizer has dropped its event sender by now, so the forwarder
        // has run out of events to forward.
        if let Some(forwarder) = self.forwarder {
            let _ = forwarder.await;
        }

        for (what, task) in self.tasks {
            if let Some(e) = join_task(what, task).await {
                failure = failure.or(Some(e));
            }
        }

        failure
    }
}

/// What the command-queue consumer should do when listening stops.
///
/// `hotkey.interrupt: true` gives it its own view of the listening state, so it
/// can drop what it is doing the moment the hotkey says to stop — the output
/// half of the mute which [`listening_bridge`] performs on the input half.
/// Without a hotkey the listening state never changes at all, so there is
/// nothing to subscribe to and nothing which could ever interrupt.
fn interrupt_for(settings: &ResolvedSettings, listening: &watch::Sender<bool>) -> Interrupt {
    match &settings.hotkey {
        Some(hotkey) if hotkey.interrupt => Interrupt::when_listening_stops(listening.subscribe()),
        _ => Interrupt::Never,
    }
}

/// Compiles a profile into everything the runtime pipeline needs to match
/// speech: the grammar's automaton, and the recognition feed handed to the
/// recognizer.
///
/// This is the whole of the assembly which is a pure function of the profile,
/// which is what makes it testable without a model, a microphone or
/// `/dev/uinput`. The grammar itself parsed at profile load; compiling the
/// automaton happens here so its failures — two commands accepting the same
/// words with different keys, a grammar past the state cap — can name the
/// profile's `source`.
///
/// The feed's phrases are lowercased, space-joined and globally deduped.
/// [`vosk::UNKNOWN_PHRASE`] is deliberately *not* added here — the recognizer
/// appends it itself, and adding it twice would put a duplicate entry into the
/// compiled grammar.
pub(super) fn build_pipeline_parts(
    profile: &Profile,
    source: &str,
) -> Result<(Automaton, Feed), crate::Error> {
    let automaton = Automaton::compile(&profile.grammar).map_err(|diagnostics| {
        human_errors::wrap_user(
            user_error(&diagnostics, profile.grammar.source()),
            format!(
                "We could not compile the grammar of the profile at '{source}' into something we can listen for."
            ),
            &[
                "Each report above points at the exact place in the grammar it describes.",
                "Run `voice-orders validate` on the profile to see every problem — and every warning — in one pass.",
            ],
        )
    })?;

    Ok((automaton, feed(&profile.grammar)))
}

/// Keeps the audio callback's `AtomicBool` and the recognizer in step with the
/// hotkey's listening state.
///
/// The order matters on the way *down*: the mirror is stored first, so the cpal
/// callback stops pushing frames before the [`AudioMsg::Reset`] is queued, and
/// the reset therefore lands behind the last frame of the utterance being
/// abandoned rather than in front of it. The recognizer emits the matcher's
/// `Muted` event itself when it processes that reset, so nothing synthetic is
/// injected into the event channel here — exactly one `Muted` per mute.
///
/// `announce` is `Some` when the listening state is part of the user-facing
/// report (`test`, whose UI draws it in the footer); `run` only logs it.
async fn listening_bridge(
    mut listening: watch::Receiver<bool>,
    mirror: Arc<AtomicBool>,
    audio: SyncSender<AudioMsg>,
    cancel: CancellationToken,
    announce: Option<EventSink>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("Shutdown requested, stopping the listening bridge.");
                return;
            }
            changed = listening.changed() => {
                if changed.is_err() {
                    debug!("The hotkey watcher has gone, stopping the listening bridge.");
                    return;
                }

                let now = *listening.borrow_and_update();
                mirror.store(now, Ordering::Relaxed);

                debug!(listening = now, "Listening is now {}.", if now { "on" } else { "off" });
                if let Some(events) = &announce {
                    events.send(UiEvent::Listening(now));
                }

                // Muting resets the recognizer (and tells the matcher via
                // Muted); unmuting clears anything that may have raced in
                // around the transition, so listening always starts from a
                // clean decoder. A blocking send on a std channel, off the
                // runtime: neither message may be dropped, or the matcher
                // could carry a half-spoken phrase across the mute boundary.
                let msg = if now { AudioMsg::Clear } else { AudioMsg::Reset };
                let audio = audio.clone();
                let sent = tokio::task::spawn_blocking(move || audio.send(msg)).await;
                if !matches!(sent, Ok(Ok(()))) {
                    debug!("The recognizer has gone, stopping the listening bridge.");
                    return;
                }
            }
        }
    }
}

/// Reports recognition events on their way to the matcher.
///
/// Sitting in the path rather than tapping it means the reported order is
/// exactly the matcher's order, so a `heard:` line with no `matched:` line
/// after it really does mean "recognized, but matched nothing".
async fn narrate_recognition(
    mut events: mpsc::Receiver<RecognitionEvent>,
    matcher: mpsc::Sender<RecognitionEvent>,
    narration: Narration,
    sink: EventSink,
) {
    // Utterance slots, counted exactly as the matcher counts them: every
    // Final *and* every Muted closes one. This is the narrator's half of the
    // correlation contract — the sequence stamped on a `heard:` entry here is
    // the sequence the matcher stamps on the commands that utterance fired,
    // which is how the terminal UI reunites them however far apart they land
    // (an eager fire precedes its own Final; a completion-timeout fire trails
    // the next utterance).
    // The terminal UI keeps a live entry per utterance, opened by its first
    // partial and settled (or abandoned) with its slot — so a channel sink
    // gets partials and mutes regardless of narration level. Plain output
    // stays exactly what it always printed: partials and mutes only under
    // `--debug-recognition`.
    let live = matches!(sink, EventSink::Channel(_));
    let mut closed: u64 = 0;
    while let Some(event) = events.recv().await {
        if matches!(event, RecognitionEvent::Final(_) | RecognitionEvent::Muted) {
            closed += 1;
        }
        if let Some(reported) = narration_event(&event, narration, closed, live) {
            sink.send(reported);
        }

        if matcher.send(event).await.is_err() {
            break;
        }
    }
}

/// The report one recognition event deserves, or [`None`] when it is noise the
/// user has not asked to see. `closed` is how many utterance slots the stream
/// has closed *including this event*: a `Final` or a mute closed slot
/// `closed` itself, while a partial belongs to the still-open slot after it.
/// `live` says the sink renders live entries (the terminal UI), which need
/// every partial and mute whatever the narration level.
fn narration_event(
    event: &RecognitionEvent,
    narration: Narration,
    closed: u64,
    live: bool,
) -> Option<UiEvent> {
    match (event, narration) {
        (_, Narration::Silent) => None,
        // A finalized utterance is the whole point: it is what the matcher gets
        // to work with, so it is reported whenever anything is reported at all.
        (RecognitionEvent::Final(utterance), _) => Some(UiEvent::Heard {
            text: utterance.text.clone(),
            seq: closed,
        }),
        // A recognizer which cannot decode is reported as loudly as an
        // utterance: without it, a session where nothing works looks exactly
        // like one where nobody spoke. The recognizer thread has already
        // coalesced these down to one per run of failures.
        (RecognitionEvent::Failed, _) => Some(UiEvent::Warning(
            "the speech recognizer could not decode the audio".to_string(),
        )),
        // Partials and mutes drive the UI's live entries; in plain mode they
        // are noise unless they were asked for.
        (RecognitionEvent::Partial(text), narration) => {
            (live || narration == Narration::Everything).then(|| UiEvent::Hearing {
                text: text.clone(),
                seq: closed + 1,
            })
        }
        (RecognitionEvent::Muted, narration) => {
            (live || narration == Narration::Everything).then_some(UiEvent::Muted { seq: closed })
        }
    }
}

/// Reports every matched command on its way to the virtual keyboard.
///
/// The executor knows nothing about any of this, and should not: it is the one
/// part of the pipeline which must never be slowed down or complicated by
/// reporting. So under the terminal UI a forwarder sits in front of it, with
/// the same shape (and the same guarantee) as [`narrate_recognition`] — what
/// the log says fired is exactly what the executor was handed, in order.
///
/// The wording is deliberately the same one `test` uses: `test` reports the
/// plan it *would* have played and `run` the one it *is* playing, and
/// `"deploy the autocannon" → Autocannon (leftctrl+4)` is true of both.
async fn narrate_commands(
    mut queue: mpsc::Receiver<CommandAction>,
    executor: mpsc::Sender<CommandAction>,
    events: EventSink,
) -> Result<(), crate::Error> {
    while let Some(action) = queue.recv().await {
        events.send(UiEvent::Matched {
            name: action.command.clone(),
            plan: super::ui::render_plan(&action.output),
            utterance: Some(action.utterance),
        });

        if executor.send(action).await.is_err() {
            debug!("The output executor has gone, stopping the command reporter.");
            break;
        }
    }

    Ok(())
}

// --- Signals and the child process ---------------------------------------

/// Resolves when the user interrupts us (Ctrl+C).
///
/// If the handler cannot be installed we say so and then wait forever, so that
/// a machine which will not give us SIGINT still leaves SIGTERM working rather
/// than shutting the pipeline down the instant it starts.
///
/// A signal delivered this way reached the child too — it shares our process
/// group — so there is nothing left to forward: [`Shutdown::Quiet`].
pub(super) async fn interrupts() -> Shutdown {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!("We could not watch for Ctrl+C ({e}); use SIGTERM to stop voice-orders.");
        std::future::pending::<()>().await;
    }

    Shutdown::Quiet
}

/// A future which resolves when we are asked to terminate (SIGTERM — which is
/// how Steam stops a game).
#[cfg(target_os = "linux")]
pub(super) fn terminations() -> Result<impl Future<Output = ()>, crate::Error> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| signal_handler_error(e, "SIGTERM"))?;

    Ok(async move {
        sigterm.recv().await;
    })
}

/// A future which resolves when Windows asks us to terminate.
///
/// There is no SIGTERM to wait for: the two console control events which mean
/// "your process is going away" are `CTRL_CLOSE_EVENT` (the console window was
/// closed) and `CTRL_SHUTDOWN_EVENT` (the system is shutting down), and either
/// one is the same intent, so they are raced. `CTRL_BREAK_EVENT` is
/// deliberately not among them — it is a user gesture like Ctrl-C, which
/// [`interrupts`] already covers.
///
/// Both of these arrive on a **deadline**: Windows gives a console application
/// roughly five seconds to handle `CTRL_CLOSE_EVENT` (and no more than that for
/// a shutdown) before it kills the process where it stands. That is less than
/// [`SIGTERM_GRACE`] plus a tidy pipeline shutdown, so the close-window path can
/// end with us being killed part-way through — which is exactly the case
/// [`contain_child`]'s job object exists to cover: our handles close as we die,
/// and the wrapped application goes with us.
#[cfg(not(target_os = "linux"))]
pub(super) fn terminations() -> Result<impl Future<Output = ()>, crate::Error> {
    let mut close =
        tokio::signal::windows::ctrl_close().map_err(|e| signal_handler_error(e, "CTRL_CLOSE"))?;
    let mut shutdown = tokio::signal::windows::ctrl_shutdown()
        .map_err(|e| signal_handler_error(e, "CTRL_SHUTDOWN"))?;

    Ok(async move {
        tokio::select! {
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
        }
    })
}

/// The failure to install a shutdown handler, which is never the user's fault.
fn signal_handler_error(e: std::io::Error, signal: &str) -> crate::Error {
    human_errors::wrap_system(
        e,
        format!("We could not install a handler for the {signal} signal."),
        &["Please report this issue on GitHub so that we can investigate."],
    )
}

/// Starts the wrapped application, if we were given one.
///
/// In plain mode stdio is **inherited**, so the child's output is the
/// terminal's (or Steam's) exactly as though voice-orders were not in the way
/// at all — that is the wrapper contract, and nothing about a TTY-only UI may
/// change it. Under the terminal UI it is **piped** instead: an inherited child
/// would write straight over the alternate screen, so its output is read and
/// logged ([`forward_child_output`]).
///
/// The two platform hooks around the spawn — [`group_child`] before it and
/// [`contain_child`] after it — do nothing at all on Linux, where a child
/// already inherits our process group and a signal already reaches it.
fn spawn_child(app: &[String], mode: ReportMode) -> Result<Option<Child>, crate::Error> {
    let Some((executable, arguments)) = app.split_first() else {
        return Ok(None);
    };

    debug!(executable, "Starting the wrapped application.");

    let mut command = tokio::process::Command::new(executable);
    command.args(arguments);
    if mode == ReportMode::Tui {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    group_child(&mut command);

    let child = command
        .spawn()
        .map_err(|e| {
            human_errors::wrap_user(
                e,
                format!("We could not start '{executable}'."),
                &[
                    "Check that the program exists, is executable, and is spelled correctly — everything after the '--' is passed to it untouched.",
                    "Steam launch options should read `voice-orders run profile.yaml -- %command%`, with the '--' before %command%.",
                ],
            )
        })?;

    contain_child(&child);

    Ok(Some(child))
}

/// Nothing to do: a child we spawn is already in our process group, which is
/// what makes a Ctrl-C at the terminal reach it without us lifting a finger.
#[cfg(target_os = "linux")]
fn group_child(_command: &mut tokio::process::Command) {}

/// Starts the child in a process group of its own.
///
/// This is what makes a graceful stop possible at all on Windows:
/// `GenerateConsoleCtrlEvent` addresses a process *group*, and the only group
/// it can address other than "everybody on this console" is one created by
/// `CREATE_NEW_PROCESS_GROUP`, whose identifier is the child's own process id.
///
/// It costs the child its inherited Ctrl-C: a process started this way begins
/// with Ctrl-C handling disabled, which is precisely why [`send_signal`] sends
/// `CTRL_BREAK_EVENT` (never `CTRL_C_EVENT`, which is silently discarded for a
/// non-zero group) and why the job object below exists.
///
/// `tokio::process::Command` has its own inherent `creation_flags`, so this
/// needs no import of `std::os::windows::process::CommandExt`.
#[cfg(not(target_os = "linux"))]
fn group_child(command: &mut tokio::process::Command) {
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
}

/// Nothing to contain: on Linux the process group is the containment, and a
/// child which outlives us is the user's business.
#[cfg(target_os = "linux")]
fn contain_child(_child: &Child) {}

/// Puts the child — and everything it goes on to start — into a job object we
/// hold the only handle to.
///
/// **This is the wrapper contract's backstop on Windows, and it is the one
/// guarantee which survives everything.** The job is created with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which means the kernel terminates
/// every process still in it the moment our last handle to it closes — and our
/// handles close when this process exits, whether that is a clean shutdown, a
/// panic, or somebody `TerminateProcess`-ing us from Task Manager. Steam's
/// "Stop" button is an arbitrary kill of exactly that kind, so without this a
/// stopped game would keep running with nothing left to supervise it.
///
/// A failure to assign is a warning rather than an error: a child which was
/// launched into a job of its own (some launchers and anti-cheat services do
/// this) cannot be adopted into ours, and refusing to run the session over it
/// would be worse than running it without the backstop.
#[cfg(not(target_os = "linux"))]
fn contain_child(child: &Child) {
    job::contain(child);
}

/// What to call the wrapped application in the UI: the executable's file name,
/// not the whole path a Steam launch command runs to hundreds of characters.
fn program_name(app: &[String]) -> Option<String> {
    let executable = app.first()?;

    Some(
        Path::new(executable)
            .file_name()
            .map_or_else(|| executable.clone(), |name| name.to_string_lossy().into()),
    )
}

/// Reads a piped child's stdout and stderr, turning each line into a log entry.
///
/// Both streams are read concurrently and reported identically: which of the
/// two a game wrote to says nothing useful, and interleaving them is what makes
/// the log read like the terminal the child thinks it has. The tasks end on
/// their own when the child closes its pipes.
fn forward_child_output(
    child: &mut Child,
    program: &str,
    events: &EventSink,
) -> Vec<JoinHandle<()>> {
    let mut forwarders = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        forwarders.push(tokio::spawn(forward_lines(
            stdout,
            program.to_string(),
            events.clone(),
        )));
    }
    if let Some(stderr) = child.stderr.take() {
        forwarders.push(tokio::spawn(forward_lines(
            stderr,
            program.to_string(),
            events.clone(),
        )));
    }

    forwarders
}

/// Reports every line of one of the child's streams, until it closes.
///
/// Generic over the reader so the forwarding can be tested against bytes
/// instead of a process.
async fn forward_lines<R>(reader: R, program: String, events: EventSink)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => events.send(UiEvent::Child {
                program: program.clone(),
                line,
            }),
            Ok(None) => {
                debug!("The application closed one of its output streams.");
                return;
            }
            Err(e) => {
                // Losing the child's output is not worth ending the session
                // over: the session is about the microphone, not the pipe.
                debug!("We stopped reading the application's output ({e}).");
                return;
            }
        }
    }
}

/// Why the session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    /// The wrapped application exited with this code, which becomes ours.
    Child(i32),
    /// The session was interrupted: a real SIGINT (which the kernel had already
    /// delivered to the child, since it shares our process group) or a Ctrl-C
    /// keystroke under the terminal UI, which we forward ourselves.
    Interrupted,
    /// SIGTERM: forwarded to the child, which either exited within the grace
    /// period or did not.
    Terminated { child_exited: bool },
}

impl Ending {
    /// The exit code to leave with. Only the child's own code is propagated: a
    /// signalled shutdown is a *successful* one from our point of view.
    pub fn exit_code(self) -> i32 {
        match self {
            Ending::Child(code) => code,
            Ending::Interrupted | Ending::Terminated { .. } => 0,
        }
    }
}

impl std::fmt::Display for Ending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ending::Child(code) => write!(f, "The application exited with code {code}"),
            Ending::Interrupted => f.write_str("Interrupted"),
            Ending::Terminated { child_exited: true } => {
                f.write_str("Terminated; the application stopped")
            }
            Ending::Terminated {
                child_exited: false,
            } => f.write_str("Terminated; the application did not stop in time"),
        }
    }
}

/// Waits for whichever comes first: the wrapped application exiting, an
/// interrupt, or a termination request.
///
/// The two signals arrive as futures rather than as `tokio::signal` handles so
/// that the semantics below can be tested without signalling the test runner;
/// the interrupt future says *what kind* of interrupt it was ([`Shutdown`]),
/// because a Ctrl-C read as a keystroke under the terminal UI is one the child
/// has not been told about. Cancellation is the caller's job — every ending
/// cancels, so doing it here would only duplicate one line.
pub(super) async fn supervise(
    mut child: Option<Child>,
    interrupt: impl Future<Output = Shutdown>,
    terminate: impl Future<Output = ()>,
) -> Ending {
    // Bound to its own `let` so the borrow `wait_for` takes is released before
    // the arms below need the child back.
    let outcome = tokio::select! {
        status = wait_for(child.as_mut()) => Outcome::Exited(status),
        shutdown = interrupt => Outcome::Interrupt(shutdown),
        _ = terminate => Outcome::Terminate,
    };

    match outcome {
        Outcome::Exited(Ok(status)) => Ending::Child(status.code().unwrap_or(1)),
        Outcome::Exited(Err(e)) => {
            // We can no longer tell how the application finished, so we report
            // a failure rather than a clean exit.
            warn!("We lost track of the application we started ({e}).");
            Ending::Child(1)
        }
        Outcome::Interrupt(Shutdown::Quiet) => Ending::Interrupted,
        Outcome::Interrupt(Shutdown::ForwardSigint) => {
            // The graceful path a real Ctrl-C would have taken, performed by
            // hand: the child is told to stop and given the same grace period
            // a termination gets, rather than being abandoned to a wrapper
            // which has already exited.
            if let Some(child) = child.as_mut() {
                forward_signal(child, Signal::Interrupt).await;
            }
            Ending::Interrupted
        }
        Outcome::Terminate => {
            let child_exited = match child.as_mut() {
                Some(child) => forward_signal(child, Signal::Terminate).await,
                None => true,
            };
            Ending::Terminated { child_exited }
        }
    }
}

/// The raw result of the supervision `select!`, kept separate from [`Ending`]
/// so the child can be borrowed again once the select's futures are dropped.
enum Outcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    Interrupt(Shutdown),
    Terminate,
}

/// Waits for a child to exit, or forever when there is no child to wait for.
async fn wait_for(child: Option<&mut Child>) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

/// Which of the two "please stop" signals we are passing on to the child.
///
/// Named rather than numeric because the two platforms number them
/// differently — and Windows does not number them at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    /// The SIGINT a real Ctrl-C would have delivered.
    Interrupt,
    /// The SIGTERM a shutdown (Steam stopping the game) delivers.
    Terminate,
}

impl Signal {
    /// What this signal means, for the message a failure to send it produces.
    fn what(self) -> &'static str {
        match self {
            Signal::Interrupt => "interrupt",
            Signal::Terminate => "shutdown",
        }
    }
}

/// Delivers a signal to a child process.
#[cfg(target_os = "linux")]
fn send_signal(pid: u32, signal: Signal) {
    let number = match signal {
        Signal::Interrupt => libc::SIGINT,
        Signal::Terminate => libc::SIGTERM,
    };

    debug!(
        pid,
        signal = number,
        "Forwarding a signal to the application."
    );
    // SAFETY: `kill` is safe to call with any pid, and this one belongs to a
    // child we started and have not yet reaped, so it cannot have been recycled
    // onto an unrelated process.
    if unsafe { libc::kill(pid as libc::pid_t, number) } != 0 {
        let error = std::io::Error::last_os_error();
        warn!(
            "We could not pass the {} signal on to the application ({error}).",
            signal.what()
        );
    }
}

/// Asks a child to stop, as far as Windows lets us ask anything.
///
/// Windows has no per-process signals. The nearest thing is a console control
/// event, and it comes with two honest caveats:
///
/// - `CTRL_BREAK_EVENT` is the only event which can be aimed at one process
///   group. `CTRL_C_EVENT` cannot: for any non-zero group id it *succeeds* and
///   delivers nothing at all, which is the worst possible failure mode, so it
///   is never sent here.
/// - It only reaches a **console** child sharing our console — one started by
///   [`group_child`] with `CREATE_NEW_PROCESS_GROUP`. A GUI application (which
///   is to say: a game) has no console control handler and receives nothing.
///
/// So this is a courtesy for the console case and a no-op everywhere else,
/// which is why the plan behind [`forward_signal`] always ends in a kill on
/// this platform rather than trusting the ask.
#[cfg(not(target_os = "linux"))]
fn send_signal(pid: u32, signal: Signal) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

    debug!(
        pid,
        "Sending CTRL_BREAK to the application's process group ({}).",
        signal.what()
    );

    // SAFETY: no memory is involved — the call takes two integers, and the
    // group id is the child's own pid, which `CREATE_NEW_PROCESS_GROUP` made
    // the identifier of a group containing exactly it and its descendants.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        let error = std::io::Error::last_os_error();
        // Not a warning: a windowed application has no console control handler
        // and this failing says nothing the user can act on. The kill which
        // follows is what actually stops it.
        debug!(
            "The application did not accept the {} console event ({error}).",
            signal.what()
        );
    }
}

/// Stops the child by force, along with everything it started.
///
/// Never reached on Linux: [`CONTAINMENT`] is [`Containment::Signals`] here, so
/// no plan this platform builds contains a [`StopStep::Kill`]. It exists so the
/// plan executor below can stay one shared function.
#[cfg(target_os = "linux")]
fn kill_contained() -> bool {
    false
}

/// Terminates the job object the child was placed in when it was spawned.
///
/// `TerminateJobObject` is the one stop a process cannot postpone, argue with,
/// or catch — and because it acts on the *job*, it takes the whole tree the
/// application built (launchers, shader compilers, an anti-cheat service) rather
/// than leaving orphans behind a stopped game.
#[cfg(not(target_os = "linux"))]
fn kill_contained() -> bool {
    job::terminate()
}

/// What this platform can do about a wrapped application which will not stop
/// when it is asked to.
///
/// The two are different in kind rather than in degree, which is why this is an
/// enum and not a flag. On Linux the signal *is* the mechanism: a process which
/// ignores SIGTERM has made a decision, and a wrapper which escalated to
/// SIGKILL would be overruling the user's own program. On Windows the ask
/// reaches console children only (see [`send_signal`]), so the job object is
/// not an escalation at all — it is the only thing that ever guaranteed the
/// application stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Containment {
    /// Signals, and nothing beyond them.
    Signals,
    /// A job object holding the child and everything it started.
    JobObject,
}

/// This platform's containment.
///
/// `cfg!` rather than `#[cfg]` deliberately: both arms are compiled everywhere,
/// so the plan each of them produces can be asserted on the platform we develop
/// on rather than only on the platform it ships to.
const CONTAINMENT: Containment = if cfg!(windows) {
    Containment::JobObject
} else {
    Containment::Signals
};

/// One step of stopping a wrapped application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopStep {
    /// Ask it to stop.
    Ask(Signal),
    /// Give it this long to go on its own.
    Wait(Duration),
    /// Stop it ourselves, and everything it started with it.
    Kill,
}

/// The ordered steps for stopping a wrapped application.
///
/// The choreography is the interesting half of child shutdown and the half
/// which differs per platform, so it is derived here — from a [`Signal`] and a
/// [`Containment`], with no process anywhere near it — and merely *performed*
/// by [`forward_signal`]. Both platforms' plans are therefore asserted by unit
/// tests on either platform.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildStopPlan(Vec<StopStep>);

impl ChildStopPlan {
    /// How this platform stops a child it is forwarding `signal` to.
    fn new(signal: Signal, containment: Containment) -> Self {
        let mut steps = vec![StopStep::Ask(signal), StopStep::Wait(SIGTERM_GRACE)];

        if containment == Containment::JobObject {
            steps.push(StopStep::Kill);
        }

        Self(steps)
    }

    /// The steps, in the order they are taken.
    fn steps(&self) -> &[StopStep] {
        &self.0
    }

    /// Whether the plan ends by stopping the application itself, which is what
    /// the "it did not exit in time" message has to say honestly.
    fn kills(&self) -> bool {
        self.0.contains(&StopStep::Kill)
    }
}

/// Forwards a signal to the child and gives it [`SIGTERM_GRACE`] to wind down.
///
/// Both shutdown paths come through here: SIGTERM (Steam stopping the game) and
/// the SIGINT the terminal never sent for us, because raw mode delivered the
/// user's Ctrl-C to this process as a key press instead.
///
/// Returns whether it actually stopped in time; we proceed with shutdown either
/// way, because an application which refuses to exit must not keep the wrapper
/// (and therefore Steam) hanging indefinitely.
async fn forward_signal(child: &mut Child, signal: Signal) -> bool {
    let Some(pid) = child.id() else {
        // Already reaped: there is nothing left to signal.
        return true;
    };

    let plan = ChildStopPlan::new(signal, CONTAINMENT);
    let mut exited = false;

    for step in plan.steps() {
        match *step {
            StopStep::Ask(signal) => send_signal(pid, signal),
            StopStep::Wait(grace) => {
                exited = wait_out(child, grace, plan.kills()).await;
                if exited {
                    break;
                }
            }
            StopStep::Kill => {
                if !kill_contained() {
                    warn!("We could not stop the application ourselves; shutting down anyway.");
                    break;
                }

                exited = matches!(
                    tokio::time::timeout(KILL_GRACE, child.wait()).await,
                    Ok(Ok(_))
                );
            }
        }
    }

    exited
}

/// Waits `grace` for a child to exit on its own, saying so when it does not.
///
/// `escalating` says whether a [`StopStep::Kill`] follows, which is the only
/// thing the message may not lie about: "shutting down anyway" is the truth on
/// a platform which leaves the application alone, and would be a lie on one
/// which is about to terminate it.
async fn wait_out(child: &mut Child, grace: Duration, escalating: bool) -> bool {
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            warn!("We lost track of the application while it was shutting down ({e}).");
            false
        }
        Err(_) if escalating => {
            warn!(
                "The application has not exited {}s after we asked it to; stopping it and everything it started.",
                grace.as_secs()
            );
            false
        }
        Err(_) => {
            warn!(
                "The application has not exited {}s after we asked it to; shutting down anyway.",
                grace.as_secs()
            );
            false
        }
    }
}

/// The job object which holds the wrapped application, and the two syscalls
/// which put it there and tear it down.
///
/// The handle lives in a `static` rather than in a value threaded through
/// [`supervise`]: the supervisor is shared, platform-neutral code and stays
/// that way, and the guarantee we are buying is a *process-lifetime* one —
/// the job must outlive every path out of this program, including the ones
/// which never run another line of our code. `Job`'s `Drop` closes the handle
/// (and so kills whatever is still in the job) if the guard is ever replaced or
/// taken; on the ordinary path the handle is closed by the kernel as the
/// process goes away, which has exactly the same effect and is the reason this
/// is a backstop rather than a courtesy.
#[cfg(not(target_os = "linux"))]
mod job {
    use std::sync::Mutex;

    use tokio::process::Child;
    use tracing_batteries::prelude::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// The exit code processes killed with the job report. It is never
    /// propagated — a terminated child ends the session as
    /// [`super::Ending::Terminated`], not as [`super::Ending::Child`].
    const KILLED_EXIT_CODE: u32 = 1;

    /// An owned job-object handle, closed on drop.
    struct Job(HANDLE);

    // SAFETY: a `HANDLE` is a kernel object identifier rather than a pointer
    // into our address space; every job API is thread-safe and none of them
    // cares which thread holds it.
    unsafe impl Send for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from `CreateJobObjectW`, is closed
            // exactly once (here), and is not used afterwards.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The job the wrapped application is in, for as long as we live.
    static JOB: Mutex<Option<Job>> = Mutex::new(None);

    /// Creates the job, sets kill-on-close, and adopts the child into it.
    pub(super) fn contain(child: &Child) {
        let Some(handle) = child.raw_handle() else {
            // Already exited: there is nothing to contain.
            return;
        };

        // SAFETY: an unnamed job with default security is two null pointers;
        // the call allocates the object and returns a handle we own.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            warn!(
                "We could not create the job object which stops the application when voice-orders does ({}).",
                std::io::Error::last_os_error()
            );
            return;
        }
        let job = Job(job);

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // SAFETY: the information class and the struct we point at match, and
        // the length we declare is that struct's own size.
        let configured = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            warn!(
                "We could not configure the job object which stops the application when voice-orders does ({}).",
                std::io::Error::last_os_error()
            );
            return;
        }

        // SAFETY: both handles are live — ours from `CreateJobObjectW`, the
        // child's borrowed from a `Child` which has not been reaped.
        if unsafe { AssignProcessToJobObject(job.0, handle) } == 0 {
            // A child which was launched into a job of its own (some launchers
            // and anti-cheat services do exactly this) cannot be adopted into
            // ours. Worth saying out loud, because it is the one case where
            // stopping voice-orders may leave the application running.
            warn!(
                "We could not take charge of the application's process tree ({}); if voice-orders is killed, the application may keep running.",
                std::io::Error::last_os_error()
            );
            return;
        }

        debug!("The application is contained in a job object.");
        *JOB.lock().unwrap_or_else(|e| e.into_inner()) = Some(job);
    }

    /// Terminates everything in the job, if we have one.
    pub(super) fn terminate() -> bool {
        let job = JOB.lock().unwrap_or_else(|e| e.into_inner());

        let Some(job) = job.as_ref() else {
            debug!("There is no job object to terminate the application with.");
            return false;
        };

        // SAFETY: the handle is ours and still open — nothing takes it out of
        // the static — and terminating a job is safe at any time.
        unsafe { TerminateJobObject(job.0, KILLED_EXIT_CODE) != 0 }
    }
}

/// Awaits a spawned pipeline task, turning both a panic and a returned error
/// into something reportable.
async fn join_task(what: &str, task: JoinHandle<Result<(), crate::Error>>) -> Option<crate::Error> {
    match task.await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(e) => Some(human_errors::wrap_system(
            e,
            format!("The {what} stopped unexpectedly."),
            &["Please report this issue on GitHub so that we can investigate."],
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedProfile;
    use crate::output::{CompiledOutput, KeyCode, KeyEvent, KeySink, keys};
    use crate::recognition::Utterance;
    use std::sync::Mutex;

    pub(super) fn profile(yaml: &str) -> Profile {
        Profile::parse(&LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: yaml.to_string(),
        })
        .expect("the profile should load")
    }

    #[test]
    fn test_the_feed_is_every_phrase_of_the_grammar() {
        let (_automaton, feed) = build_pipeline_parts(
            &profile(
                "model: /models/en\ngrammar: |\n  Deploy = \"deploy\" \"the\"? (\"autocannon\" | \"auto cannon\") { 4 }\n  Salute = \"salute\" { hold(x) }\n",
            ),
            "test-profile.yaml",
        )
        .expect("the profile should assemble");

        assert_eq!(
            feed.phrases,
            vec![
                "deploy autocannon",
                "deploy auto cannon",
                "deploy the autocannon",
                "deploy the auto cannon",
                "salute",
            ]
        );
        assert!(feed.decompositions.is_empty());
    }

    #[test]
    fn test_the_feed_never_carries_the_unknown_phrase() {
        // The recognizer appends "[unk]" itself; adding it here as well would
        // put a duplicate into the compiled grammar.
        let (_automaton, feed) = build_pipeline_parts(
            &profile("model: /models/en\ngrammar: |\n  Salute = \"salute\" { x }\n"),
            "test-profile.yaml",
        )
        .expect("the profile should assemble");

        assert!(
            !feed.phrases.iter().any(|p| p == vosk::UNKNOWN_PHRASE),
            "unexpected feed: {:?}",
            feed.phrases
        );
    }

    #[test]
    fn test_a_shared_phrase_reaches_the_recognizer_once() {
        // Both commands accept "the salute", but the feed is globally deduped,
        // so the recognizer is told about it once.
        let (_automaton, feed) = build_pipeline_parts(
            &profile(
                "model: /models/en\ngrammar: |\n  Salute = (\"the\" | \"a\") \"salute\" { x }\n  SaluteNow = \"the\" \"salute\" \"now\" { y }\n",
            ),
            "test-profile.yaml",
        )
        .expect("the profile should assemble");

        assert_eq!(
            feed.phrases,
            vec!["the salute", "a salute", "the salute now"]
        );
    }

    #[test]
    fn test_a_duplicate_command_is_an_error_naming_the_profile_and_both_rules() {
        // The grammar itself loads — accepting the same words twice is only
        // detected on the automaton — so the failure lands here, where it can
        // name the profile's source.
        let error = build_pipeline_parts(
            &profile(
                "model: /models/en\ngrammar: |\n  First = \"salute\" { x }\n  Second = \"the\"? \"salute\" { y }\n",
            ),
            "test-profile.yaml",
        )
        .expect_err("two commands cannot share a phrase with different keys");

        let message = error.to_string();
        assert!(
            message.contains("test-profile.yaml"),
            "the error should name the profile source, got: {message}"
        );
        assert!(
            message.contains("'First'") && message.contains("'Second'"),
            "the error should name both commands, got: {message}"
        );
    }

    // --- The engine, end to end over the canonical profile -----------------

    /// The full grammar path over a real shipped profile: `profiles/arma3.yaml`
    /// loads as a Profile, compiles, and a synthetic utterance walks through
    /// the engine to the command queue with its assembled key plan.
    #[tokio::test]
    async fn test_an_arma_command_fires_through_the_engine() {
        use crate::output::assembly::{ActionItem, assemble};

        let arma = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/profiles/arma3.yaml"));
        let profile = Profile::parse(&LoadedProfile {
            source: "profiles/arma3.yaml".to_string(),
            content: arma.to_string(),
        })
        .expect("the shipped profile should load");

        let (automaton, feed) = build_pipeline_parts(&profile, "profiles/arma3.yaml")
            .expect("the shipped profile should assemble");
        assert!(
            feed.phrases.iter().any(|phrase| phrase == "advance"),
            "the feed should carry the fragment the utterance needs"
        );

        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (queue_tx, mut queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let cancel = CancellationToken::new();
        let options = MatcherOptions::from_profile(&profile, Arc::new(|_| {}));
        let engine = tokio::spawn(engine_task(
            automaton,
            profile.defaults,
            options,
            events_rx,
            queue_tx,
            cancel.clone(),
        ));

        events_tx
            .send(RecognitionEvent::Final(Utterance::plain("two advance")))
            .await
            .expect("the engine should be listening");

        let action = tokio::time::timeout(Duration::from_secs(5), queue_rx.recv())
            .await
            .expect("the command should fire promptly")
            .expect("the queue should carry the command");
        assert_eq!(action.command, "Advance");

        // `Advance = subject ("advance" | "move up") { ..., 1, 2 }`: the
        // subject's F2 spliced ahead of the move-menu presses, paced by the
        // profile's defaults.
        let key = |name: &str| keys::from_name(name).expect("a known key");
        let expected = assemble(
            &[
                ActionItem::Press(vec![key("f2")]),
                ActionItem::Press(vec![key("1")]),
                ActionItem::Press(vec![key("2")]),
            ],
            &profile.defaults,
        );
        assert_eq!(action.output, CompiledOutput::Keyboard(expected));

        cancel.cancel();
        engine
            .await
            .expect("the engine should not panic")
            .expect("the engine should stop cleanly");
    }

    // --- Interrupting the executor ---------------------------------------

    #[rstest::rstest]
    // No hotkey at all: listening never stops, so nothing can interrupt.
    #[case("model: /models/en\n", false)]
    // A hotkey which does not ask for it: today's behaviour, an in-flight
    // command plays out in full.
    #[case("model: /models/en\nhotkey:\n  key: rightctrl\n", false)]
    #[case(
        "model: /models/en\nhotkey:\n  key: rightctrl\n  interrupt: false\n",
        false
    )]
    // A hotkey which does: the executor gets its own view of the listening
    // state, alongside the bridge's.
    #[case(
        "model: /models/en\nhotkey:\n  key: rightctrl\n  mode: push-to-talk\n  interrupt: true\n",
        true
    )]
    fn test_only_an_interrupting_hotkey_subscribes(#[case] yaml: &str, #[case] expected: bool) {
        let profile = profile(&format!(
            "{yaml}grammar: |\n  Salute = \"salute\" {{ x }}\n"
        ));
        let settings = ResolvedSettings::resolve(&profile, &SystemConfig::default())
            .expect("the settings should resolve");
        let (listening, _rx) = watch::channel(false);

        let interrupt = interrupt_for(&settings, &listening);

        assert_eq!(
            matches!(interrupt, Interrupt::WhenListeningStops(_)),
            expected,
            "unexpected interrupt {interrupt:?} for: {yaml:?}"
        );
        assert_eq!(
            listening.receiver_count(),
            1 + usize::from(expected),
            "only an interrupting executor should hold a second receiver"
        );
    }

    #[test]
    fn test_a_machine_supplied_hotkey_interrupts_too() {
        // The hotkey a shared profile never mentions still decides what
        // happens to the command in flight, because the merged settings are
        // the only thing the pipeline ever looks at.
        let system: crate::config::SystemConfig = serde_yaml::from_str(
            "hotkey:\n  key: rightctrl\n  mode: push-to-talk\n  interrupt: true\n",
        )
        .expect("the system configuration should load");

        let profile = profile("model: /models/en\ngrammar: |\n  Salute = \"salute\" { x }\n");
        let settings =
            ResolvedSettings::resolve(&profile, &system).expect("the settings should resolve");
        let (listening, _rx) = watch::channel(false);

        assert!(matches!(
            interrupt_for(&settings, &listening),
            Interrupt::WhenListeningStops(_)
        ));
    }

    // --- Narration ---------------------------------------------------------

    #[rstest::rstest]
    // Silence is silence, whatever happens.
    #[case(
        RecognitionEvent::Final(Utterance::plain("salute")),
        Narration::Silent,
        None
    )]
    #[case(RecognitionEvent::Partial("sal".into()), Narration::Silent, None)]
    #[case(RecognitionEvent::Muted, Narration::Silent, None)]
    #[case(RecognitionEvent::Failed, Narration::Silent, None)]
    // A recognizer which cannot decode is reported wherever anything is: a
    // session where nothing works otherwise looks like one where nobody spoke.
    #[case(
        RecognitionEvent::Failed,
        Narration::Utterances,
        Some("warning: the speech recognizer could not decode the audio")
    )]
    #[case(
        RecognitionEvent::Failed,
        Narration::Everything,
        Some("warning: the speech recognizer could not decode the audio")
    )]
    // `test` reports what it heard, and nothing else — an utterance with no
    // 'matched:' line under it is how an unrecognized command shows up.
    #[case(
        RecognitionEvent::Final(Utterance::plain("salute")),
        Narration::Utterances,
        Some("heard: \"salute\"")
    )]
    #[case(RecognitionEvent::Partial("sal".into()), Narration::Utterances, None)]
    #[case(RecognitionEvent::Muted, Narration::Utterances, None)]
    // `--debug-recognition` shows the working out as well.
    #[case(
        RecognitionEvent::Final(Utterance::plain("salute")),
        Narration::Everything,
        Some("heard: \"salute\"")
    )]
    #[case(
        RecognitionEvent::Partial("sal".into()),
        Narration::Everything,
        Some("hearing: \"sal\"")
    )]
    #[case(
        RecognitionEvent::Muted,
        Narration::Everything,
        Some("hearing: (muted)")
    )]
    fn test_narration_line(
        #[case] event: RecognitionEvent,
        #[case] narration: Narration,
        #[case] expected: Option<&str>,
    ) {
        // Asserted through the plain rendering with `live` off, because these
        // exact lines are `test`'s piped output — scripts and CI read them,
        // so they are pinned character for character and the live entries the
        // terminal UI needs must never leak into them.
        assert_eq!(
            narration_event(&event, narration, 1, false)
                .map(|reported| reported.plain_line())
                .as_deref(),
            expected,
            "{event:?} under {narration:?}"
        );
    }

    #[test]
    fn test_a_live_sink_gets_partials_and_mutes_with_their_slots() {
        // The terminal UI's live entries are driven by partials and mutes
        // whatever the narration level; each carries the slot the log needs
        // to attribute it. A partial belongs to the still-open slot (the one
        // after the last closed), a mute to the slot it just closed.
        assert_eq!(
            narration_event(
                &RecognitionEvent::Partial("sal".into()),
                Narration::Utterances,
                1,
                true
            ),
            Some(UiEvent::Hearing {
                text: "sal".into(),
                seq: 2
            })
        );
        assert_eq!(
            narration_event(&RecognitionEvent::Muted, Narration::Utterances, 2, true),
            Some(UiEvent::Muted { seq: 2 })
        );

        // Silent narration is still silence: plain `run` reports nothing at
        // all, and it never has a live sink to feed anyway.
        assert_eq!(
            narration_event(
                &RecognitionEvent::Partial("sal".into()),
                Narration::Silent,
                1,
                true
            ),
            None
        );
    }

    #[tokio::test]
    async fn test_narration_forwards_every_event_in_order() {
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (matcher_tx, mut matcher_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let forwarder = tokio::spawn(narrate_recognition(
            events_rx,
            matcher_tx,
            Narration::Utterances,
            EventSink::Plain,
        ));

        // Including the ones it prints nothing for: narration must never change
        // what the matcher sees, only what the terminal does.
        let sent = vec![
            RecognitionEvent::Partial("sal".to_string()),
            RecognitionEvent::Final(Utterance::plain("salute")),
            RecognitionEvent::Muted,
        ];
        for event in sent.clone() {
            events_tx.send(event).await.expect("it should be listening");
        }
        drop(events_tx);

        let mut received = Vec::new();
        while let Some(event) = matcher_rx.recv().await {
            received.push(event);
        }

        assert_eq!(received, sent);
        forwarder.await.expect("the forwarder should not panic");
    }

    #[tokio::test]
    async fn test_every_command_is_reported_on_its_way_to_the_keyboard() {
        // What the log says fired must be exactly what the executor played, in
        // order: the reporter is *in* the path rather than tapping it, for the
        // same reason the recognition narrator is.
        let (queue_tx, queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (executor_tx, mut executor_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();

        let reporter = tokio::spawn(narrate_commands(
            queue_rx,
            executor_tx,
            EventSink::Channel(events_tx),
        ));

        // The plan a real match assembles for `{ leftctrl+4 }` under the
        // default pacing.
        let key = |name: &str| keys::from_name(name).expect("a known key");
        let output = CompiledOutput::Keyboard(vec![
            KeyEvent::Down(key("leftctrl")),
            KeyEvent::Down(key("4")),
            KeyEvent::Wait(Duration::from_millis(30)),
            KeyEvent::Up(key("4")),
            KeyEvent::Up(key("leftctrl")),
        ]);
        queue_tx
            .send(CommandAction {
                command: "Autocannon".to_string(),
                output: output.clone(),
                utterance: 1,
            })
            .await
            .expect("the reporter should be listening");
        drop(queue_tx);

        assert_eq!(
            events_rx.recv().await,
            Some(UiEvent::Matched {
                name: "Autocannon".to_string(),
                // The plan a person reads, not the compiled event list — and
                // the same rendering `test` reports.
                plan: "leftctrl+4".to_string(),
                utterance: Some(1),
            })
        );

        let played = executor_rx
            .recv()
            .await
            .expect("the command should have reached the executor");
        assert_eq!(played.command, "Autocannon");
        assert_eq!(played.output, output);

        reporter
            .await
            .expect("the reporter should not panic")
            .expect("the reporter should stop cleanly when the queue closes");
    }

    // --- Child supervision -----------------------------------------------

    /// A future which never resolves, for the signals a test is not exercising.
    fn never<T>() -> impl Future<Output = T> {
        std::future::pending()
    }

    /// An interrupt which has already happened, of the given kind. Used only by
    /// the `cfg(unix)` supervision tests below.
    #[cfg(unix)]
    fn interrupted(shutdown: Shutdown) -> impl Future<Output = Shutdown> {
        std::future::ready(shutdown)
    }

    /// Spawns a real process for the supervisor to supervise.
    ///
    /// The tests which use it are `cfg(unix)`: they are about what a *signal*
    /// does to a real child, and they name real programs (`/bin/sleep`) to do
    /// it. The platform-independent half of the same behaviour — which steps
    /// each platform takes, in which order — is asserted from
    /// [`ChildStopPlan`] below, on every platform.
    #[cfg(unix)]
    async fn child(program: &str, args: &[&str]) -> Child {
        tokio::process::Command::new(program)
            .args(args)
            .spawn()
            .expect("the test child should start")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_successful_child_exits_zero() {
        let ending = supervise(Some(child("/bin/true", &[]).await), never(), never()).await;

        assert_eq!(ending, Ending::Child(0));
        assert_eq!(ending.exit_code(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_failing_child_propagates_its_code() {
        let ending = supervise(Some(child("/bin/false", &[]).await), never(), never()).await;

        assert_eq!(ending, Ending::Child(1));
        assert_eq!(ending.exit_code(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_interrupt_does_not_touch_the_child() {
        // SIGINT reaches the child through the process group, so we must not
        // signal it ourselves — the child here outlives the supervisor.
        let mut sleeping = child("/bin/sleep", &["30"]).await;

        let ending = supervise(None, interrupted(Shutdown::Quiet), never()).await;
        assert_eq!(ending, Ending::Interrupted);
        assert_eq!(ending.exit_code(), 0);

        assert!(
            sleeping.try_wait().expect("the child is ours").is_none(),
            "an interrupt must not stop anything itself"
        );
        sleeping.kill().await.expect("the child should be killable");
    }

    #[rstest::rstest]
    // Under the terminal UI, Ctrl-C is a keystroke: raw mode means the kernel
    // never delivered a SIGINT to anybody, so the child has to be told.
    #[case(Stop::Keyboard, true, Shutdown::ForwardSigint)]
    // Nothing to tell.
    #[case(Stop::Keyboard, false, Shutdown::Quiet)]
    // A real signal reached the whole process group already; a second one
    // would be a double interrupt, not a graceful shutdown.
    #[case(Stop::Signal, true, Shutdown::Quiet)]
    #[case(Stop::Signal, false, Shutdown::Quiet)]
    fn test_shutdown_for(#[case] stop: Stop, #[case] has_child: bool, #[case] expected: Shutdown) {
        assert_eq!(shutdown_for(stop, has_child), expected);
    }

    #[tokio::test]
    async fn test_quitting_the_ui_asks_for_the_interrupt_to_be_forwarded() {
        // The UI's only way of stopping the session is this token, and with a
        // child running that must become a SIGINT — the decision is made here
        // rather than in the key handler so it can be asserted without one.
        let quit = CancellationToken::new();
        quit.cancel();

        assert_eq!(
            stopped_by_ui(quit.clone(), true).await,
            Shutdown::ForwardSigint
        );
        assert_eq!(stopped_by_ui(quit, false).await, Shutdown::Quiet);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_keyboard_interrupt_is_forwarded_to_the_child() {
        // 'q' (or Ctrl-C) under the UI: the child gets the SIGINT the terminal
        // did not send, and is waited for rather than abandoned.
        let started = std::time::Instant::now();
        let ending = supervise(
            Some(child("/bin/sleep", &["30"]).await),
            interrupted(Shutdown::ForwardSigint),
            never(),
        )
        .await;

        assert_eq!(ending, Ending::Interrupted);
        assert_eq!(
            ending.exit_code(),
            0,
            "a session the user ended is a successful one"
        );
        assert!(
            started.elapsed() < SIGTERM_GRACE,
            "the child should have taken the signal well inside the grace period"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_keyboard_interrupt_without_a_child_signals_nothing() {
        let mut sleeping = child("/bin/sleep", &["30"]).await;

        let ending = supervise(None, interrupted(Shutdown::ForwardSigint), never()).await;

        assert_eq!(ending, Ending::Interrupted);
        assert!(
            sleeping.try_wait().expect("the child is ours").is_none(),
            "only the child we were given may ever be signalled"
        );
        sleeping.kill().await.expect("the child should be killable");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_termination_is_forwarded_to_the_child() {
        let started = std::time::Instant::now();
        let ending = supervise(
            Some(child("/bin/sleep", &["30"]).await),
            never(),
            std::future::ready(()),
        )
        .await;

        assert_eq!(ending, Ending::Terminated { child_exited: true });
        assert_eq!(ending.exit_code(), 0);
        assert!(
            started.elapsed() < SIGTERM_GRACE,
            "the child should have taken the signal well inside the grace period"
        );
    }

    #[tokio::test]
    async fn test_a_termination_without_a_child_is_immediate() {
        let ending = supervise(None, never(), std::future::ready(())).await;

        assert_eq!(ending, Ending::Terminated { child_exited: true });
        assert_eq!(ending.exit_code(), 0);
    }

    // --- How a child is stopped, per platform ------------------------------
    //
    // The choreography is derived rather than performed here, which is what
    // lets the Windows one be asserted from Linux: only the three one-line
    // platform functions it names (`send_signal`, `kill_contained`,
    // `contain_child`) are compiled for one platform each.

    #[rstest::rstest]
    #[case(Signal::Interrupt)]
    #[case(Signal::Terminate)]
    fn test_signals_are_the_whole_plan_where_signals_work(#[case] signal: Signal) {
        // Linux: ask, wait, and then leave the application alone. A wrapper
        // which escalated to SIGKILL would be overruling the user's program.
        assert_eq!(
            ChildStopPlan::new(signal, Containment::Signals).steps(),
            &[StopStep::Ask(signal), StopStep::Wait(SIGTERM_GRACE)]
        );
        assert!(!ChildStopPlan::new(signal, Containment::Signals).kills());
    }

    #[rstest::rstest]
    #[case(Signal::Interrupt)]
    #[case(Signal::Terminate)]
    fn test_a_job_object_ends_the_plan_where_the_ask_is_best_effort(#[case] signal: Signal) {
        // Windows: the same ask and the same grace period, and then the kill —
        // because a windowed application never received the ask at all.
        assert_eq!(
            ChildStopPlan::new(signal, Containment::JobObject).steps(),
            &[
                StopStep::Ask(signal),
                StopStep::Wait(SIGTERM_GRACE),
                StopStep::Kill
            ]
        );
        assert!(ChildStopPlan::new(signal, Containment::JobObject).kills());
    }

    #[test]
    fn test_the_plan_asks_before_it_waits_and_waits_before_it_kills() {
        // The ordering is the whole point: a kill which preceded the grace
        // period would make the grace period a lie.
        let plan = ChildStopPlan::new(Signal::Terminate, Containment::JobObject);
        let steps = plan.steps();

        assert!(matches!(steps[0], StopStep::Ask(_)));
        assert!(matches!(steps[1], StopStep::Wait(_)));
        assert_eq!(steps[2], StopStep::Kill);
    }

    #[test]
    fn test_this_platform_uses_the_containment_it_has() {
        assert_eq!(
            CONTAINMENT,
            if cfg!(windows) {
                Containment::JobObject
            } else {
                Containment::Signals
            },
            "a job object is Windows' only guarantee, and Linux needs none"
        );
    }

    #[rstest::rstest]
    fn test_no_application_means_no_child(
        #[values(ReportMode::Plain, ReportMode::Tui)] mode: ReportMode,
    ) {
        assert!(
            spawn_child(&[], mode)
                .expect("no application is not a failure")
                .is_none(),
            "an empty application list is the always-listening case"
        );
    }

    #[test]
    fn test_a_missing_application_is_named_in_the_error() {
        let error = spawn_child(
            &["/definitely/not/a/program".to_string()],
            ReportMode::Plain,
        )
        .expect_err("a missing executable cannot be started");

        assert!(
            error.to_string().contains("'/definitely/not/a/program'"),
            "the error should name the executable, got: {error}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_only_a_ui_session_pipes_the_childs_output() {
        // The wrapper contract: with no UI the child writes to our stdout and
        // stderr directly, exactly as though voice-orders were not here. Under
        // the UI it must not, or it would draw over the alternate screen.
        let app = vec!["/bin/true".to_string()];

        let mut plain = spawn_child(&app, ReportMode::Plain)
            .expect("the child should start")
            .expect("there is an application to wrap");
        assert!(plain.stdout.is_none(), "plain mode inherits our stdout");
        assert!(plain.stderr.is_none(), "plain mode inherits our stderr");
        plain.wait().await.expect("the child is ours");

        let mut piped = spawn_child(&app, ReportMode::Tui)
            .expect("the child should start")
            .expect("there is an application to wrap");
        assert!(
            piped.stdout.is_some(),
            "a UI session reads the child's output"
        );
        assert!(piped.stderr.is_some());
        piped.wait().await.expect("the child is ours");
    }

    #[rstest::rstest]
    // The header wants something short: a Steam launch command is a path
    // hundreds of characters long, and its last component is the game.
    #[case(&["/usr/bin/sleep", "30"], Some("sleep"))]
    #[case(&["sleep"], Some("sleep"))]
    #[case(&["/opt/games/Helldivers 2/bin/helldivers2"], Some("helldivers2"))]
    #[case(&["/opt/games/helldivers2/"], Some("helldivers2"))]
    // A path with no file name in it at all: the argument is still what we
    // were told to run, so it is what we say.
    #[case(&["/"], Some("/"))]
    #[case(&[], None)]
    fn test_program_name(#[case] app: &[&str], #[case] expected: Option<&str>) {
        let app: Vec<String> = app.iter().map(ToString::to_string).collect();

        assert_eq!(program_name(&app).as_deref(), expected);
    }

    #[rstest::rstest]
    // Plain `run` says nothing: its stdout belongs to the wrapped application
    // (and, under Steam, to a log nobody reads).
    #[case(ReportMode::Plain, false, Narration::Silent)]
    // Under the UI there is a log to fill, and it is the reason the UI exists.
    #[case(ReportMode::Tui, false, Narration::Utterances)]
    // `--debug-recognition` shows the working out either way.
    #[case(ReportMode::Plain, true, Narration::Everything)]
    #[case(ReportMode::Tui, true, Narration::Everything)]
    fn test_narration_for(
        #[case] mode: ReportMode,
        #[case] debug_recognition: bool,
        #[case] expected: Narration,
    ) {
        assert_eq!(narration_for(mode, debug_recognition), expected);
    }

    #[tokio::test]
    async fn test_the_childs_output_becomes_log_entries() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let output = "hello-from-child\nand another line\n";

        forward_lines(
            output.as_bytes(),
            "helldivers2".to_string(),
            EventSink::Channel(events_tx),
        )
        .await;

        let mut logged = Vec::new();
        while let Ok(event) = events_rx.try_recv() {
            logged.push(event);
        }

        assert_eq!(
            logged,
            vec![
                UiEvent::Child {
                    program: "helldivers2".to_string(),
                    line: "hello-from-child".to_string(),
                },
                UiEvent::Child {
                    program: "helldivers2".to_string(),
                    line: "and another line".to_string(),
                },
            ],
            "every line the application writes should be one entry, named after it"
        );
    }

    #[tokio::test]
    async fn test_the_forwarder_stops_when_the_stream_closes() {
        // A child which exits mid-line still gets its last line reported, and
        // the task must end rather than wait on a pipe nobody will write to.
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();

        tokio::time::timeout(
            Duration::from_secs(5),
            forward_lines(
                &b"no trailing newline"[..],
                "sh".to_string(),
                EventSink::Channel(events_tx),
            ),
        )
        .await
        .expect("the forwarder should end with the stream");

        assert_eq!(
            events_rx.try_recv(),
            Ok(UiEvent::Child {
                program: "sh".to_string(),
                line: "no trailing newline".to_string(),
            })
        );
    }

    // --- The matcher end of the pipeline ---------------------------------

    /// The command queue really does carry a `CommandAction` naming the command
    /// which fired, through the real matcher built from a real profile.
    #[tokio::test]
    async fn test_a_matched_utterance_reaches_the_command_queue() {
        let profile = profile("model: /models/en\ngrammar: |\n  Salute = \"salute\" { x }\n");
        let (automaton, _feed) = build_pipeline_parts(&profile, "test-profile.yaml")
            .expect("the profile should assemble");

        let cancel = CancellationToken::new();
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (queue_tx, mut queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);

        let matcher = tokio::spawn(engine_task(
            automaton,
            profile.defaults,
            MatcherOptions::with_timeout(profile.recognition.completion_timeout),
            events_rx,
            queue_tx,
            cancel.clone(),
        ));

        events_tx
            .send(RecognitionEvent::Final(Utterance::plain("salute")))
            .await
            .expect("the matcher should be listening");

        let action = tokio::time::timeout(Duration::from_secs(5), queue_rx.recv())
            .await
            .expect("a command should have been queued")
            .expect("the queue should not have closed");

        assert_eq!(action.command, "Salute");

        cancel.cancel();
        matcher
            .await
            .expect("the matcher should not panic")
            .expect("the matcher should shut down cleanly");
    }

    // --- Gated: the real model, wired to a real matcher and executor ------

    /// A [`KeySink`] which records what it was asked to emit.
    #[derive(Clone, Default)]
    struct FakeSink {
        pressed: Arc<Mutex<Vec<KeyEvent>>>,
    }

    impl KeySink for FakeSink {
        fn press(&mut self, key: KeyCode) -> impl Future<Output = Result<(), crate::Error>> + Send {
            self.pressed.lock().unwrap().push(KeyEvent::Down(key));
            std::future::ready(Ok(()))
        }

        fn release(
            &mut self,
            key: KeyCode,
        ) -> impl Future<Output = Result<(), crate::Error>> + Send {
            self.pressed.lock().unwrap().push(KeyEvent::Up(key));
            std::future::ready(Ok(()))
        }

        fn synchronize(&mut self) -> impl Future<Output = Result<(), crate::Error>> + Send {
            std::future::ready(Ok(()))
        }
    }

    fn model_path() -> PathBuf {
        std::env::var_os(crate::config::MODEL_PATH_ENV).map_or_else(
            || {
                PathBuf::from(std::env::var_os("HOME").expect("HOME must be set"))
                    .join(".cache/vosk/vosk-model-small-en-us-0.15")
            },
            PathBuf::from,
        )
    }

    /// Everything `run` assembles except the microphone and `/dev/uinput`: a
    /// grammar built from a real profile and compiled by a real Vosk model, and
    /// a real matcher and executor driven end to end by one synthetic
    /// utterance.
    ///
    /// Real audio and real uinput stay manual — they need hardware and a udev
    /// rule, and there is nothing left for them to prove that this does not.
    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn real_model_drives_the_assembled_pipeline() {
        let model = model_path();
        assert!(
            model.is_dir(),
            "no Vosk model at '{}' — download one from https://alphacephei.com/vosk/models and set VOSK_MODEL_PATH, or run with --features pure_tests to skip this test",
            model.display()
        );

        let profile = profile(
            "grammar: |\n  Salute = \"salute\" { x }\n  Terminal = \"open\" \"the\"? \"terminal\" { leftctrl+leftalt+t }\n",
        );
        let (automaton, feed) = build_pipeline_parts(&profile, "test-profile.yaml")
            .expect("the profile should assemble");
        let grammar = feed.phrases;

        // The grammar this profile produces really does compile against a real
        // model, which is the half of assembly no fake can stand in for.
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (recognizer, audio) = vosk::spawn_recognizer_with_drop_counter(
            &model,
            RECOGNIZER_SAMPLE_RATE,
            &grammar,
            crate::recognition::RecognizerOptions::default(),
            events_tx.clone(),
            Arc::new(AtomicU64::new(0)),
        )
        .expect("the recognizer should start");

        let cancel = CancellationToken::new();
        let (queue_tx, queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let sink = FakeSink::default();

        let matcher = tokio::spawn(engine_task(
            automaton,
            profile.defaults,
            MatcherOptions::with_timeout(profile.recognition.completion_timeout),
            events_rx,
            queue_tx,
            cancel.clone(),
        ));
        let output = tokio::spawn(executor(
            queue_rx,
            sink.clone(),
            cancel.clone(),
            Interrupt::Never,
        ));

        // One synthetic utterance, straight down the real event path.
        events_tx
            .send(RecognitionEvent::Final(Utterance::plain("salute")))
            .await
            .expect("the matcher should be listening");

        let key = keys::from_name("x").expect("a known key");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if sink.pressed.lock().unwrap().contains(&KeyEvent::Up(key)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the command never reached the executor: {:?}",
                sink.pressed.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            *sink.pressed.lock().unwrap(),
            vec![KeyEvent::Down(key), KeyEvent::Up(key)],
            "the profile's 'x' should have been pressed and released"
        );

        // And it all shuts down in the order the pipeline uses.
        cancel.cancel();
        drop(audio);
        drop(events_tx);
        tokio::task::spawn_blocking(move || recognizer.join())
            .await
            .expect("the join should not panic")
            .expect("the recognizer should shut down cleanly");
        matcher
            .await
            .expect("the matcher should not panic")
            .expect("the matcher should shut down cleanly");
        output
            .await
            .expect("the executor should not panic")
            .expect("the executor should shut down cleanly");
    }

    /// The eager-latency claim, end to end against the real recognizer: with
    /// eager on and **no trailing silence ever fed**, commands fire from
    /// stable partials alone — no `Final` is ever produced — and the last
    /// command lands within `debounce` (plus generous scheduling slack) of
    /// the last partial hypothesis.
    ///
    /// Real time rather than paused time: the recognizer decodes on its own
    /// thread, so the clock cannot be virtualized — the bounds are generous
    /// for CI, and the hard assertions are the *ordering* facts (digits fired,
    /// no `Final` involved).
    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn real_model_fires_eagerly_from_partials_without_a_final() {
        let model = model_path();
        assert!(
            model.is_dir(),
            "no Vosk model at '{}' — download one from https://alphacephei.com/vosk/models and set VOSK_MODEL_PATH, or run with --features pure_tests to skip this test",
            model.display()
        );

        // The digits the fixture speaks, each its own unambiguous command.
        // Rule names are the digits themselves — published TitleCase names in
        // the grammar, but matched case-insensitively when asserted below.
        let digits = ["one", "zero", "nine", "oh", "two", "eight", "three"];
        let source: String = digits
            .iter()
            .map(|word| {
                let mut name = word.to_string();
                name[..1].make_ascii_uppercase();
                format!("{name} = \"{word}\" {{ x }}\n")
            })
            .collect();
        let grammar_source =
            crate::grammar::Grammar::parse(&source).expect("the digit grammar should parse");
        let automaton = crate::grammar::Automaton::compile(&grammar_source)
            .expect("the digit grammar should compile");
        let grammar: Vec<String> = digits.iter().map(|word| (*word).to_string()).collect();

        // An endpointer that cannot fire behind our backs: 10s of trailing
        // silence would be needed, and the fixture is cut off mid-speech.
        let (events_tx, mut events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (recognizer, audio) = vosk::spawn_recognizer_with_drop_counter(
            &model,
            RECOGNIZER_SAMPLE_RATE,
            &grammar,
            crate::recognition::RecognizerOptions {
                silence: Duration::from_secs(10),
                alternatives: 0,
            },
            events_tx,
            Arc::new(AtomicU64::new(0)),
        )
        .expect("the recognizer should start");

        // A tap between the recognizer and the matcher records when each
        // event arrived, so the fire can be timed against the partial that
        // armed it.
        let (matcher_tx, matcher_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let seen: Arc<Mutex<Vec<(std::time::Instant, RecognitionEvent)>>> = Arc::default();
        let tap = tokio::spawn({
            let seen = seen.clone();
            async move {
                while let Some(event) = events_rx.recv().await {
                    seen.lock()
                        .unwrap()
                        .push((std::time::Instant::now(), event.clone()));
                    if matcher_tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });

        const DEBOUNCE: Duration = Duration::from_millis(150);
        let cancel = CancellationToken::new();
        let (queue_tx, mut queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let matcher = tokio::spawn(engine_task(
            automaton,
            crate::config::OutputDefaults::default(),
            crate::matcher::MatcherOptions {
                eager: true,
                debounce: DEBOUNCE,
                ..MatcherOptions::with_timeout(Duration::from_millis(300))
            },
            matcher_rx,
            queue_tx,
            cancel.clone(),
        ));

        // Real recorded speech (spoken digits, 16 kHz mono s16le), cut off
        // mid-utterance — and nothing appended: no silence means no
        // endpointer finalization, ever.
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
        for chunk in speech.chunks(1_600) {
            audio
                .send(AudioMsg::Frame(chunk.to_vec()))
                .expect("the recognizer should be listening");
        }

        // Collect every command that fires, allowing the decode plus the
        // eager delay to play out; the audio channel stays open the whole
        // time, so nothing here can come from a Final.
        let mut fired: Vec<(std::time::Instant, String)> = Vec::new();
        while let Ok(Some(action)) =
            tokio::time::timeout(Duration::from_secs(5), queue_rx.recv()).await
        {
            fired.push((std::time::Instant::now(), action.command));
        }

        assert!(
            !fired.is_empty(),
            "at least one digit should fire from partials alone; events seen: {:?}",
            seen.lock().unwrap()
        );
        assert!(
            fired
                .iter()
                .all(|(_, name)| digits.contains(&name.to_lowercase().as_str())),
            "only digit commands may fire: {fired:?}"
        );

        let events = seen.lock().unwrap().clone();
        assert!(
            !events
                .iter()
                .any(|(_, event)| matches!(event, RecognitionEvent::Final(_))),
            "no Final may be involved in an eager fire: {events:?}"
        );

        // The latency claim: the last command fired within debounce (plus
        // generous real-time slack for CI) of the partial which armed it —
        // i.e. of the last partial at or before the fire.
        let (last_fire_at, _) = fired.last().expect("checked non-empty above");
        let armed_at = events
            .iter()
            .rev()
            .find(|(at, event)| at <= last_fire_at && matches!(event, RecognitionEvent::Partial(_)))
            .map(|(at, _)| *at)
            .expect("a partial must precede an eager fire");
        let elapsed = last_fire_at.saturating_duration_since(armed_at);
        assert!(
            elapsed <= DEBOUNCE + Duration::from_millis(1_500),
            "the eager fire should land within debounce of the settled partial, took {elapsed:?}"
        );

        // Shutdown in the pipeline's order.
        cancel.cancel();
        drop(audio);
        tokio::task::spawn_blocking(move || recognizer.join())
            .await
            .expect("the join should not panic")
            .expect("the recognizer should shut down cleanly");
        tap.await.expect("the tap should not panic");
        matcher
            .await
            .expect("the matcher should not panic")
            .expect("the matcher should shut down cleanly");
    }
}
