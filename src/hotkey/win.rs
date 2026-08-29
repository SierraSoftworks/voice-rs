//! The Windows half of the listen hotkey: a `WH_KEYBOARD_LL` hook which
//! *observes* rather than consumes.
//!
//! A low-level keyboard hook is the closest Windows analogue of reading
//! `/dev/input/event*`: it sees keys system-wide, including inside fullscreen
//! games, and it sees them before any application does. The hook always calls
//! `CallNextHookEx` — it never swallows the hotkey, so the key still reaches the
//! game, exactly as an evdev read does (DESIGN.md §"Windows support").
//!
//! Three facts shape the implementation:
//!
//! 1. **A hook needs a message pump.** `SetWindowsHookExW` binds the hook to the
//!    installing thread, and Windows delivers callbacks by pumping that thread's
//!    message queue. That is a dedicated OS thread with a `GetMessageW` loop,
//!    not a Tokio task; shutdown is a `WM_QUIT` posted to it.
//! 2. **The callback has no context pointer.** `HOOKPROC` takes nothing we can
//!    hang state off, so the channel it forwards on and the key it is watching
//!    for live in a `static`. The sender is never dropped for as long as the
//!    process runs, because a hook which fires after its channel died would be
//!    using freed state.
//! 3. **The callback must be fast.** Windows silently removes a hook which
//!    exceeds `LowLevelHooksTimeout`, so [`hook_proc`] allocates nothing, logs
//!    nothing, locks nothing beyond a bounded `try_send`, and does its matching
//!    with the integer comparisons in [`classify`].
//!
//! **Privacy:** a system-wide hook is handed every keystroke on the machine, and
//! this one looks at nothing but whether the event is the single configured
//! hotkey. Nothing is stored, nothing is logged, and every other key is
//! discarded inside [`classify`] on its code alone (DESIGN.md §"Risks" 3).
//!
//! `hotkey.device` has no meaning here: a low-level hook is system-wide rather
//! than per-device, so device selection stays a Linux concept until Raw Input
//! (`WM_INPUT` with `RIDEV_INPUTSINK`) makes per-keyboard selection real. A
//! profile which sets it gets one warning and is otherwise watched normally.
//!
//! Everything above the syscalls — which events are ours, which key an event is,
//! and how a stream of hook callbacks becomes the `0`/`1`/`2` key values
//! [`transition`](super::transition) expects — is pure and compiled on every
//! platform, so it is tested where this project is developed rather than only
//! where it runs.

use crate::output::KeyCode;
use crate::output::keys::WinKey;
use crate::output::sendinput::INJECTED_MARKER;

/// The `hotkey.device` value which means "we choose"; the only one Windows can
/// honour, because the hook is system-wide.
const AUTO_DEVICE: &str = "auto";

/// The name the hook's message-pump thread runs under.
const THREAD_NAME: &str = "hotkey-hook";

/// How many hotkey transitions may be in flight between the hook and the task.
///
/// A hotkey produces a handful of events a second at worst, and the consumer
/// only ever does integer arithmetic and a `watch` send, so this is enormous
/// slack. It exists solely so that the hook can `try_send` and never block: a
/// blocking hook is a removed hook.
const QUEUE_DEPTH: usize = 64;

// The `KBDLLHOOKSTRUCT` flags and the four keyboard messages we care about,
// mirrored as plain integers so the matching compiles (and is tested) off
// Windows. A `cfg(windows)` test pins each against the `windows-sys` constant it
// copies, the same way the key table pins its evdev codes.
const LLKHF_EXTENDED: u32 = 0x0001;
const LLKHF_INJECTED: u32 = 0x0010;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;

/// Which way the hotkey moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The key went down — possibly again, while already held.
    Down,
    /// The key came back up.
    Up,
}

/// The parts of a `KBDLLHOOKSTRUCT` callback [`classify`] looks at.
///
/// A plain-integer copy of the fields, so the decision is testable without a
/// hook to be called by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookEvent {
    /// The event's virtual-key code.
    pub vk: u32,
    /// The event's scancode, without any `E0` prefix.
    pub scan: u32,
    /// The `LLKHF_*` flags: extended-key and injected live here.
    pub flags: u32,
    /// The `dwExtraInfo` the injector attached, if any.
    pub extra: usize,
    /// The message: `WM_KEYDOWN`, `WM_KEYUP`, or their `SYS` variants.
    pub message: u32,
}

/// Whether this hook callback is the configured hotkey moving, and which way.
///
/// `None` — the overwhelmingly common answer — means the event is none of our
/// business and the hook simply passes it on. Four things make it `None`:
///
/// - **It is our own output.** Every key voice-orders presses carries
///   [`INJECTED_MARKER`] in `dwExtraInfo`; without this check, a macro which
///   types the hotkey would toggle listening.
/// - **It is somebody else's injection.** `LLKHF_INJECTED` covers every
///   synthetic keystroke, ours included: a hotkey should mean a physical key,
///   not a remote-desktop replay or another macro tool's output.
/// - **It is a different key**, matched on scancode plus the extended flag
///   (which is what makes right control distinguishable from left), or on the
///   virtual key for the one row — `pause` — whose scancode is a multi-byte
///   sequence.
/// - **It is not a key transition at all**, which the message says.
pub fn classify(event: HookEvent, target: WinKey) -> Option<Edge> {
    if event.extra == INJECTED_MARKER || event.flags & LLKHF_INJECTED != 0 {
        return None;
    }

    let extended = event.flags & LLKHF_EXTENDED != 0;
    let matches = match target {
        WinKey::Scan(scan) => event.scan == u32::from(scan) && !extended,
        WinKey::ScanExt(scan) => event.scan == u32::from(scan) && extended,
        // `pause` arrives as the `E1 1D 45` sequence, whose scancode field is
        // not the whole story; its virtual key is unambiguous.
        WinKey::VirtualKey(vk) => event.vk == u32::from(vk),
    };

    if !matches {
        return None;
    }

    match event.message {
        WM_KEYDOWN | WM_SYSKEYDOWN => Some(Edge::Down),
        WM_KEYUP | WM_SYSKEYUP => Some(Edge::Up),
        _ => None,
    }
}

/// Turns a stream of hook edges into the key values [`transition`] speaks.
///
/// Windows repeats `WM_KEYDOWN` while a key is held, exactly as the kernel
/// repeats `EV_KEY` with value `2` on Linux — but it does not label the repeats,
/// so we label them ourselves by remembering whether the key is already down.
/// The distinction matters: a held push-to-talk key must not flap, which is
/// precisely what [`transition`](super::transition) uses the `2` to avoid.
///
/// [`transition`]: super::transition
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Repeats {
    /// Whether we have seen a down without its matching up.
    down: bool,
}

impl Repeats {
    /// The evdev key value this edge amounts to: `1` pressed, `2` auto-repeat,
    /// `0` released.
    pub fn value(&mut self, edge: Edge) -> i32 {
        match edge {
            Edge::Down if self.down => 2,
            Edge::Down => {
                self.down = true;
                1
            }
            Edge::Up => {
                self.down = false;
                0
            }
        }
    }
}

/// The hotkey is a key `SendInput`'s encoding has no room for.
///
/// Unreachable with the current key table — every row has an encoding, and a
/// test says so — but the table is data, and this is what a future gap would
/// say rather than watching for nothing.
fn unwatchable_key(key: KeyCode) -> crate::Error {
    human_errors::user(
        format!(
            "We cannot watch for the '{key}' key on Windows, because it has no scancode or virtual-key code a keyboard hook could match."
        ),
        &[
            "Choose a different key for 'hotkey.key', or run 'voice-orders keys' to see the keys we can watch for.",
            "Please report this issue on GitHub so that we can add the missing key.",
        ],
    )
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tracing_batteries::prelude::*;
    use windows_sys::Win32::Foundation::{GetLastError, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
        PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_QUIT,
        WM_USER,
    };

    use super::{
        AUTO_DEVICE, Edge, HookEvent, QUEUE_DEPTH, Repeats, THREAD_NAME, classify, unwatchable_key,
    };
    use crate::hotkey::{ListenMode, transition};
    use crate::output::{KeyCode, keys};

    /// Everything [`hook_proc`] needs, and the only way it can have it: a
    /// `HOOKPROC` is a bare function pointer with nowhere to hang state.
    struct HookState {
        /// Where matched transitions go. Never dropped for the life of the
        /// process — see [`HOOK`].
        events: mpsc::Sender<Edge>,
        /// The encoding of the key we are watching for.
        target: keys::WinKey,
    }

    /// The hook's state, for as long as the process lives.
    ///
    /// A `OnceLock` rather than anything replaceable on purpose. The hook can
    /// fire on any thread at any moment between `SetWindowsHookExW` and the
    /// `UnhookWindowsHookEx` which retires it, so the sender must outlive every
    /// possible callback — and the simplest way to guarantee that is for it
    /// never to be dropped at all. The cost is that one process watches one
    /// hotkey, which is exactly what a `run` does.
    static HOOK: OnceLock<HookState> = OnceLock::new();

    /// The low-level keyboard callback.
    ///
    /// Runs on the hook thread, inside Windows' input path, with a deadline: a
    /// hook slower than `LowLevelHooksTimeout` is silently removed and the
    /// hotkey stops working with no error anywhere. So: no allocation, no
    /// logging, no locking beyond the bounded `try_send`, and `CallNextHookEx`
    /// on every path — we observe the hotkey, we never consume it.
    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // Negative codes are Windows telling us not to look, only to forward.
        if code >= 0
            && let Some(state) = HOOK.get()
        {
            // SAFETY: for a non-negative code, `WH_KEYBOARD_LL` guarantees
            // `lparam` points at a `KBDLLHOOKSTRUCT` which Windows keeps valid
            // for the duration of this call. We only read it, and only here.
            let raw = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };

            let event = HookEvent {
                vk: raw.vkCode,
                scan: raw.scanCode,
                flags: raw.flags,
                extra: raw.dwExtraInfo,
                message: wparam as u32,
            };

            if let Some(edge) = classify(event, state.target) {
                // A full queue means the consumer is wedged; dropping the edge
                // is the only option which keeps this callback fast.
                let _ = state.events.try_send(edge);
            }
        }

        // SAFETY: passing the hook chain along is always legal; a null `HHOOK`
        // is the documented way to say "the next hook after this one".
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    /// The thread which owns the hook and pumps its messages.
    struct HookThread {
        /// The thread's id, which is what `WM_QUIT` is posted to.
        tid: u32,
        /// `None` once the thread has been handed to [`HookThread::shutdown`].
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl HookThread {
        /// Installs the hook on a fresh thread, returning once it is live.
        ///
        /// Blocking until the thread reports back is deliberate: installing the
        /// hook is part of *starting* the watcher, so a failure is reported to
        /// the `run` assembly before anything is spawned — the same contract the
        /// Linux path has when it resolves an evdev device.
        fn install() -> Result<Self, crate::Error> {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, u32>>();

            let thread = std::thread::Builder::new()
                .name(THREAD_NAME.to_string())
                .spawn(move || {
                    // SAFETY: a null module handle with a zero thread id is the
                    // documented way to install a global low-level hook from
                    // this process; `hook_proc` outlives the hook.
                    let hook = unsafe {
                        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0)
                    };

                    if hook.is_null() {
                        // SAFETY: reads this thread's own last-error value.
                        let _ = ready_tx.send(Err(unsafe { GetLastError() }));
                        return;
                    }

                    let mut msg = MSG::default();

                    // Force the message queue into existence before anybody is
                    // told our thread id: `PostThreadMessageW` fails against a
                    // thread which has never called a message function, and the
                    // shutdown path must not be able to lose that race.
                    // SAFETY: `msg` is a valid, writable `MSG` for the call.
                    unsafe {
                        PeekMessageW(
                            &mut msg,
                            std::ptr::null_mut(),
                            WM_USER,
                            WM_USER,
                            PM_NOREMOVE,
                        )
                    };

                    // SAFETY: takes no arguments and reads only our own id.
                    let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));

                    loop {
                        // SAFETY: `msg` is valid and writable; a null window
                        // handle asks for this thread's own messages.
                        let pumped = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };

                        // `0` is the `WM_QUIT` our shutdown posts; `-1` is an
                        // error which every subsequent call would repeat.
                        if pumped <= 0 {
                            break;
                        }
                    }

                    // SAFETY: `hook` is the handle we were given above, and this
                    // is the only place it is removed.
                    unsafe { UnhookWindowsHookEx(hook) };
                })
                .map_err(|e| {
                    human_errors::wrap_system(
                        e,
                        "We could not start the thread which watches for your listen hotkey.",
                        &["Please report this issue on GitHub so that we can investigate."],
                    )
                })?;

            match ready_rx.recv() {
                Ok(Ok(tid)) => Ok(Self {
                    tid,
                    thread: Some(thread),
                }),
                // The thread ran, failed to install, and exited; joining it is
                // immediate and keeps the failure tidy.
                Ok(Err(code)) => {
                    let _ = thread.join();
                    Err(install_failed(code))
                }
                // The sender was dropped without a verdict, which means the
                // thread panicked before it could report one.
                Err(_) => {
                    let _ = thread.join();
                    Err(install_failed(0))
                }
            }
        }

        /// Stops the hook thread and waits for it to unhook.
        ///
        /// The join happens on the blocking pool: the thread is inside
        /// `GetMessageW` until the `WM_QUIT` reaches it, and a runtime worker is
        /// not the place to wait for that.
        async fn shutdown(mut self) {
            self.post_quit();

            if let Some(thread) = self.thread.take()
                && tokio::task::spawn_blocking(move || thread.join())
                    .await
                    .is_err()
            {
                debug!("The hotkey hook thread did not shut down cleanly.");
            }
        }

        /// Asks the pump to stop. Safe to call more than once.
        fn post_quit(&self) {
            // SAFETY: posts a message to a thread id we own; the handle cannot
            // have been recycled while we still hold the thread's join handle.
            unsafe { PostThreadMessageW(self.tid, WM_QUIT, 0, 0) };
        }
    }

    impl Drop for HookThread {
        /// A watcher future which is dropped rather than run to completion —
        /// its task cancelled out from under it — must still take the hook down.
        /// We post the `WM_QUIT` and let the thread retire on its own rather
        /// than joining, because a `Drop` which blocks a runtime worker on a
        /// message pump is its own kind of bug.
        fn drop(&mut self) {
            if self.thread.take().is_some() {
                debug!("The hotkey watcher was dropped; retiring its keyboard hook.");
                self.post_quit();
            }
        }
    }

    /// Windows refused to install the hook.
    ///
    /// This is rare enough that there is no well-known cause to advise on: the
    /// call needs no privilege and no configuration, so a failure is genuinely
    /// something to report rather than something to fix.
    fn install_failed(last_error: u32) -> crate::Error {
        human_errors::system(
            format!(
                "Windows would not let us install the keyboard hook we watch for your listen hotkey with (error {last_error})."
            ),
            &[
                "Security software occasionally blocks low-level keyboard hooks; if you have any, check whether it is holding voice-orders back.",
                "Please report this issue on GitHub so that we can investigate.",
            ],
        )
    }

    /// A second hotkey in one process, which the `static` the callback reads its
    /// state from cannot represent.
    fn already_watching() -> crate::Error {
        human_errors::system(
            "We are already watching for a listen hotkey in this process, and Windows only lets us watch for one.",
            &["Please report this issue on GitHub so that we can investigate."],
        )
    }

    /// Starts watching for the listen hotkey, returning the task which does it.
    ///
    /// Mirrors the Linux [`crate::hotkey::watch`] exactly, including doing the
    /// fallible part — here, installing the hook rather than resolving a device
    /// — *before* returning a future, so the `run` assembly reports the failure
    /// before it spins anything up.
    pub fn watch(
        device_hint: &str,
        key: KeyCode,
        mode: ListenMode,
        listening: tokio::sync::watch::Sender<bool>,
        cancel: CancellationToken,
    ) -> Result<
        impl std::future::Future<Output = Result<(), crate::Error>> + Send + 'static,
        crate::Error,
    > {
        if device_hint != AUTO_DEVICE {
            warn!(
                device = device_hint,
                "device selection is not available on Windows; the hotkey is watched system-wide"
            );
        }

        let target = keys::to_windows(key).ok_or_else(|| unwatchable_key(key))?;

        if HOOK.get().is_some() {
            return Err(already_watching());
        }

        let (events, edges) = mpsc::channel(QUEUE_DEPTH);
        let hook = HookThread::install()?;

        if HOOK.set(HookState { events, target }).is_err() {
            // Lost a race with another watcher; `hook` retires as it drops.
            return Err(already_watching());
        }

        Ok(watcher(edges, key, mode, listening, cancel, hook))
    }

    /// Consumes the hook's edges and publishes listening-state changes.
    ///
    /// The logic below the channel is the shared one: the same
    /// [`transition`](crate::hotkey::transition) the Linux task uses, fed the
    /// same `0`/`1`/`2` key values — with the auto-repeat value synthesized by
    /// [`Repeats`], because Windows repeats a held key without labelling the
    /// repeats.
    async fn watcher(
        mut edges: mpsc::Receiver<Edge>,
        key: KeyCode,
        mode: ListenMode,
        listening: tokio::sync::watch::Sender<bool>,
        cancel: CancellationToken,
        hook: HookThread,
    ) -> Result<(), crate::Error> {
        debug!("Listening for the {mode} hotkey ('{key}') with a system-wide keyboard hook.");

        let mut repeats = Repeats::default();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("The hotkey watcher is shutting down.");
                    break;
                }
                edge = edges.recv() => {
                    // The sender lives in a `static` and is never dropped, so
                    // this is unreachable; treating it as a shutdown rather than
                    // spinning on a closed channel is the safe reading.
                    let Some(edge) = edge else { break };

                    let value = repeats.value(edge);
                    let current = *listening.borrow();
                    if let Some(next) = transition(mode, current, value) {
                        debug!(
                            "The listen hotkey turned listening {}.",
                            if next { "on" } else { "off" }
                        );
                        // `send_replace` rather than `send`: the state change
                        // must land even in the window where no receiver happens
                        // to be alive, and shutdown is the token's job.
                        listening.send_replace(next);
                    }
                }
            }
        }

        hook.shutdown().await;

        Ok(())
    }

    /// Installs a low-level keyboard hook and immediately removes it.
    ///
    /// `voice-orders doctor`'s equivalent of creating a throwaway virtual
    /// keyboard on Linux: rather than inferring that hooking would work, it
    /// hooks. Nothing is pumped in between, so no callback can arrive — the
    /// question being asked is whether Windows will hand us the hook at all.
    pub fn probe_hook() -> Result<(), crate::Error> {
        // SAFETY: as in `HookThread::install` — a null module handle and a zero
        // thread id install a global hook whose callback outlives it.
        let hook: HHOOK =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0) };

        if hook.is_null() {
            // SAFETY: reads this thread's own last-error value.
            return Err(install_failed(unsafe { GetLastError() }));
        }

        // SAFETY: the handle we were just given, removed exactly once.
        unsafe { UnhookWindowsHookEx(hook) };

        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::super::{
            LLKHF_EXTENDED, LLKHF_INJECTED, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging as wam;

        /// The plain-integer mirrors of the flags and messages must be the
        /// flags and messages.
        #[test]
        fn the_mirrored_constants_are_the_windows_ones() {
            assert_eq!(LLKHF_EXTENDED, wam::LLKHF_EXTENDED);
            assert_eq!(LLKHF_INJECTED, wam::LLKHF_INJECTED);
            assert_eq!(WM_KEYDOWN, wam::WM_KEYDOWN);
            assert_eq!(WM_KEYUP, wam::WM_KEYUP);
            assert_eq!(WM_SYSKEYDOWN, wam::WM_SYSKEYDOWN);
            assert_eq!(WM_SYSKEYUP, wam::WM_SYSKEYUP);
        }

        /// The hook must be installable on this machine, which is the whole of
        /// what `doctor`'s check 3 asks.
        #[test]
        #[cfg_attr(feature = "pure_tests", ignore)]
        fn a_hook_can_be_installed_and_removed() {
            super::probe_hook().expect("a low-level keyboard hook should install");
        }
    }
}

#[cfg(windows)]
pub use imp::{probe_hook, watch};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{ListenMode, transition};
    use crate::output::keys;
    use rstest::rstest;

    /// A physical key press of `key`, as the hook would report it.
    fn pressed(name: &str, message: u32) -> HookEvent {
        let target = keys::to_windows(keys::from_name(name).expect("known key"))
            .expect("every key has an encoding");

        match target {
            WinKey::Scan(scan) => HookEvent {
                vk: 0,
                scan: u32::from(scan),
                flags: 0,
                extra: 0,
                message,
            },
            WinKey::ScanExt(scan) => HookEvent {
                vk: 0,
                scan: u32::from(scan),
                flags: LLKHF_EXTENDED,
                extra: 0,
                message,
            },
            WinKey::VirtualKey(vk) => HookEvent {
                vk: u32::from(vk),
                scan: 0,
                flags: 0,
                extra: 0,
                message,
            },
        }
    }

    fn target(name: &str) -> WinKey {
        keys::to_windows(keys::from_name(name).expect("known key")).expect("an encoding")
    }

    #[rstest]
    #[case("w", WM_KEYDOWN, Some(Edge::Down))]
    #[case("w", WM_KEYUP, Some(Edge::Up))]
    // Alt-modified keys arrive as the SYS variants, which mean the same thing.
    #[case("w", WM_SYSKEYDOWN, Some(Edge::Down))]
    #[case("w", WM_SYSKEYUP, Some(Edge::Up))]
    // An extended key, matched on its extended flag as well as its scancode.
    #[case("rightctrl", WM_KEYDOWN, Some(Edge::Down))]
    #[case("up", WM_KEYUP, Some(Edge::Up))]
    // The virtual-key row.
    #[case("pause", WM_KEYDOWN, Some(Edge::Down))]
    // Anything which is not a key transition is not our business.
    #[case("w", 0x0007, None)]
    fn the_configured_key_is_classified_by_its_message(
        #[case] name: &str,
        #[case] message: u32,
        #[case] expected: Option<Edge>,
    ) {
        assert_eq!(classify(pressed(name, message), target(name)), expected);
    }

    /// Every other key on the keyboard is discarded on its code alone.
    #[test]
    fn other_keys_are_ignored() {
        let watched = target("w");

        for &code in keys::all_codes() {
            let name = keys::name(code).expect("named key");
            if name == "w" {
                continue;
            }

            assert_eq!(
                classify(pressed(name, WM_KEYDOWN), watched),
                None,
                "'{name}' was mistaken for the hotkey"
            );
        }
    }

    /// The extended flag is half of the key's identity: left and right control
    /// share a scancode and are told apart by nothing else.
    #[rstest]
    #[case("leftctrl", "rightctrl")]
    #[case("rightctrl", "leftctrl")]
    fn the_extended_flag_distinguishes_the_two_controls(
        #[case] watched: &str,
        #[case] pressed_key: &str,
    ) {
        assert_eq!(
            classify(pressed(pressed_key, WM_KEYDOWN), target(watched)),
            None,
            "'{pressed_key}' must not match '{watched}'"
        );
        assert_eq!(
            classify(pressed(watched, WM_KEYDOWN), target(watched)),
            Some(Edge::Down)
        );
    }

    /// The keystrokes voice-orders types must never look like the hotkey: a
    /// macro bound to the same key as the hotkey would otherwise toggle
    /// listening every time it ran.
    #[test]
    fn our_own_output_is_never_the_hotkey() {
        let mut event = pressed("w", WM_KEYDOWN);
        event.extra = INJECTED_MARKER;
        event.flags |= LLKHF_INJECTED;

        assert_eq!(classify(event, target("w")), None);

        // The marker alone is enough, even if the injected flag were somehow
        // absent — and so is the flag alone, for another tool's injection.
        let mut marked = pressed("w", WM_KEYDOWN);
        marked.extra = INJECTED_MARKER;
        assert_eq!(classify(marked, target("w")), None);

        let mut injected = pressed("w", WM_KEYDOWN);
        injected.flags |= LLKHF_INJECTED;
        assert_eq!(classify(injected, target("w")), None);
    }

    /// Somebody else's `dwExtraInfo` is not ours, and must not be filtered.
    #[test]
    fn a_foreign_extra_info_does_not_hide_a_real_key_press() {
        let mut event = pressed("w", WM_KEYDOWN);
        event.extra = 0xDEAD_BEEF;

        assert_eq!(classify(event, target("w")), Some(Edge::Down));
    }

    #[test]
    fn a_held_key_repeats_rather_than_pressing_again() {
        let mut repeats = Repeats::default();

        assert_eq!(repeats.value(Edge::Down), 1, "the first press");
        assert_eq!(repeats.value(Edge::Down), 2, "Windows repeating the hold");
        assert_eq!(repeats.value(Edge::Down), 2, "and again");
        assert_eq!(repeats.value(Edge::Up), 0, "the release");
        assert_eq!(repeats.value(Edge::Down), 1, "a fresh press");
    }

    /// An `Up` we never saw the `Down` for — the hotkey was held while
    /// voice-orders started — must not be mistaken for a press.
    #[test]
    fn an_unmatched_release_is_still_a_release() {
        let mut repeats = Repeats::default();
        assert_eq!(repeats.value(Edge::Up), 0);
        assert_eq!(repeats.value(Edge::Down), 1);
    }

    /// The point of synthesizing the repeat value: fed through the shared
    /// transition, a held push-to-talk key stays held rather than flapping.
    #[test]
    fn a_held_push_to_talk_key_does_not_flap() {
        let mut repeats = Repeats::default();
        let mut listening = crate::hotkey::initial_listening(ListenMode::PushToTalk);
        assert!(!listening, "push-to-talk starts muted");

        let apply = |edge: Edge, repeats: &mut Repeats, listening: &mut bool| {
            if let Some(next) = transition(ListenMode::PushToTalk, *listening, repeats.value(edge))
            {
                *listening = next;
            }
        };

        apply(Edge::Down, &mut repeats, &mut listening);
        assert!(listening, "the press starts listening");

        for _ in 0..5 {
            apply(Edge::Down, &mut repeats, &mut listening);
            assert!(listening, "a repeat must not change anything");
        }

        apply(Edge::Up, &mut repeats, &mut listening);
        assert!(!listening, "the release stops listening");
    }

    /// Toggle mode: one press flips, its repeats and its release do not.
    #[test]
    fn toggle_flips_once_per_physical_press() {
        let mut repeats = Repeats::default();
        let mut listening = crate::hotkey::initial_listening(ListenMode::Toggle);

        for edge in [Edge::Down, Edge::Down, Edge::Down, Edge::Up] {
            if let Some(next) = transition(ListenMode::Toggle, listening, repeats.value(edge)) {
                listening = next;
            }
        }

        assert!(listening, "one press, one flip");
    }

    #[test]
    fn a_key_outside_the_table_cannot_be_watched_for() {
        let error = unwatchable_key(crate::output::KeyCode(60000));

        assert!(
            error.is(human_errors::Kind::User),
            "an unusable key is something the user can change"
        );
        assert!(
            error.description().contains("60000"),
            "the error must name the key, got: {}",
            error.description()
        );
    }
}
