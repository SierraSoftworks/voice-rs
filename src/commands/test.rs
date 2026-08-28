//! `voice-orders test <profile>`: rehearse a profile out loud without typing
//! anything. See DESIGN.md §"CLI".
//!
//! This is the same pipeline `run` assembles — the same microphone, the same
//! grammar-constrained recognizer, the same matcher, and the same hotkey, so
//! the keybind itself is exercised — with two things taken away:
//!
//! - **no `/dev/uinput`**, so a profile can be rehearsed before the udev rule
//!   and group membership are even set up; and
//! - **no child process**, because there is nothing to wrap.
//!
//! In place of the executor, the command queue is consumed by a reporter which
//! reports what *would* have been typed. Every finalized utterance is reported
//! as it is heard, so an utterance which matched nothing shows up as a `heard:`
//! line with no `matched:` line under it — which is exactly the thing you want
//! to see when a command refuses to trigger.
//!
//! Everything the rehearsal has to say travels as a single [`UiEvent`], the
//! same one `run` reports (`super::ui`), consumed by exactly one of two
//! renderers, chosen once by [`ReportMode`]:
//!
//! - the **terminal UI** when stdout is a TTY, which is what DESIGN.md §"The
//!   session terminal UI (ratatui)" describes; and
//! - the **plain line-printed report** otherwise, unchanged to the character,
//!   because piped output is something scripts and CI already read.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use crate::config::{Profile, ResolvedSettings, SystemConfig, loader, resolve_model};
use crate::matcher::CommandAction;
use crate::output::{CompiledOutput, Interrupt, KeyEvent};

use super::run::{
    Ending, Narration, Pipeline, PipelineOptions, build_pipeline_parts, interrupts, stopped_by_ui,
    supervise, terminations,
};
use super::ui::{EventSink, ReportMode, UiEvent, render_plan, tui};

#[derive(Args, Debug)]
pub struct TestArgs {
    /// The profile to rehearse: a local path or an https:// URL.
    pub profile: String,

    /// The Vosk model to recognize with.
    /// Overrides the profile's `model:` field and $VOSK_MODEL_PATH.
    #[arg(long)]
    pub model: Option<PathBuf>,

    /// Print partial recognition results as well as finalized ones.
    #[arg(long, hide = true)]
    pub debug_recognition: bool,
}

/// Rehearses a profile, returning the exit code to leave with.
pub async fn run(args: TestArgs) -> Result<i32, crate::Error> {
    let loaded = loader::load(&args.profile).await?;
    let profile = Profile::parse(&loaded)?;

    // The same steps `run` takes before it touches any hardware — this
    // machine's configuration merged into the profile, the grammar compiled,
    // the model resolved — but no uinput device is created here, which is the
    // whole point of the command.
    let system = SystemConfig::load()?;
    let settings = ResolvedSettings::resolve(&profile, &system)?;
    let parts = build_pipeline_parts(&profile)?;
    let model = resolve_model(args.model.as_deref(), &profile, &system)?;

    let (events, ui) = ReportMode::of(std::io::stdout().is_terminal()).sink();

    let (mut pipeline, queue) = Pipeline::start(
        &profile,
        &settings,
        model,
        parts,
        PipelineOptions {
            narration: if args.debug_recognition {
                Narration::Everything
            } else {
                Narration::Utterances
            },
            announce_listening: true,
            events: events.clone(),
        },
    )?;

    let cancel = pipeline.cancel();
    let interrupt = pipeline.interrupt();
    pipeline.watch(
        "command reporter",
        tokio::spawn(report_task(queue, cancel, interrupt, events)),
    );

    // No child to supervise either way: we run until the user stops us, from
    // the keyboard or with a signal.
    let (ending, failure) = match ui {
        Some(ui) => {
            let overview =
                tui::Overview::describe(&profile, &settings, &loaded.source, pipeline.summary());
            rehearse_on_screen(overview, ui).await?
        }
        None => {
            print_header(&profile, &settings, pipeline.summary());
            let ending = supervise(None, interrupts(), terminations()?).await;
            println!();
            (ending, None)
        }
    };

    debug!("{ending}; shutting the rehearsal down.");

    // The pipeline's own failure wins: the UI's is almost always a consequence
    // of the terminal going away, which is not the interesting half.
    match pipeline.shutdown().await.or(failure) {
        Some(e) => Err(e),
        None => Ok(ending.exit_code()),
    }
}

/// Runs the rehearsal behind the terminal UI, returning how it ended and
/// whatever the UI itself failed with.
///
/// The UI owns the screen for as long as it is up, so nothing here may print:
/// the ending is reported by the caller once the terminal has been handed back.
async fn rehearse_on_screen(
    overview: tui::Overview,
    events: mpsc::UnboundedReceiver<UiEvent>,
) -> Result<(Ending, Option<crate::Error>), crate::Error> {
    // Installed before the UI takes the terminal: a failure here has to be
    // reportable, and nothing is reportable once the alternate screen is up.
    let terminate = terminations()?;

    let quit = CancellationToken::new();
    let ui = tokio::spawn(tui::run(overview, events, quit.clone()));

    // Two ways out, and the supervisor has to watch for both: 'q' (and Ctrl-C,
    // which raw mode delivers as a key rather than a signal) cancels the token,
    // while a signal sent to the process still arrives as a signal. A rehearsal
    // wraps nothing, so neither has a child to tell about it.
    let ending = supervise(None, stopped_by_ui(quit.clone(), false), terminate).await;
    quit.cancel();

    let failure = match ui.await {
        Ok(result) => result.err(),
        Err(e) => Some(human_errors::wrap_system(
            e,
            "The rehearsal display stopped unexpectedly.",
            &["Please report this issue on GitHub so that we can investigate."],
        )),
    };

    Ok((ending, failure))
}

/// Prints what we are about to rehearse, so the report below it has context:
/// which microphone, which model, and whether the hotkey has to be pressed
/// before anything will be heard at all.
fn print_header(
    profile: &Profile,
    settings: &ResolvedSettings,
    summary: &super::run::PipelineSummary,
) {
    println!("{} — rehearsal (nothing is typed)", profile.display_name());
    println!("  microphone: {}", summary.device);
    println!("  model:      {}", summary.model.display());
    println!(
        "  commands:   {} ({} phrase(s))",
        summary.commands, summary.phrases
    );

    match summary.mode {
        Some(mode) => {
            let key = settings
                .hotkey
                .as_ref()
                .map_or_else(String::new, |hotkey| hotkey.key.to_string());
            println!("  hotkey:     {key} ({mode})");
            println!(
                "listening: {}",
                if summary.listening { "on" } else { "off" }
            );
        }
        None => println!("  hotkey:     none (always listening)"),
    }

    println!();
    println!("Speak a command; press Ctrl-C to stop.");
}

/// Consumes the command queue and reports what each match *would* have typed.
///
/// Stands in for the uinput executor, and shuts down the same way it does: on
/// cancellation, or when the matcher closes the queue.
///
/// With `hotkey.interrupt: true` it also *behaves* the way the executor does
/// when listening stops: a reported command occupies the wall-clock time its
/// plan would have taken, so letting go of the key part-way through prints an
/// `interrupted:` line for it and a `discarded:` line for everything the
/// matcher had already queued behind it. Without the option there is nothing
/// which could ever cut a plan short, so nothing is waited out and the report
/// stays as immediate as it has always been.
async fn report_task(
    mut queue: mpsc::Receiver<CommandAction>,
    cancel: CancellationToken,
    mut interrupt: Interrupt,
    events: EventSink,
) -> Result<(), crate::Error> {
    loop {
        let action = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("Shutdown requested, stopping the command reporter.");
                return Ok(());
            }
            () = interrupt.triggered() => {
                // Nothing is being rehearsed, but the matcher may have queued
                // commands behind us which `run` would now throw away.
                discard_queued(&mut queue, &events);
                continue;
            }
            action = queue.recv() => action,
        };

        let Some(action) = action else {
            debug!("The command queue was closed, stopping the command reporter.");
            return Ok(());
        };

        events.send(UiEvent::Matched {
            name: action.command.clone(),
            plan: render_plan(&action.output),
        });

        if matches!(interrupt, Interrupt::Never) {
            continue;
        }

        let interrupted = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("Shutdown requested, stopping the command reporter.");
                return Ok(());
            }
            () = interrupt.triggered() => true,
            () = tokio::time::sleep(plan_duration(&action.output)) => false,
        };

        if interrupted {
            events.send(UiEvent::Interrupted(action.command));
            discard_queued(&mut queue, &events);
        }
    }
}

/// Reports every command the matcher had already queued as discarded.
///
/// The mirror image of the executor's drain: `run` would throw these away
/// unplayed, so a rehearsal must not pretend they still fire.
fn discard_queued(queue: &mut mpsc::Receiver<CommandAction>, events: &EventSink) {
    while let Ok(action) = queue.try_recv() {
        events.send(UiEvent::Discarded(action.command));
    }
}

/// How long the executor would take to play a plan: its waits, since the key
/// events themselves are as good as instantaneous.
fn plan_duration(output: &CompiledOutput) -> Duration {
    let CompiledOutput::Keyboard(plan) = output;

    plan.iter()
        .map(|event| match event {
            KeyEvent::Wait(duration) => *duration,
            _ => Duration::ZERO,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedProfile;
    use rstest::rstest;
    use std::time::Duration;

    /// Compiles one command's `keys:`/`events:` YAML the way the pipeline does,
    /// so the rendering is asserted against real compiled plans rather than
    /// hand-built ones.
    fn plan(command: &str) -> CompiledOutput {
        let profile = Profile::parse(&LoadedProfile {
            source: "test-profile.yaml".to_string(),
            content: format!("model: /models/en\ncommands:\n  - phrase: salute\n{command}"),
        })
        .expect("the profile should load");

        profile.commands[0]
            .compile(&profile.defaults)
            .expect("the command should compile")
    }

    #[tokio::test]
    async fn test_the_reporter_drains_the_queue_and_stops_with_it() {
        let (queue_tx, queue_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let reporter = tokio::spawn(report_task(
            queue_rx,
            cancel.clone(),
            Interrupt::Never,
            EventSink::Plain,
        ));

        queue_tx
            .send(CommandAction {
                command: "Salute".to_string(),
                output: plan("    keys: [\"x\"]\n"),
            })
            .await
            .expect("the reporter should be listening");

        // Closing the queue is how the matcher tells the reporter it is done.
        drop(queue_tx);

        tokio::time::timeout(Duration::from_secs(5), reporter)
            .await
            .expect("the reporter should stop when the queue closes")
            .expect("the reporter should not panic")
            .expect("the reporter should stop cleanly");
    }

    #[tokio::test]
    async fn test_the_reporter_stops_on_cancellation() {
        let (queue_tx, queue_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let reporter = tokio::spawn(report_task(
            queue_rx,
            cancel.clone(),
            Interrupt::Never,
            EventSink::Plain,
        ));

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(5), reporter)
            .await
            .expect("the reporter should stop promptly once cancelled")
            .expect("the reporter should not panic")
            .expect("the reporter should stop cleanly");

        drop(queue_tx);
    }

    // --- Interrupting a rehearsal ----------------------------------------

    #[rstest]
    // The `keys:` shorthand: one hold, and no interval after the last chord.
    #[case("    keys: [\"x\"]\n", Duration::from_millis(30))]
    // Two chords: hold, interval, hold.
    #[case("    keys: [\"a\", \"b\"]\n", Duration::from_millis(85))]
    // The explicit form times exactly what it says.
    #[case(
        "    events:\n      - down: x\n      - wait: 750ms\n      - up: x\n",
        Duration::from_millis(750)
    )]
    // A hold-style macro takes no time at all to *start*.
    #[case("    events:\n      - down: w\n", Duration::ZERO)]
    fn test_plan_duration(#[case] command: &str, #[case] expected: Duration) {
        assert_eq!(plan_duration(&plan(command)), expected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_an_interrupt_cuts_a_rehearsal_short_and_drains_the_queue() {
        let (queue_tx, queue_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let (listening_tx, listening_rx) = tokio::sync::watch::channel(true);
        let reporter = tokio::spawn(report_task(
            queue_rx,
            cancel.clone(),
            Interrupt::when_listening_stops(listening_rx),
            EventSink::Plain,
        ));

        // A long hold, and two commands the matcher queued behind it.
        for command in ["Sprint", "Reload", "Salute"] {
            queue_tx
                .send(CommandAction {
                    command: command.to_string(),
                    output: plan("    events:\n      - down: w\n      - wait: 1h\n      - up: w\n"),
                })
                .await
                .expect("the reporter should be listening");
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            queue_tx.capacity(),
            2,
            "the reporter should have taken one command and be sitting out its plan"
        );

        // Listening stops: the first is interrupted, the other two discarded.
        listening_tx.send(false).expect("the reporter is listening");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            queue_tx.capacity(),
            4,
            "everything queued behind the interrupted command should have been discarded"
        );

        drop(queue_tx);
        tokio::time::timeout(Duration::from_secs(5), reporter)
            .await
            .expect("the reporter should stop when the queue closes")
            .expect("the reporter should not panic")
            .expect("the reporter should stop cleanly");
    }
}
