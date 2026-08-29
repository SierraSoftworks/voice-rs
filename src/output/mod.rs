//! Output emission: compiled key-event plans and the executor which plays
//! them onto the uinput virtual keyboard. See DESIGN.md §"Output forms".

// The executor and the uinput sink are wired up by `commands/run.rs`; until
// that lands nothing in the binary reaches them.
#![allow(dead_code)]

pub mod assembly;
pub mod keys;

// The virtual keyboard is `/dev/uinput` on Linux and `SendInput` on Windows.
// Both are the same shape — a [`KeySink`] with an async `new()` — so the `run`
// assembly names [`PlatformSink`] and never learns which one it got.
//
// `sendinput` is compiled everywhere on purpose. The half of it which turns a
// key into a `SendInput` record is pure arithmetic over the key table, and a
// table only Windows could see would be a table only Windows could test
// (`keys::to_windows` is compiled everywhere for the same reason); only the sink
// itself and the syscall it wraps are `cfg(windows)`.
pub mod sendinput;
#[cfg(target_os = "linux")]
pub mod uinput;

pub use keys::KeyCode;
// Re-exported for `commands/run.rs`; nothing reaches it until that lands.
#[allow(unused_imports)]
#[cfg(target_os = "linux")]
pub use uinput::UinputSink;

/// The keyboard sink this platform types through.
#[cfg(not(target_os = "linux"))]
pub use sendinput::WinKeySink as PlatformSink;
/// The keyboard sink this platform types through.
#[cfg(target_os = "linux")]
pub use uinput::UinputSink as PlatformSink;

use crate::Error;
use crate::matcher::CommandAction;
use std::future::Future;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

/// A fully compiled output plan for one command. An enum so that future
/// output kinds (mouse, process execution, audio playback) slot in without
/// touching the pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum CompiledOutput {
    Keyboard(Vec<KeyEvent>),
}

/// A single step in a keyboard output plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Down(KeyCode),
    Up(KeyCode),
    Wait(std::time::Duration),
    /// Release every key the virtual keyboard is currently holding — the
    /// grammar's `release(*)`, which is what makes a panic command possible
    /// (DESIGN.md §"Command semantics"). Plays from the executor's own tracked
    /// set, so it lets go of keys held by *earlier* commands too, and does
    /// nothing at all when nothing is held.
    ReleaseAll,
}

/// The seam between the executor and the virtual keyboard.
///
/// Production uses [`UinputSink`]; tests use a recording fake, which is what
/// lets the executor's ordering and timing guarantees be asserted without
/// touching `/dev/uinput` (DESIGN.md §"Testing strategy").
///
/// The futures are spelled out rather than written as `async fn` so that they
/// carry a `Send` bound and the executor can be spawned onto the multi-threaded
/// runtime.
pub trait KeySink {
    /// Emit a key-down event for `key`.
    fn press(&mut self, key: KeyCode) -> impl Future<Output = Result<(), Error>> + Send;

    /// Emit a key-up event for `key`.
    fn release(&mut self, key: KeyCode) -> impl Future<Output = Result<(), Error>> + Send;

    /// Flush the events emitted so far (`EV_SYN`/`SYN_REPORT`).
    fn synchronize(&mut self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Whether stopping listening also stops whatever is being played.
///
/// The profile's `hotkey.interrupt` decides between the two; with no hotkey at
/// all there is nothing which could ever stop listening, so the assembly always
/// passes [`Interrupt::Never`] (DESIGN.md §"`run` assembly & child processes").
#[derive(Debug, Clone, Default)]
pub enum Interrupt {
    /// An in-flight command always plays out in full. The default, and the only
    /// possibility without a hotkey.
    #[default]
    Never,
    /// Abandon the in-flight command — and everything queued behind it — the
    /// moment the watched listening state turns `false`.
    WhenListeningStops(watch::Receiver<bool>),
}

impl Interrupt {
    /// Interrupts whenever `listening` goes from `true` to `false`.
    ///
    /// The current state is marked as seen, so a pipeline which starts muted
    /// (toggle and push-to-talk both do) does not count that as a stop.
    pub fn when_listening_stops(mut listening: watch::Receiver<bool>) -> Self {
        let _ = listening.borrow_and_update();
        Self::WhenListeningStops(listening)
    }

    /// Resolves the moment listening stops; never, for [`Interrupt::Never`].
    ///
    /// Cancel-safe, so it can be raced against a sleep or a queue read: the
    /// only state it keeps is the watch channel's "seen" marker, which is
    /// advanced in the same poll as the change which moved it.
    ///
    /// A closed channel (the hotkey watcher has gone) is not a stop — shutdown
    /// arrives on the cancellation token instead — so it parks forever rather
    /// than spinning the caller's `select!` arm.
    ///
    /// `pub(crate)` because `voice-orders test` consumes the command queue with
    /// its own reporter, and a rehearsal must show what `run` would do.
    pub(crate) async fn triggered(&mut self) {
        match self {
            Interrupt::Never => std::future::pending::<()>().await,
            Interrupt::WhenListeningStops(listening) => loop {
                if listening.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }

                if !*listening.borrow_and_update() {
                    return;
                }
            },
        }
    }

    /// Has listening stopped since we last looked? Never blocks.
    fn stopped(&mut self) -> bool {
        match self {
            Interrupt::Never => false,
            Interrupt::WhenListeningStops(listening) => {
                listening.has_changed().unwrap_or(false) && !*listening.borrow_and_update()
            }
        }
    }
}

/// The keys the virtual keyboard is holding down, in the order they went down.
///
/// Press order is kept because every path which lets go of the whole set at
/// once — `release(*)`, an interrupt, shutdown — releases in the reverse of it,
/// so a modifier outlives the key it modifies exactly as a chord's own releases
/// do. A `Vec` rather than a `HashSet` for that reason alone: a virtual
/// keyboard never holds more than a handful of keys, so the linear scans cost
/// nothing and the ordering is the whole point.
#[derive(Debug, Default)]
struct HeldKeys(Vec<KeyCode>);

impl HeldKeys {
    /// Records that `key` is now down. Pressing a key which is already held
    /// keeps its original position, so it is still released in press order.
    fn pressed(&mut self, key: KeyCode) {
        if !self.0.contains(&key) {
            self.0.push(key);
        }
    }

    /// Records that `key` is no longer down.
    fn released(&mut self, key: KeyCode) {
        if let Some(position) = self.0.iter().position(|held| *held == key) {
            self.0.remove(position);
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    /// Empties the set, handing back the keys in the order they must be
    /// released: the reverse of the order they were pressed.
    fn take(&mut self) -> Vec<KeyCode> {
        let mut keys = std::mem::take(&mut self.0);
        keys.reverse();
        keys
    }
}

/// Plays command output plans from the command queue onto `sink`.
///
/// Runs until the queue closes or `cancel` fires, and **always** releases every
/// key it still holds down before returning — including on the error path. A
/// voice macro must never leave `W` held down in a game (DESIGN.md §"`run`
/// assembly & child processes").
///
/// `interrupt` decides what happens when listening stops mid-command: nothing
/// at all ([`Interrupt::Never`]), or the in-flight plan is abandoned where it
/// stands, its keys released, and every command queued behind it discarded.
/// Either way the executor keeps running — an interrupt is not a shutdown.
pub async fn executor<S: KeySink>(
    mut queue: mpsc::Receiver<CommandAction>,
    mut sink: S,
    cancel: CancellationToken,
    mut interrupt: Interrupt,
) -> Result<(), Error> {
    let mut held = HeldKeys::default();

    let outcome = loop {
        let action = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("Shutdown requested, stopping the output executor.");
                break Ok(());
            }
            () = interrupt.triggered() => {
                // Nothing is playing, but the matcher may have queued commands
                // behind us which we are no longer meant to type.
                discard_queued(&mut queue);
                continue;
            }
            action = queue.recv() => action,
        };

        let Some(action) = action else {
            debug!("The command queue was closed, stopping the output executor.");
            break Ok(());
        };

        info!(command = %action.command, "Executing command '{}'.", action.command);

        match &action.output {
            CompiledOutput::Keyboard(plan) => {
                match play(&mut sink, &mut held, plan, &cancel, &mut interrupt).await {
                    Ok(Played::Finished) => {}
                    Ok(Played::Cancelled) => break Ok(()),
                    Ok(Played::Interrupted) => {
                        info!(
                            command = %action.command,
                            "Listening stopped; interrupting '{}' and releasing its keys.",
                            action.command
                        );

                        if let Err(e) = release_all(&mut sink, &mut held).await {
                            break Err(e);
                        }

                        discard_queued(&mut queue);
                    }
                    Err(e) => break Err(e),
                }
            }
        }
    };

    // Whatever happened above, we own the responsibility for leaving the
    // virtual keyboard in a resting state.
    let released = release_all(&mut sink, &mut held).await;

    outcome.and(released)
}

/// How far [`play`] got through a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Played {
    /// Every event was emitted.
    Finished,
    /// `cancel` fired: we are shutting down, and the caller stops.
    Cancelled,
    /// Listening stopped: the caller releases, discards the queue, and carries
    /// on.
    Interrupted,
}

/// Plays a single compiled plan, tracking which keys end up held down.
///
/// Returns as soon as `cancel` fires or listening stops — including from under
/// a [`KeyEvent::Wait`], because the whole point of an interrupt is that a
/// half-second hold does not outlive the key you just let go of. The caller
/// releases whatever is still held either way.
async fn play<S: KeySink>(
    sink: &mut S,
    held: &mut HeldKeys,
    plan: &[KeyEvent],
    cancel: &CancellationToken,
    interrupt: &mut Interrupt,
) -> Result<Played, Error> {
    for event in plan {
        if cancel.is_cancelled() {
            return Ok(Played::Cancelled);
        }

        if interrupt.stopped() {
            return Ok(Played::Interrupted);
        }

        match *event {
            KeyEvent::Down(key) => {
                sink.press(key).await?;
                sink.synchronize().await?;
                held.pressed(key);
            }
            KeyEvent::Up(key) => {
                sink.release(key).await?;
                sink.synchronize().await?;
                held.released(key);
            }
            KeyEvent::ReleaseAll => {
                debug!(
                    keys = held.len(),
                    "Releasing the {} key(s) currently held down.",
                    held.len()
                );
                release_held(sink, held).await?;
            }
            KeyEvent::Wait(duration) => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(Played::Cancelled),
                    () = interrupt.triggered() => return Ok(Played::Interrupted),
                    _ = tokio::time::sleep(duration) => {}
                }
            }
        }
    }

    Ok(Played::Finished)
}

/// Throws away every command the matcher has already queued, naming each one.
///
/// Only ever called when listening has stopped: everything in the queue was
/// matched while we were still listening, and typing it now would be typing
/// after the user asked us to stop. Returns how many were discarded.
fn discard_queued(queue: &mut mpsc::Receiver<CommandAction>) -> usize {
    let mut discarded = 0;

    while let Ok(action) = queue.try_recv() {
        info!(
            command = %action.command,
            "Listening stopped; discarding the queued command '{}'.",
            action.command
        );
        discarded += 1;
    }

    discarded
}

/// Releases every key which is *still* held down and flushes the device.
///
/// Used both on the way out and when an interrupt cuts a plan short — neither
/// is something a well-written plan should need, hence the warning; the
/// deliberate [`KeyEvent::ReleaseAll`] goes to [`release_held`] directly.
async fn release_all<S: KeySink>(sink: &mut S, held: &mut HeldKeys) -> Result<(), Error> {
    if held.is_empty() {
        return Ok(());
    }

    warn!(
        keys = held.len(),
        "Releasing {} key(s) which were still held down.",
        held.len()
    );

    release_held(sink, held).await
}

/// Lets go of everything the virtual keyboard holds, newest key first.
///
/// Releasing in the reverse of press order keeps a modifier held until the keys
/// it modifies are up, which is what a game (and the X server) expects to see.
/// Holding nothing is a no-op: no releases, and no flush of an empty batch.
async fn release_held<S: KeySink>(sink: &mut S, held: &mut HeldKeys) -> Result<(), Error> {
    if held.is_empty() {
        return Ok(());
    }

    let mut result = Ok(());
    for key in held.take() {
        // Keep going even if one release fails: a stuck key is worse than a
        // duplicated error, and we want to free as many as we can.
        if let Err(e) = sink.release(key).await
            && result.is_ok()
        {
            result = Err(e);
        }
    }

    if let Err(e) = sink.synchronize().await
        && result.is_ok()
    {
        result = Err(e);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::Instant;

    /// One observation made by [`FakeSink`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SinkEvent {
        Press(KeyCode),
        Release(KeyCode),
        Synchronize,
    }

    /// A [`KeySink`] which records what it was asked to do and when.
    #[derive(Clone, Default)]
    struct FakeSink {
        log: Arc<Mutex<Vec<(SinkEvent, Instant)>>>,
    }

    impl FakeSink {
        fn record(&self, event: SinkEvent) {
            self.log.lock().unwrap().push((event, Instant::now()));
        }

        /// Every recorded event, with syncs elided.
        fn keys(&self) -> Vec<SinkEvent> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .map(|(e, _)| *e)
                .filter(|e| *e != SinkEvent::Synchronize)
                .collect()
        }

        /// Every recorded event, syncs included.
        fn all(&self) -> Vec<SinkEvent> {
            self.log.lock().unwrap().iter().map(|(e, _)| *e).collect()
        }

        /// The offset from `origin` at which each non-sync event happened.
        fn key_offsets(&self, origin: Instant) -> Vec<Duration> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| *e != SinkEvent::Synchronize)
                .map(|(_, at)| *at - origin)
                .collect()
        }
    }

    impl KeySink for FakeSink {
        async fn press(&mut self, key: KeyCode) -> Result<(), Error> {
            self.record(SinkEvent::Press(key));
            Ok(())
        }

        async fn release(&mut self, key: KeyCode) -> Result<(), Error> {
            self.record(SinkEvent::Release(key));
            Ok(())
        }

        async fn synchronize(&mut self) -> Result<(), Error> {
            self.record(SinkEvent::Synchronize);
            Ok(())
        }
    }

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("known key")
    }

    fn action(command: &str, plan: Vec<KeyEvent>) -> CommandAction {
        CommandAction {
            command: command.to_string(),
            output: CompiledOutput::Keyboard(plan),
            utterance: 1,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn plays_a_chord_in_order() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let cancel = CancellationToken::new();

        let (ctrl, alt, t) = (key("leftctrl"), key("leftalt"), key("t"));
        tx.send(action(
            "open the terminal",
            vec![
                KeyEvent::Down(ctrl),
                KeyEvent::Down(alt),
                KeyEvent::Down(t),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(t),
                KeyEvent::Up(alt),
                KeyEvent::Up(ctrl),
            ],
        ))
        .await
        .unwrap();
        drop(tx);

        executor(rx, sink.clone(), cancel, Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(ctrl),
                SinkEvent::Press(alt),
                SinkEvent::Press(t),
                SinkEvent::Release(t),
                SinkEvent::Release(alt),
                SinkEvent::Release(ctrl),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_key_event_is_synchronized() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let x = key("x");

        tx.send(action("salute", vec![KeyEvent::Down(x), KeyEvent::Up(x)]))
            .await
            .unwrap();
        drop(tx);

        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.all(),
            vec![
                SinkEvent::Press(x),
                SinkEvent::Synchronize,
                SinkEvent::Release(x),
                SinkEvent::Synchronize,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waits_are_timed_exactly() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let x = key("x");

        tx.send(action(
            "salute",
            vec![
                KeyEvent::Down(x),
                KeyEvent::Wait(Duration::from_millis(750)),
                KeyEvent::Up(x),
                KeyEvent::Wait(Duration::from_millis(25)),
                KeyEvent::Down(x),
                KeyEvent::Up(x),
            ],
        ))
        .await
        .unwrap();
        drop(tx);

        let origin = Instant::now();
        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.key_offsets(origin),
            vec![
                Duration::ZERO,
                Duration::from_millis(750),
                Duration::from_millis(775),
                Duration::from_millis(775),
            ]
        );
        assert_eq!(Instant::now() - origin, Duration::from_millis(775));
    }

    #[tokio::test(start_paused = true)]
    async fn timing_spans_multiple_commands() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (a, b) = (key("a"), key("b"));

        tx.send(action(
            "first",
            vec![
                KeyEvent::Down(a),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(a),
                KeyEvent::Wait(Duration::from_millis(25)),
            ],
        ))
        .await
        .unwrap();
        tx.send(action(
            "second",
            vec![
                KeyEvent::Down(b),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::Up(b),
            ],
        ))
        .await
        .unwrap();
        drop(tx);

        let origin = Instant::now();
        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(a),
                SinkEvent::Release(a),
                SinkEvent::Press(b),
                SinkEvent::Release(b),
            ]
        );
        assert_eq!(
            sink.key_offsets(origin),
            vec![
                Duration::ZERO,
                Duration::from_millis(30),
                Duration::from_millis(55),
                Duration::from_millis(85),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn releases_held_keys_on_cancellation() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let cancel = CancellationToken::new();

        // `w` and `leftshift` go down and stay down: a sprint-forward macro.
        let (w, shift) = (key("w"), key("leftshift"));

        tx.send(action(
            "sprint forward",
            vec![
                KeyEvent::Down(w),
                KeyEvent::Down(shift),
                KeyEvent::Wait(Duration::from_secs(3600)),
                KeyEvent::Up(shift),
                KeyEvent::Up(w),
            ],
        ))
        .await
        .unwrap();

        let handle = tokio::spawn(executor(rx, sink.clone(), cancel.clone(), Interrupt::Never));

        // Let the plan reach its (very long) wait, then shut down under it.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            sink.keys(),
            vec![SinkEvent::Press(w), SinkEvent::Press(shift)],
            "the macro should be mid-flight with both keys held"
        );

        cancel.cancel();
        handle.await.unwrap().unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
            ],
            "both held keys must be released on cancellation, newest first"
        );
        assert_eq!(
            sink.all().last(),
            Some(&SinkEvent::Synchronize),
            "the releases must be flushed to the device"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn releases_held_keys_when_the_queue_closes() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let w = key("w");

        // An unmatched `Down` is legal — a hold-style macro.
        tx.send(action("hold forward", vec![KeyEvent::Down(w)]))
            .await
            .unwrap();
        drop(tx);

        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.all(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Synchronize,
                SinkEvent::Release(w),
                SinkEvent::Synchronize,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_release_keys_it_never_held() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let x = key("x");

        tx.send(action("balanced", vec![KeyEvent::Down(x), KeyEvent::Up(x)]))
            .await
            .unwrap();
        drop(tx);

        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.keys(),
            vec![SinkEvent::Press(x), SinkEvent::Release(x)],
            "a balanced plan must not emit a second release"
        );
    }

    // --- `release(*)` -----------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn release_all_lets_go_in_reverse_press_order() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (w, shift, a) = (key("w"), key("leftshift"), key("a"));

        tx.send(action(
            "stand down",
            vec![
                KeyEvent::Down(w),
                KeyEvent::Down(shift),
                KeyEvent::Down(a),
                KeyEvent::Wait(Duration::from_millis(30)),
                KeyEvent::ReleaseAll,
            ],
        ))
        .await
        .unwrap();
        drop(tx);

        let origin = Instant::now();
        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Press(a),
                SinkEvent::Release(a),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
            ],
            "the keys must come up in the reverse of the order they went down"
        );
        assert_eq!(
            sink.key_offsets(origin).last(),
            Some(&Duration::from_millis(30)),
            "a release-everything is immediate; it adds no pacing of its own"
        );
        assert_eq!(
            sink.all().last(),
            Some(&SinkEvent::Synchronize),
            "the releases must be flushed to the device"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn release_all_with_nothing_held_does_nothing() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();

        tx.send(action("panic", vec![KeyEvent::ReleaseAll]))
            .await
            .unwrap();
        drop(tx);

        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert!(
            sink.all().is_empty(),
            "a panic command with nothing to release must not even flush the device"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn release_all_frees_keys_an_earlier_command_left_held() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (w, shift) = (key("w"), key("leftshift"));

        // The whole point of a panic command: it lets go of the sprint some
        // *other* command started.
        tx.send(action(
            "sprint forward",
            vec![KeyEvent::Down(w), KeyEvent::Down(shift)],
        ))
        .await
        .unwrap();
        tx.send(action("panic", vec![KeyEvent::ReleaseAll]))
            .await
            .unwrap();
        drop(tx);

        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
            ],
            "the panic command releases the hold, and the shutdown finds nothing left to free"
        );
    }

    // --- Assembled grammar plans, played end to end -----------------------

    /// Plays one assembled action program and reports what the keyboard saw
    /// and when, so the pacing rules are asserted against the real executor
    /// rather than against [`assembly::assemble`]'s output alone.
    async fn play_assembled(items: &[assembly::ActionItem]) -> (Vec<SinkEvent>, Vec<Duration>) {
        let pacing = assembly::Pacing {
            duration: Duration::from_millis(30),
            interval: Duration::from_millis(25),
        };

        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();

        tx.send(action("assembled", assembly::assemble(items, &pacing)))
            .await
            .unwrap();
        drop(tx);

        let origin = Instant::now();
        executor(rx, sink.clone(), CancellationToken::new(), Interrupt::Never)
            .await
            .unwrap();

        (sink.keys(), sink.key_offsets(origin))
    }

    #[tokio::test(start_paused = true)]
    async fn assembled_presses_are_separated_by_the_interval() {
        let (a, b) = (key("a"), key("b"));
        let (events, offsets) = play_assembled(&[
            assembly::ActionItem::Press(vec![a]),
            assembly::ActionItem::Press(vec![b]),
        ])
        .await;

        assert_eq!(
            events,
            vec![
                SinkEvent::Press(a),
                SinkEvent::Release(a),
                SinkEvent::Press(b),
                SinkEvent::Release(b),
            ]
        );
        assert_eq!(
            offsets,
            vec![
                Duration::ZERO,
                Duration::from_millis(30),
                Duration::from_millis(55),
                Duration::from_millis(85),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_assembled_wait_replaces_the_interval() {
        let (a, b) = (key("a"), key("b"));
        let (_, offsets) = play_assembled(&[
            assembly::ActionItem::Press(vec![a]),
            assembly::ActionItem::Wait(Duration::from_millis(20)),
            assembly::ActionItem::Press(vec![b]),
        ])
        .await;

        assert_eq!(
            offsets,
            vec![
                Duration::ZERO,
                Duration::from_millis(30),
                // 20ms after the first press let go — the interval it replaced
                // is not added on top of it.
                Duration::from_millis(50),
                Duration::from_millis(80),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_assembled_hold_spans_the_press_inside_it() {
        let (shift, one) = (key("leftshift"), key("1"));
        let (events, offsets) = play_assembled(&[
            assembly::ActionItem::Hold(vec![shift]),
            assembly::ActionItem::Press(vec![one]),
            assembly::ActionItem::Release(vec![shift]),
        ])
        .await;

        assert_eq!(
            events,
            vec![
                SinkEvent::Press(shift),
                SinkEvent::Press(one),
                SinkEvent::Release(one),
                SinkEvent::Release(shift),
            ],
            "the explicit release balances the hold, so the shutdown finds nothing to free"
        );
        assert_eq!(
            offsets,
            vec![
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_millis(30),
                Duration::from_millis(30),
            ],
            "a hold and a release are immediate; only the press is paced"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn returns_immediately_when_cancelled_while_idle() {
        let (_tx, rx) = mpsc::channel::<CommandAction>(4);
        let sink = FakeSink::default();
        let cancel = CancellationToken::new();
        cancel.cancel();

        executor(rx, sink.clone(), cancel, Interrupt::Never)
            .await
            .unwrap();

        assert!(sink.all().is_empty());
    }

    // --- Interrupting on the listening state ------------------------------

    /// The sprint-forward macro every interrupt test cuts short: two keys down,
    /// a wait nothing would ever sit through, then the releases.
    fn long_hold(w: KeyCode, shift: KeyCode) -> Vec<KeyEvent> {
        vec![
            KeyEvent::Down(w),
            KeyEvent::Down(shift),
            KeyEvent::Wait(Duration::from_secs(3600)),
            KeyEvent::Up(shift),
            KeyEvent::Up(w),
        ]
    }

    #[tokio::test(start_paused = true)]
    async fn interrupting_mid_wait_releases_and_discards_the_rest() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (listening_tx, listening_rx) = watch::channel(true);

        let (w, shift, x) = (key("w"), key("leftshift"), key("x"));

        tx.send(action("sprint forward", long_hold(w, shift)))
            .await
            .unwrap();
        // Matched while we were still listening, and so never to be typed.
        tx.send(action("queued behind it", vec![KeyEvent::Down(x)]))
            .await
            .unwrap();

        let handle = tokio::spawn(executor(
            rx,
            sink.clone(),
            CancellationToken::new(),
            Interrupt::when_listening_stops(listening_rx),
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            sink.keys(),
            vec![SinkEvent::Press(w), SinkEvent::Press(shift)],
            "the macro should be mid-flight with both keys held"
        );

        // Push-to-talk released: the hold must not outlive the key.
        listening_tx.send(false).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
            ],
            "both held keys must be released, and the rest of the plan skipped"
        );
        assert_eq!(
            sink.all().last(),
            Some(&SinkEvent::Synchronize),
            "the releases must be flushed to the device"
        );

        // And the executor is still running: listening resumes, and the next
        // command it is given plays normally.
        listening_tx.send(true).unwrap();
        tx.send(action("after the interrupt", vec![KeyEvent::Down(x)]))
            .await
            .unwrap();
        drop(tx);

        handle.await.unwrap().unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
                // The command queued behind the interrupted one was discarded,
                // so 'x' is pressed exactly once — by the command which came
                // after listening resumed.
                SinkEvent::Press(x),
                SinkEvent::Release(x),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn without_an_interrupt_the_plan_plays_out_in_full() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (listening_tx, _listening_rx) = watch::channel(true);

        let (w, shift, x) = (key("w"), key("leftshift"), key("x"));
        tx.send(action(
            "sprint forward",
            vec![
                KeyEvent::Down(w),
                KeyEvent::Down(shift),
                KeyEvent::Wait(Duration::from_millis(500)),
                KeyEvent::Up(shift),
                KeyEvent::Up(w),
            ],
        ))
        .await
        .unwrap();
        tx.send(action(
            "queued behind it",
            vec![KeyEvent::Down(x), KeyEvent::Up(x)],
        ))
        .await
        .unwrap();
        drop(tx);

        let handle = tokio::spawn(executor(
            rx,
            sink.clone(),
            CancellationToken::new(),
            Interrupt::Never,
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;
        // Listening stops — and with the default `hotkey.interrupt: false`
        // nothing whatsoever happens to the output.
        listening_tx.send(false).unwrap();

        handle.await.unwrap().unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
                SinkEvent::Press(x),
                SinkEvent::Release(x),
            ],
            "the plan and everything queued behind it must play out in full"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interrupting_while_idle_does_nothing() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (listening_tx, listening_rx) = watch::channel(true);
        let x = key("x");

        let handle = tokio::spawn(executor(
            rx,
            sink.clone(),
            CancellationToken::new(),
            Interrupt::when_listening_stops(listening_rx),
        ));

        // Nothing is playing and nothing is queued: the interrupt has nothing
        // to interrupt and nothing to discard.
        listening_tx.send(false).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            sink.all().is_empty(),
            "an interrupt with nothing in flight must not touch the keyboard"
        );

        listening_tx.send(true).unwrap();
        tx.send(action("salute", vec![KeyEvent::Down(x), KeyEvent::Up(x)]))
            .await
            .unwrap();
        drop(tx);

        handle.await.unwrap().unwrap();

        assert_eq!(
            sink.keys(),
            vec![SinkEvent::Press(x), SinkEvent::Release(x)],
            "the executor must still be listening to its queue afterwards"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_racing_the_queue_closing_still_releases_once() {
        let (tx, rx) = mpsc::channel(4);
        let sink = FakeSink::default();
        let (listening_tx, listening_rx) = watch::channel(true);

        let (w, shift) = (key("w"), key("leftshift"));
        tx.send(action("sprint forward", long_hold(w, shift)))
            .await
            .unwrap();

        let handle = tokio::spawn(executor(
            rx,
            sink.clone(),
            CancellationToken::new(),
            Interrupt::when_listening_stops(listening_rx),
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;

        // The matcher goes away in the same breath as the mute: the drain finds
        // a closed queue, and the executor stops once it has tidied up.
        drop(tx);
        listening_tx.send(false).unwrap();

        handle.await.unwrap().unwrap();

        assert_eq!(
            sink.keys(),
            vec![
                SinkEvent::Press(w),
                SinkEvent::Press(shift),
                SinkEvent::Release(shift),
                SinkEvent::Release(w),
            ],
            "the interrupt releases the held keys, and the shutdown must not release them again"
        );
    }
}
