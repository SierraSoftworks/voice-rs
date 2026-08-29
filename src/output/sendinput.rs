//! The Windows keyboard sink: a [`KeySink`] which plays compiled key plans
//! through the `SendInput` API.
//!
//! This is the Windows counterpart of [`crate::output::uinput`], and it is
//! deliberately the *simpler* of the two: `SendInput` needs no device, no
//! kernel module and no group membership, so there is nothing to open and
//! nothing to ask the user to configure. What it does need is an encoding, and
//! that lives in the key table already ([`crate::output::keys::WinKey`]).
//!
//! **Scancodes, not virtual keys.** Every row but one is injected with
//! `KEYEVENTF_SCANCODE`: a game reading raw input sees a scancode where it would
//! ignore a synthesized virtual key, and a scancode is layout-independent — `w`
//! is the key above `s` on AZERTY too, which is what a movement macro means
//! (DESIGN.md §"Windows support").
//!
//! **`wVk` is left at zero for scancode events**, deliberately. It would be
//! cheap to fill in with `MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)`, and it was
//! considered, but that translation is done against *our* keyboard layout: on an
//! AZERTY machine the `w` scancode maps to `VK_Z`, so filling the field would
//! put a layout-dependent value into a record whose whole point is to be
//! layout-independent. It is also unnecessary — when Windows turns a
//! `KEYEVENTF_SCANCODE` record into a `WM_KEYDOWN` it derives `wParam` from the
//! scancode using the *receiving* thread's layout, so an application which reads
//! virtual keys already gets a correct one. Leaving the field alone additionally
//! keeps [`keystroke`] a pure function of the key table, which is what lets the
//! encoding be unit-tested on the platform this project is developed on.
//!
//! The pure half of this module — [`KeyStroke`] and [`keystroke`] — therefore
//! compiles everywhere, exactly as the key table's Windows column does. Only
//! [`WinKeySink`] and the syscall it wraps are `cfg(windows)`.

use crate::output::{KeyCode, keys};

/// The `dwExtraInfo` marker every event we inject carries.
///
/// The listen hotkey watches the whole system with a low-level keyboard hook
/// ([`crate::hotkey`]), which means it sees our own output as well as the user's
/// typing. Stamping every injected event with a value nothing else uses is what
/// lets the hook tell the two apart and ignore its own tail: without it, a macro
/// which types the hotkey would toggle listening.
///
/// The value is ASCII `"voic"`, and stays inside 32 bits so it is the same
/// marker on a 32-bit build.
pub const INJECTED_MARKER: usize = 0x766f_6963;

/// Which way a key is moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The key is being pressed.
    Down,
    /// The key is being released.
    Up,
}

// The three `KEYBD_EVENT_FLAGS` we use, mirrored as plain integers so that the
// encoding compiles (and is testable) off Windows. A `cfg(windows)` test pins
// each one against the `windows-sys` constant it copies, which is the same trick
// the key table uses for its evdev codes.
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;

/// One `KEYBDINPUT` record, as plain integers.
///
/// Field-for-field the payload of the `INPUT` we hand to `SendInput`, minus the
/// Windows type aliases, so that building it is ordinary arithmetic which any
/// platform can run and any platform can test. `time` is not modelled: it is
/// always zero, which asks Windows to timestamp the event itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    /// The virtual-key code, or `0` for a scancode event.
    pub vk: u16,
    /// The AT set-1 scancode, or `0` for a virtual-key event.
    pub scan: u16,
    /// The `KEYEVENTF_*` flags describing the record.
    pub flags: u32,
    /// Our own [`INJECTED_MARKER`], so the hotkey hook can ignore this event.
    pub extra: usize,
}

/// How `key` should be handed to `SendInput` to move it in `direction`.
///
/// `None` means the key has no Windows encoding at all. No row of the current
/// table is like that — a test asserts as much — but [`keys::to_windows`] admits
/// the possibility, so the sink turns it into an error which names the key
/// rather than pressing something arbitrary.
pub fn keystroke(key: KeyCode, direction: Direction) -> Option<KeyStroke> {
    let up = match direction {
        Direction::Down => 0,
        Direction::Up => KEYEVENTF_KEYUP,
    };

    Some(match keys::to_windows(key)? {
        keys::WinKey::Scan(scan) => KeyStroke {
            vk: 0,
            scan,
            flags: KEYEVENTF_SCANCODE | up,
            extra: INJECTED_MARKER,
        },
        keys::WinKey::ScanExt(scan) => KeyStroke {
            vk: 0,
            scan,
            flags: KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | up,
            extra: INJECTED_MARKER,
        },
        // `pause` only: its scancode is the multi-byte `E1 1D 45` sequence,
        // which a single `wScan` cannot carry, so it goes by virtual key with no
        // `KEYEVENTF_SCANCODE` flag. `wScan` stays zero for the same reason.
        keys::WinKey::VirtualKey(vk) => KeyStroke {
            vk,
            scan: 0,
            flags: up,
            extra: INJECTED_MARKER,
        },
    })
}

#[cfg(windows)]
mod imp {
    use super::{Direction, KeyStroke, keystroke};
    use crate::Error;
    use crate::output::{KeyCode, KeySink};
    use std::collections::HashSet;
    use tracing_batteries::prelude::*;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, SendInput,
    };

    /// Advice for the one thing which can go wrong once we are running.
    const BLOCKED_ADVICE: &[&str] = &[
        "Windows only lets a program send keystrokes to windows running at the same integrity level or lower, so an elevated game ignores input from an ordinary program.",
        "If the application you are controlling runs as administrator, start voice-orders as administrator too (right-click it and choose 'Run as administrator', or launch it from an elevated terminal).",
        "Anti-cheat software and 'block input' calls from other programs can also refuse injected keystrokes; see https://sierrasoftworks.github.io/voice-rs/ for what is known to interfere.",
    ];

    impl KeyStroke {
        /// The `INPUT` record this stroke describes.
        pub(super) fn to_input(self) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: self.vk,
                        wScan: self.scan,
                        dwFlags: self.flags,
                        // Zero asks Windows to timestamp the event for us.
                        time: 0,
                        dwExtraInfo: self.extra,
                    },
                },
            }
        }
    }

    /// A [`KeySink`] backed by Windows' `SendInput`.
    ///
    /// Holds nothing but the set of keys it has pressed and not yet released,
    /// which it releases on drop. The executor already guarantees that
    /// ([`crate::output::executor`] releases everything it holds on its way
    /// out), and this is the belt to that pair of braces: a voice macro must
    /// never leave `W` held down in a game.
    pub struct WinKeySink {
        held: HashSet<KeyCode>,
    }

    impl WinKeySink {
        /// Creates the Windows key sink.
        ///
        /// Cannot fail: `SendInput` injects into the session's input queue and
        /// needs no device, no driver and no permission to be granted up front.
        /// The signature still matches the Linux sink's — fallible and `async` —
        /// because that is what keeps the `run` assembly platform-neutral.
        pub async fn new() -> Result<Self, Error> {
            debug!(
                "Keyboard output will be injected with SendInput; Windows needs no virtual device for it."
            );

            Ok(Self {
                held: HashSet::new(),
            })
        }

        /// Injects one key transition, as a single `SendInput` batch.
        fn emit(&self, key: KeyCode, direction: Direction) -> Result<(), Error> {
            let input = keystroke(key, direction)
                .ok_or_else(|| unsupported_key(key))?
                .to_input();

            // SAFETY: `SendInput` is handed exactly one correctly sized `INPUT`
            // record, which lives for the duration of the call and is not
            // retained by Windows afterwards.
            let sent = unsafe {
                SendInput(
                    1,
                    std::ptr::from_ref(&input),
                    std::mem::size_of::<INPUT>() as i32,
                )
            };

            if sent == 1 {
                return Ok(());
            }

            // SAFETY: `GetLastError` reads this thread's own last-error value.
            Err(blocked(key, unsafe { GetLastError() }))
        }
    }

    /// A key the executor asked for which `SendInput` has no encoding for.
    ///
    /// Unreachable with the current key table (a test in `keys.rs` proves every
    /// row has an encoding), but the table is data and this is what a future gap
    /// in it would say.
    fn unsupported_key(key: KeyCode) -> Error {
        human_errors::user(
            format!(
                "We cannot press the '{key}' key on Windows, because it has no scancode or virtual-key code we can send."
            ),
            &[
                "Choose a different key for this command, or run 'voice-orders keys' to see the keys we can press.",
                "Please report this issue on GitHub so that we can add the missing key.",
            ],
        )
    }

    /// Windows refused to accept the keystroke.
    ///
    /// Overwhelmingly this is UIPI: a program may only inject input into windows
    /// running at its own integrity level or below, so an ordinary voice-orders
    /// cannot type into an elevated game. The error says so, because "nothing
    /// happened in the game" is otherwise an impossible thing to debug.
    fn blocked(key: KeyCode, last_error: u32) -> Error {
        human_errors::user(
            format!(
                "Windows would not let us press the '{key}' key (error {last_error}): our keystrokes are being blocked before they reach any application."
            ),
            BLOCKED_ADVICE,
        )
    }

    impl KeySink for WinKeySink {
        async fn press(&mut self, key: KeyCode) -> Result<(), Error> {
            self.emit(key, Direction::Down)?;
            self.held.insert(key);
            Ok(())
        }

        async fn release(&mut self, key: KeyCode) -> Result<(), Error> {
            let result = self.emit(key, Direction::Up);
            // Forget the key either way: a release we could not send is not a
            // key we can usefully try to release again on the way out.
            self.held.remove(&key);
            result
        }

        /// Nothing to flush.
        ///
        /// `SendInput` inserts its whole batch into the input stream atomically,
        /// so there is no `EV_SYN` equivalent to send. The method stays in the
        /// trait because dropping it would change the sequence uinput emits,
        /// which is pinned by tests and is what a game actually sees
        /// (DESIGN.md §"Rejected compromises").
        async fn synchronize(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl Drop for WinKeySink {
        fn drop(&mut self) {
            if self.held.is_empty() {
                return;
            }

            // Ascending code order, so the emitted sequence is deterministic
            // regardless of `HashSet` iteration order — the same rule
            // `release_all` follows.
            let mut pending: Vec<KeyCode> = self.held.drain().collect();
            pending.sort_unstable();

            warn!(
                keys = pending.len(),
                "Releasing {} key(s) which were still held down when keyboard output shut down.",
                pending.len()
            );

            for key in pending {
                if let Err(e) = self.emit(key, Direction::Up) {
                    warn!("We could not release the '{key}' key on shutdown: {e}");
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::{Direction, INJECTED_MARKER, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE};
        use super::*;
        use crate::output::keys;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            KEYEVENTF_EXTENDEDKEY as WIN_EXTENDEDKEY, KEYEVENTF_KEYUP as WIN_KEYUP,
            KEYEVENTF_SCANCODE as WIN_SCANCODE,
        };

        /// The plain-integer mirrors of the flags must be the flags.
        #[test]
        fn the_mirrored_flags_are_the_windows_ones() {
            assert_eq!(super::super::KEYEVENTF_EXTENDEDKEY, WIN_EXTENDEDKEY);
            assert_eq!(KEYEVENTF_KEYUP, WIN_KEYUP);
            assert_eq!(KEYEVENTF_SCANCODE, WIN_SCANCODE);
        }

        #[test]
        fn a_stroke_becomes_a_keyboard_input_record() {
            let key = keys::from_name("w").expect("known key");
            let stroke = keystroke(key, Direction::Down).expect("w has an encoding");
            let input = stroke.to_input();

            assert_eq!(input.r#type, INPUT_KEYBOARD);

            // SAFETY: the record was built as a keyboard event a line ago.
            let ki = unsafe { input.Anonymous.ki };
            assert_eq!(ki.wVk, 0);
            assert_eq!(ki.wScan, 0x11);
            assert_eq!(ki.dwFlags, WIN_SCANCODE);
            assert_eq!(ki.time, 0);
            assert_eq!(ki.dwExtraInfo, INJECTED_MARKER);
        }

        /// The real API, on the real input queue: `f24` is the key nothing is
        /// bound to, which is why the Linux uinput probe uses it too.
        #[tokio::test]
        #[cfg_attr(feature = "pure_tests", ignore)]
        async fn presses_a_real_key() {
            let mut sink = WinKeySink::new().await.expect("SendInput needs no device");
            let key = keys::from_name("f24").expect("known key");

            sink.press(key).await.expect("press f24");
            sink.synchronize().await.expect("synchronize");
            sink.release(key).await.expect("release f24");
            sink.synchronize().await.expect("synchronize");
        }
    }
}

#[cfg(windows)]
pub use imp::WinKeySink;

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn key(name: &str) -> KeyCode {
        keys::from_name(name).expect("known key")
    }

    #[rstest]
    // A plain scancode: `w` is 0x11, and nothing but KEYEVENTF_SCANCODE.
    #[case("w", Direction::Down, 0, 0x11, KEYEVENTF_SCANCODE)]
    #[case("w", Direction::Up, 0, 0x11, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP)]
    #[case("a", Direction::Down, 0, 0x1E, KEYEVENTF_SCANCODE)]
    #[case("f24", Direction::Down, 0, 0x6F, KEYEVENTF_SCANCODE)]
    // An E0-prefixed scancode additionally needs KEYEVENTF_EXTENDEDKEY.
    #[case(
        "up",
        Direction::Down,
        0,
        0x48,
        KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY
    )]
    #[case(
        "rightctrl",
        Direction::Up,
        0,
        0x1D,
        KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
    )]
    // The one virtual-key row: no scancode, and no KEYEVENTF_SCANCODE flag.
    #[case("pause", Direction::Down, 0x13, 0, 0)]
    #[case("pause", Direction::Up, 0x13, 0, KEYEVENTF_KEYUP)]
    fn strokes_carry_the_encoding_the_key_table_gives(
        #[case] name: &str,
        #[case] direction: Direction,
        #[case] vk: u16,
        #[case] scan: u16,
        #[case] flags: u32,
    ) {
        assert_eq!(
            keystroke(key(name), direction),
            Some(KeyStroke {
                vk,
                scan,
                flags,
                extra: INJECTED_MARKER,
            })
        );
    }

    /// Every key we can be asked to press must produce a record, and every
    /// record must carry the marker which stops the hotkey hook reacting to our
    /// own typing.
    #[test]
    fn every_key_encodes_and_is_marked_as_ours() {
        for &code in keys::all_codes() {
            for direction in [Direction::Down, Direction::Up] {
                let stroke = keystroke(code, direction)
                    .unwrap_or_else(|| panic!("'{code}' has no SendInput encoding"));

                assert_eq!(
                    stroke.extra, INJECTED_MARKER,
                    "'{code}' was injected without our marker"
                );
                assert!(
                    stroke.vk != 0 || stroke.scan != 0,
                    "'{code}' would press nothing at all"
                );
                assert_eq!(
                    stroke.flags & KEYEVENTF_KEYUP != 0,
                    direction == Direction::Up,
                    "'{code}' has the wrong direction flag"
                );
            }
        }
    }

    /// A scancode event never claims a virtual key: see the module docs for why
    /// filling `wVk` would be a layout-dependent lie.
    #[test]
    fn scancode_events_leave_the_virtual_key_alone() {
        for &code in keys::all_codes() {
            let stroke = keystroke(code, Direction::Down).expect("every key encodes");
            if stroke.flags & KEYEVENTF_SCANCODE != 0 {
                assert_eq!(stroke.vk, 0, "'{code}' filled in a virtual key");
            } else {
                assert_ne!(stroke.vk, 0, "'{code}' has neither a scancode nor a key");
            }
        }
    }

    #[test]
    fn a_key_outside_the_table_has_no_encoding() {
        assert_eq!(keystroke(KeyCode(60000), Direction::Down), None);
    }
}
