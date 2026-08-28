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
//! prints what *would* have been typed. Every finalized utterance is printed as
//! it is heard, so an utterance which matched nothing shows up as a `heard:`
//! line with no `matched:` line under it — which is exactly the thing you want
//! to see when a command refuses to trigger.

use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use crate::config::{Profile, ResolvedSettings, SystemConfig, loader, resolve_model};
use crate::matcher::CommandAction;
use crate::output::{CompiledOutput, Interrupt, KeyCode, KeyEvent};

use super::run::{
    Narration, Pipeline, PipelineOptions, build_pipeline_parts, interrupts, supervise, terminations,
};

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
        },
    )?;

    print_header(&profile, &settings, pipeline.summary());

    let cancel = pipeline.cancel();
    let interrupt = pipeline.interrupt();
    pipeline.watch(
        "command reporter",
        tokio::spawn(report_task(queue, cancel, interrupt)),
    );

    // No child to supervise: we run until the user stops us.
    let ending = supervise(None, interrupts(), terminations()?).await;
    debug!("{ending}; shutting the rehearsal down.");
    println!();

    match pipeline.shutdown().await {
        Some(e) => Err(e),
        None => Ok(ending.exit_code()),
    }
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
                discard_queued(&mut queue);
                continue;
            }
            action = queue.recv() => action,
        };

        let Some(action) = action else {
            debug!("The command queue was closed, stopping the command reporter.");
            return Ok(());
        };

        println!(
            "matched: {:?} → {}",
            action.command,
            render_plan(&action.output)
        );

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
            println!("interrupted: {:?}", action.command);
            discard_queued(&mut queue);
        }
    }
}

/// Reports every command the matcher had already queued as discarded.
///
/// The mirror image of the executor's drain: `run` would throw these away
/// unplayed, so a rehearsal must not pretend they still fire.
fn discard_queued(queue: &mut mpsc::Receiver<CommandAction>) {
    while let Ok(action) = queue.try_recv() {
        println!("discarded: {:?}", action.command);
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

/// Renders a compiled output plan the way a person reads a macro.
///
/// Keys pressed together come back as a `+`-joined chord, waits are elided (the
/// hold and interval timings are a `run` concern, not a "did I say the right
/// thing?" one), and the two unbalanced cases a profile is allowed to contain
/// are called out rather than silently dropped: a key which is never released
/// (a hold-style macro) and a release with no press before it.
fn render_plan(output: &CompiledOutput) -> String {
    let CompiledOutput::Keyboard(plan) = output;

    let mut steps: Vec<String> = Vec::new();
    // The keys of the chord being assembled, in the order they were pressed,
    // and how many of them are still held down.
    let mut chord: Vec<KeyCode> = Vec::new();
    let mut holding = 0usize;

    for event in plan {
        match *event {
            KeyEvent::Down(key) => {
                if !chord.contains(&key) {
                    chord.push(key);
                    holding += 1;
                }
            }
            KeyEvent::Up(key) => {
                if !chord.contains(&key) {
                    steps.push(format!("(release {key})"));
                    continue;
                }

                holding -= 1;
                // The chord is only finished once every key in it is back up.
                if holding == 0 {
                    steps.push(render_chord(&chord));
                    chord.clear();
                }
            }
            KeyEvent::Wait(_) => {}
        }
    }

    if !chord.is_empty() {
        steps.push(format!("{} (held)", render_chord(&chord)));
    }

    if steps.is_empty() {
        return "(nothing)".to_string();
    }

    steps.join(" ")
}

/// One chord: its key names joined by `+`, in the order they go down.
fn render_chord(keys: &[KeyCode]) -> String {
    keys.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LoadedProfile, OutputDefaults};
    use crate::output::keys;
    use rstest::rstest;
    use std::time::Duration;

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("a known key")
    }

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

    #[rstest]
    // A single key: the hold wait is elided.
    #[case("    keys: [\"4\"]\n", "4")]
    // A chord: pressed in order, released in reverse, reported as one step.
    #[case("    keys: [\"leftctrl+leftalt+t\"]\n", "leftctrl+leftalt+t")]
    // A sequence: the inter-chord interval is elided too.
    #[case("    keys: [\"a\", \"b\"]\n", "a b")]
    #[case("    keys: [\"leftshift+a\", \"b\"]\n", "leftshift+a b")]
    // The explicit form, including its long hold.
    #[case(
        "    events:\n      - down: x\n      - wait: 750ms\n      - up: x\n",
        "x"
    )]
    // A hold-style macro: legal, and worth saying out loud.
    #[case("    events:\n      - down: w\n", "w (held)")]
    // A release with no press before it: also legal, also worth saying.
    #[case(
        "    events:\n      - up: w\n      - down: x\n      - up: x\n",
        "(release w) x"
    )]
    fn test_render_plan(#[case] command: &str, #[case] expected: &str) {
        assert_eq!(render_plan(&plan(command)), expected);
    }

    #[test]
    fn test_an_empty_plan_says_so() {
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(Vec::new())),
            "(nothing)"
        );
    }

    #[test]
    fn test_waits_alone_are_elided_entirely() {
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(vec![KeyEvent::Wait(
                Duration::from_secs(1)
            )])),
            "(nothing)"
        );
    }

    #[test]
    fn test_a_repeated_press_does_not_break_the_chord() {
        // Nothing in the schema produces this, but the renderer must not
        // underflow its hold count if something ever does.
        assert_eq!(
            render_plan(&CompiledOutput::Keyboard(vec![
                KeyEvent::Down(key("x")),
                KeyEvent::Down(key("x")),
                KeyEvent::Up(key("x")),
            ])),
            "x"
        );
    }

    #[tokio::test]
    async fn test_the_reporter_drains_the_queue_and_stops_with_it() {
        let (queue_tx, queue_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let reporter = tokio::spawn(report_task(queue_rx, cancel.clone(), Interrupt::Never));

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
        let reporter = tokio::spawn(report_task(queue_rx, cancel.clone(), Interrupt::Never));

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

    #[test]
    fn test_the_defaults_do_not_leak_into_the_rendering() {
        // The timings a `keys:` list compiles to are a `run` concern; a
        // rehearsal is about which keys, in which order.
        assert_eq!(
            OutputDefaults::default().duration,
            Duration::from_millis(30)
        );
        assert_eq!(render_plan(&plan("    keys: [\"a\"]\n")), "a");
    }
}
