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
//! supervised. The pure part of the assembly — profile in, compiled commands +
//! trie + grammar out — is [`build_pipeline_parts`], so it can be tested with no
//! hardware at all, and the supervisor's signal handling is [`supervise`], which
//! takes its triggers as futures for the same reason.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use clap::Args;
use tokio::process::Child;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use crate::audio;
use crate::config::{Profile, ResolvedSettings, SystemConfig, loader, resolve_model};
use crate::grammar::expansion;
use crate::hotkey::{self, ListenMode};
use crate::matcher::{CommandAction, CompiledCommand, PhraseTrie, matcher_task};
use crate::output::{Interrupt, UinputSink, executor};
use crate::recognition::{AudioMsg, RecognitionEvent, vosk};

use super::test::{EventSink, TestEvent};

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

    // 2. The grammar: every command compiled, every phrase expanded, and the
    //    trie built — which is where duplicate phrases become an error.
    let parts = build_pipeline_parts(&profile)?;

    // 3. The virtual keyboard, *first*: creating it is what fails when
    //    /dev/uinput is missing or unreadable, and finding that out after
    //    loading a model and opening a microphone would be needlessly slow.
    let sink = UinputSink::new().await?;

    // 4. The model, the recognizer, the microphone and the tasks.
    let model = resolve_model(args.model.as_deref(), &profile, &system)?;
    let options = PipelineOptions {
        narration: if args.debug_recognition {
            Narration::Everything
        } else {
            Narration::Silent
        },
        announce_listening: false,
        // `run` has no terminal UI: whatever it narrates goes straight to
        // stdout, exactly as it always has.
        events: EventSink::Plain,
    };
    let (mut pipeline, queue) = Pipeline::start(&profile, &settings, model, parts, options)?;

    // 5. The consumer of the command queue: for `run`, the virtual keyboard.
    let cancel = pipeline.cancel();
    let interrupt = pipeline.interrupt();
    pipeline.watch(
        "output executor",
        tokio::spawn(executor(queue, sink, cancel, interrupt)),
    );

    // 6. The child, if we were given one to wrap.
    let child = spawn_child(&args.app)?;

    // 7. Supervision: whichever of the child, SIGINT or SIGTERM arrives first
    //    ends the session.
    let ending = supervise(child, interrupts(), terminations()?).await;
    debug!("{ending}; shutting the pipeline down.");

    // 8. Shutdown, in the order DESIGN.md lays down.
    let failure = pipeline.shutdown().await;

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
        parts: (Vec<CompiledCommand>, PhraseTrie, Vec<String>),
        options: PipelineOptions,
    ) -> Result<(Self, mpsc::Receiver<CommandAction>), crate::Error> {
        let (commands, trie, grammar) = parts;

        let dropped_frames = Arc::new(AtomicU64::new(0));
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (recognizer, audio_tx) = vosk::spawn_recognizer_with_drop_counter(
            &model,
            RECOGNIZER_SAMPLE_RATE,
            &grammar,
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

        let mut tasks = vec![(
            "matcher",
            tokio::spawn(matcher_task(
                trie,
                commands,
                profile.completion_timeout,
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
                let device = hotkey::discover_device(&config.device, config.key.code())?;
                tasks.push((
                    "hotkey watcher",
                    tokio::spawn(hotkey::hotkey_task(
                        device,
                        config.key.code(),
                        mode,
                        listening_tx,
                        cancel.clone(),
                    )),
                ));

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
            commands: profile.commands.len(),
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
/// speech: the commands (with their output plans), the phrase trie, and the
/// deduped grammar handed to the recognizer.
///
/// This is the whole of the assembly which is a pure function of the profile,
/// which is what makes it testable without a model, a microphone or
/// `/dev/uinput`.
///
/// The grammar is the space-joined form of every expanded phrase, globally
/// deduped. [`vosk::UNKNOWN_PHRASE`] is deliberately *not* added here — the
/// recognizer appends it itself, and adding it twice would put a duplicate
/// entry into the compiled grammar.
pub(super) fn build_pipeline_parts(
    profile: &Profile,
) -> Result<(Vec<CompiledCommand>, PhraseTrie, Vec<String>), crate::Error> {
    let mut commands = Vec::with_capacity(profile.commands.len());
    let mut grammar = Vec::new();
    let mut seen = HashSet::new();

    for command in &profile.commands {
        let name = command.display_name().to_string();
        let output = command.compile(&profile.defaults)?;

        let expanded = expansion::expand(command.phrase.expr()).map_err(|e| {
            human_errors::wrap_user(
                e,
                format!("We could not work out what the command '{name}' listens for."),
                &[
                    "Simplify the command's phrase, or split it into several commands with shorter phrases.",
                ],
            )
        })?;

        for phrase in &expanded.phrases {
            // An empty expansion is an all-optional phrase nobody can say; the
            // trie rejects it below with a message naming the command, so it
            // must not reach the grammar in the meantime.
            if phrase.is_empty() {
                continue;
            }

            let joined = phrase.join(" ");
            if seen.insert(joined.clone()) {
                grammar.push(joined);
            }
        }

        commands.push(CompiledCommand {
            name,
            output,
            phrases: expanded.phrases,
        });
    }

    // Duplicate phrases across commands are an error here; they are only a
    // warning in `validate`, so that one run reports every problem at once.
    let trie = PhraseTrie::build(&commands)?;

    Ok((commands, trie, grammar))
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
                    events.send(TestEvent::Listening(now));
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
    while let Some(event) = events.recv().await {
        if let Some(reported) = narration_event(&event, narration) {
            sink.send(reported);
        }

        if matcher.send(event).await.is_err() {
            break;
        }
    }
}

/// The report one recognition event deserves, or [`None`] when it is noise the
/// user has not asked to see.
fn narration_event(event: &RecognitionEvent, narration: Narration) -> Option<TestEvent> {
    match (event, narration) {
        (_, Narration::Silent) => None,
        // A finalized utterance is the whole point: it is what the matcher gets
        // to work with, so it is reported whenever anything is reported at all.
        (RecognitionEvent::Final(text), _) => Some(TestEvent::Heard(text.clone())),
        // Partials and mutes are noise unless they were asked for.
        (RecognitionEvent::Partial(text), Narration::Everything) => {
            Some(TestEvent::Hearing(text.clone()))
        }
        (RecognitionEvent::Muted, Narration::Everything) => Some(TestEvent::Muted),
        _ => None,
    }
}

// --- Signals and the child process ---------------------------------------

/// Resolves when the user interrupts us (Ctrl+C).
///
/// If the handler cannot be installed we say so and then wait forever, so that
/// a machine which will not give us SIGINT still leaves SIGTERM working rather
/// than shutting the pipeline down the instant it starts.
pub(super) async fn interrupts() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!("We could not watch for Ctrl+C ({e}); use SIGTERM to stop voice-orders.");
        std::future::pending::<()>().await;
    }
}

/// A future which resolves when we are asked to terminate (SIGTERM — which is
/// how Steam stops a game).
pub(super) fn terminations() -> Result<impl Future<Output = ()>, crate::Error> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| {
            human_errors::wrap_system(
                e,
                "We could not install a handler for the SIGTERM signal.",
                &["Please report this issue on GitHub so that we can investigate."],
            )
        })?;

    Ok(async move {
        sigterm.recv().await;
    })
}

/// Starts the wrapped application, if we were given one.
///
/// stdio is inherited, so the child's output is the terminal's (or Steam's)
/// exactly as though voice-orders were not in the way at all.
fn spawn_child(app: &[String]) -> Result<Option<Child>, crate::Error> {
    let Some((executable, arguments)) = app.split_first() else {
        return Ok(None);
    };

    debug!(executable, "Starting the wrapped application.");

    tokio::process::Command::new(executable)
        .args(arguments)
        .spawn()
        .map(Some)
        .map_err(|e| {
            human_errors::wrap_user(
                e,
                format!("We could not start '{executable}'."),
                &[
                    "Check that the program exists, is executable, and is spelled correctly — everything after the '--' is passed to it untouched.",
                    "Steam launch options should read `voice-orders run profile.yaml -- %command%`, with the '--' before %command%.",
                ],
            )
        })
}

/// Why the session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    /// The wrapped application exited with this code, which becomes ours.
    Child(i32),
    /// SIGINT (Ctrl+C): the child shares our process group, so the kernel has
    /// already delivered the same signal to it — forwarding would double it.
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
/// that the semantics below can be tested without signalling the test runner.
/// Cancellation is the caller's job — every ending cancels, so doing it here
/// would only duplicate one line.
pub(super) async fn supervise(
    mut child: Option<Child>,
    interrupt: impl Future<Output = ()>,
    terminate: impl Future<Output = ()>,
) -> Ending {
    // Bound to its own `let` so the borrow `wait_for` takes is released before
    // the arms below need the child back.
    let outcome = tokio::select! {
        status = wait_for(child.as_mut()) => Outcome::Exited(status),
        _ = interrupt => Outcome::Interrupt,
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
        Outcome::Interrupt => Ending::Interrupted,
        Outcome::Terminate => {
            let child_exited = match child.as_mut() {
                Some(child) => forward_sigterm(child).await,
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
    Interrupt,
    Terminate,
}

/// Waits for a child to exit, or forever when there is no child to wait for.
async fn wait_for(child: Option<&mut Child>) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

/// Forwards SIGTERM to the child and gives it [`SIGTERM_GRACE`] to wind down.
///
/// Returns whether it actually stopped in time; we proceed with shutdown either
/// way, because an application which refuses to exit must not keep the wrapper
/// (and therefore Steam) hanging indefinitely.
async fn forward_sigterm(child: &mut Child) -> bool {
    let Some(pid) = child.id() else {
        // Already reaped: there is nothing left to signal.
        return true;
    };

    debug!(pid, "Forwarding SIGTERM to the application.");
    // SAFETY: `kill` is safe to call with any pid, and this one belongs to a
    // child we started and have not yet reaped, so it cannot have been recycled
    // onto an unrelated process.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        warn!("We could not pass the shutdown signal on to the application ({error}).");
    }

    match tokio::time::timeout(SIGTERM_GRACE, child.wait()).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            warn!("We lost track of the application while it was shutting down ({e}).");
            false
        }
        Err(_) => {
            warn!(
                "The application has not exited {}s after we asked it to; shutting down anyway.",
                SIGTERM_GRACE.as_secs()
            );
            false
        }
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
    use std::sync::Mutex;

    pub(super) fn profile(yaml: &str) -> Profile {
        Profile::parse(&LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: yaml.to_string(),
        })
        .expect("the profile should load")
    }

    #[test]
    fn test_the_grammar_is_every_phrase_of_every_command() {
        let (commands, _trie, grammar) = build_pipeline_parts(&profile(
            "model: /models/en\ncommands:\n  - name: Deploy\n    phrase: deploy [the] {autocannon, auto cannon}\n    keys: [\"4\"]\n  - phrase: salute\n    events:\n      - down: x\n",
        ))
        .expect("the profile should assemble");

        assert_eq!(
            grammar,
            vec![
                // The omitted branch of an '[optional]' group comes first.
                "deploy autocannon",
                "deploy auto cannon",
                "deploy the autocannon",
                "deploy the auto cannon",
                "salute",
            ]
        );

        // Names come from `display_name`, so the named command reports its name
        // and the unnamed one reports its phrase source.
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Deploy", "salute"]);

        assert_eq!(
            commands[1].output,
            CompiledOutput::Keyboard(vec![KeyEvent::Down(
                keys::from_name("x").expect("a known key")
            )])
        );
    }

    #[test]
    fn test_the_grammar_never_carries_the_unknown_phrase() {
        // The recognizer appends "[unk]" itself; adding it here as well would
        // put a duplicate into the compiled grammar.
        let (_commands, _trie, grammar) = build_pipeline_parts(&profile(
            "model: /models/en\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n",
        ))
        .expect("the profile should assemble");

        assert!(
            !grammar.iter().any(|p| p == vosk::UNKNOWN_PHRASE),
            "unexpected grammar: {grammar:?}"
        );
    }

    #[test]
    fn test_a_shared_phrase_appears_in_the_grammar_once() {
        // Both commands expand to include "the salute", but the grammar is
        // globally deduped, so the recognizer is told about it once.
        let (_commands, _trie, grammar) = build_pipeline_parts(&profile(
            "model: /models/en\ncommands:\n  - phrase: \"{the, a} salute\"\n    keys: [\"x\"]\n  - phrase: the salute now\n    keys: [\"y\"]\n",
        ))
        .expect("the profile should assemble");

        assert_eq!(
            grammar.iter().filter(|p| *p == "the salute").count(),
            1,
            "unexpected grammar: {grammar:?}"
        );
        assert_eq!(grammar, vec!["the salute", "a salute", "the salute now"]);
    }

    #[test]
    fn test_a_duplicate_phrase_is_an_error_naming_both_commands() {
        let error = build_pipeline_parts(&profile(
            "model: /models/en\ncommands:\n  - name: First\n    phrase: salute\n    keys: [\"x\"]\n  - name: Second\n    phrase: \"[the] salute\"\n    keys: [\"y\"]\n",
        ))
        .expect_err("two commands cannot share a phrase");

        let message = error.to_string();
        assert!(
            message.contains("'First'") && message.contains("'Second'"),
            "the error should name both commands, got: {message}"
        );
    }

    #[test]
    fn test_an_unspeakable_command_is_an_error_naming_it() {
        // Every term optional, so the phrase expands to include the empty
        // phrase — which the trie rejects by name.
        let error = build_pipeline_parts(&profile(
            "model: /models/en\ncommands:\n  - name: Ghost\n    phrase: \"[deploy] [the]\"\n    keys: [\"x\"]\n",
        ))
        .expect_err("an all-optional phrase cannot be spoken");

        assert!(
            error.to_string().contains("'Ghost'"),
            "the error should name the command, got: {error}"
        );
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
            "{yaml}commands:\n  - phrase: salute\n    keys: [\"x\"]\n"
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

        let profile =
            profile("model: /models/en\ncommands:\n  - phrase: salute\n    keys: [\"x\"]\n");
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
    #[case(RecognitionEvent::Final("salute".into()), Narration::Silent, None)]
    #[case(RecognitionEvent::Partial("sal".into()), Narration::Silent, None)]
    #[case(RecognitionEvent::Muted, Narration::Silent, None)]
    // `test` reports what it heard, and nothing else — an utterance with no
    // 'matched:' line under it is how an unrecognized command shows up.
    #[case(
        RecognitionEvent::Final("salute".into()),
        Narration::Utterances,
        Some("heard: \"salute\"")
    )]
    #[case(RecognitionEvent::Partial("sal".into()), Narration::Utterances, None)]
    #[case(RecognitionEvent::Muted, Narration::Utterances, None)]
    // `--debug-recognition` shows the working out as well.
    #[case(
        RecognitionEvent::Final("salute".into()),
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
        // Asserted through the plain rendering, because these exact lines are
        // `test`'s piped output — the terminal UI draws the same text.
        assert_eq!(
            narration_event(&event, narration)
                .map(|reported| reported.plain_line())
                .as_deref(),
            expected,
            "{event:?} under {narration:?}"
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
            RecognitionEvent::Final("salute".to_string()),
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

    // --- Child supervision -----------------------------------------------

    /// A future which never resolves, for the signals a test is not exercising.
    fn never() -> impl Future<Output = ()> {
        std::future::pending()
    }

    async fn child(program: &str, args: &[&str]) -> Child {
        tokio::process::Command::new(program)
            .args(args)
            .spawn()
            .expect("the test child should start")
    }

    #[tokio::test]
    async fn test_a_successful_child_exits_zero() {
        let ending = supervise(Some(child("/bin/true", &[]).await), never(), never()).await;

        assert_eq!(ending, Ending::Child(0));
        assert_eq!(ending.exit_code(), 0);
    }

    #[tokio::test]
    async fn test_a_failing_child_propagates_its_code() {
        let ending = supervise(Some(child("/bin/false", &[]).await), never(), never()).await;

        assert_eq!(ending, Ending::Child(1));
        assert_eq!(ending.exit_code(), 1);
    }

    #[tokio::test]
    async fn test_an_interrupt_does_not_touch_the_child() {
        // SIGINT reaches the child through the process group, so we must not
        // signal it ourselves — the child here outlives the supervisor.
        let mut sleeping = child("/bin/sleep", &["30"]).await;

        let ending = supervise(None, std::future::ready(()), never()).await;
        assert_eq!(ending, Ending::Interrupted);
        assert_eq!(ending.exit_code(), 0);

        assert!(
            sleeping.try_wait().expect("the child is ours").is_none(),
            "an interrupt must not stop anything itself"
        );
        sleeping.kill().await.expect("the child should be killable");
    }

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

    #[test]
    fn test_no_application_means_no_child() {
        assert!(
            spawn_child(&[])
                .expect("no application is not a failure")
                .is_none(),
            "an empty application list is the always-listening case"
        );
    }

    #[test]
    fn test_a_missing_application_is_named_in_the_error() {
        let error = spawn_child(&["/definitely/not/a/program".to_string()])
            .expect_err("a missing executable cannot be started");

        assert!(
            error.to_string().contains("'/definitely/not/a/program'"),
            "the error should name the executable, got: {error}"
        );
        assert!(error.is(human_errors::Kind::User));
    }

    // --- The matcher end of the pipeline ---------------------------------

    /// The command queue really does carry a `CommandAction` naming the command
    /// which fired, through the real matcher built from a real profile.
    #[tokio::test]
    async fn test_a_matched_utterance_reaches_the_command_queue() {
        let profile = profile(
            "model: /models/en\ncommands:\n  - name: Salute\n    phrase: salute\n    keys: [\"x\"]\n",
        );
        let (commands, trie, _grammar) =
            build_pipeline_parts(&profile).expect("the profile should assemble");

        let cancel = CancellationToken::new();
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (queue_tx, mut queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);

        let matcher = tokio::spawn(matcher_task(
            trie,
            commands,
            profile.completion_timeout,
            events_rx,
            queue_tx,
            cancel.clone(),
        ));

        events_tx
            .send(RecognitionEvent::Final("salute".to_string()))
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
            "commands:\n  - name: Salute\n    phrase: salute\n    keys: [\"x\"]\n  - name: Open terminal\n    phrase: open [the] terminal\n    keys: [\"leftctrl+leftalt+t\"]\n",
        );
        let (commands, trie, grammar) =
            build_pipeline_parts(&profile).expect("the profile should assemble");

        // The grammar this profile produces really does compile against a real
        // model, which is the half of assembly no fake can stand in for.
        let (events_tx, events_rx) = mpsc::channel(EVENTS_CHANNEL_CAPACITY);
        let (recognizer, audio) = vosk::spawn_recognizer_with_drop_counter(
            &model,
            RECOGNIZER_SAMPLE_RATE,
            &grammar,
            events_tx.clone(),
            Arc::new(AtomicU64::new(0)),
        )
        .expect("the recognizer should start");

        let cancel = CancellationToken::new();
        let (queue_tx, queue_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let sink = FakeSink::default();

        let matcher = tokio::spawn(matcher_task(
            trie,
            commands,
            profile.completion_timeout,
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
            .send(RecognitionEvent::Final("salute".to_string()))
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
}
