//! The virtual keyboard: a `/dev/uinput` device which the executor plays
//! compiled key plans onto.
//!
//! Working below the display server is what makes voice-orders behave
//! identically on X11, Wayland and inside fullscreen games. The cost is that
//! the device has to be created up-front (DESIGN.md §"`run` assembly" step 3)
//! and that failures are almost always permission problems — so this module
//! spends most of its lines turning those into an error which tells you exactly
//! which udev rule to write.

use crate::Error;
use crate::output::{KeyCode, KeySink, keys};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId};
use tracing_batteries::prelude::*;

/// The name the virtual keyboard reports to the kernel, and therefore the name
/// SDL, libinput and games will see.
pub const DEVICE_NAME: &str = "voice-orders";

/// The longest device name the kernel accepts (`UINPUT_MAX_NAME_SIZE` is 80,
/// including the terminating NUL). `evdev` asserts on longer names, so we turn
/// that into an error before it gets the chance.
const MAX_NAME_LEN: usize = 78;

/// Advice attached to every failure to create the virtual keyboard.
const CREATION_ADVICE: &[&str] = &[
    "Load the uinput kernel module with 'sudo modprobe uinput' (and add it to /etc/modules-load.d/ to make that persist).",
    "Add yourself to the 'input' group with 'sudo usermod -aG input $USER', then log out and back in for it to take effect.",
    "See the permissions section of the installation guide for the full walkthrough.",
];

/// The udev rule which grants the `input` group access to `/dev/uinput`.
const UDEV_RULE: &str = r#"KERNEL=="uinput", GROUP="input", MODE="0660""#;

/// Builds the "we could not create the virtual keyboard" message, which carries
/// the exact udev rule because that is the single thing people need to copy.
fn creation_error(detail: impl std::fmt::Display) -> Error {
    human_errors::user(
        format!(
            "We could not create the virtual keyboard on /dev/uinput ({detail}). \
             voice-orders needs write access to /dev/uinput to press keys for you: \
             create /etc/udev/rules.d/99-uinput.rules containing {UDEV_RULE} \
             (then 'sudo udevadm control --reload-rules && sudo udevadm trigger'), \
             and make sure your user is a member of the 'input' group — the same \
             group grants the /dev/input/event* access the listen hotkey needs."
        ),
        CREATION_ADVICE,
    )
}

/// Humanizes a failure to open `/dev/uinput` itself.
///
/// A missing node almost always means the kernel module is not loaded; every
/// other failure (most often `EACCES`) is carried through verbatim.
fn open_error(error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => creation_error("the uinput device does not exist"),
        _ => creation_error(error),
    }
}

/// The capability set the virtual keyboard advertises: every key in [`keys`].
///
/// Registering every key we can ever emit (rather than only the ones a profile
/// happens to use) is deliberate: SDL and several anti-cheat systems filter
/// devices which do not look like real keyboards (DESIGN.md §Risks 4).
fn key_capabilities() -> AttributeSet<evdev::KeyCode> {
    keys::all_codes()
        .iter()
        .copied()
        .map(keys::to_evdev)
        .collect()
}

/// A [`KeySink`] backed by a `/dev/uinput` virtual keyboard.
pub struct UinputSink {
    device: VirtualDevice,
    /// Key events written since the last [`KeySink::synchronize`].
    ///
    /// `evdev` only exposes a write which terminates its batch with a
    /// `SYN_REPORT`, so events are held here and written as one batch when the
    /// executor synchronizes. That keeps the event stream the kernel sees
    /// identical to writing each event and then a separate `SYN_REPORT`: a
    /// chord pressed before a synchronize still lands in a single report.
    pending: Vec<InputEvent>,
}

impl UinputSink {
    /// Creates the virtual keyboard, registering the full key capability set
    /// from [`keys`].
    pub async fn new() -> Result<Self, Error> {
        Self::with_name(DEVICE_NAME).await
    }

    /// Creates the virtual keyboard under a specific name. Tests use this to
    /// avoid colliding with a running instance.
    pub async fn with_name(name: &str) -> Result<Self, Error> {
        if name.len() > MAX_NAME_LEN {
            return Err(creation_error(format!(
                "the device name '{name}' is longer than the kernel's {MAX_NAME_LEN} byte limit"
            )));
        }

        let capabilities = key_capabilities();

        let device = VirtualDevice::builder()
            .map_err(open_error)?
            .name(name)
            // Present as a USB keyboard; SDL is happier with a plausible bus.
            .input_id(InputId::new(BusType::BUS_USB, 0x1d6b, 0x0001, 1))
            .with_keys(&capabilities)
            .map_err(|e| creation_error(format!("we could not register the keyboard's keys: {e}")))?
            .build()
            .map_err(|e| creation_error(format!("the kernel rejected the device: {e}")))?;

        info!(
            device = DEVICE_NAME,
            keys = keys::all_codes().len(),
            "Created the '{name}' virtual keyboard with {} keys.",
            keys::all_codes().len()
        );

        Ok(Self {
            device,
            pending: Vec::new(),
        })
    }

    /// Queues a single `EV_KEY` event for the next synchronize.
    ///
    /// Every key in the table is registered as a capability (see
    /// [`key_capabilities`]), so there is no need for a raw-code fallback for
    /// keys a typed event enum fails to model (DESIGN.md §Risks 7).
    fn queue(&mut self, key: KeyCode, value: i32) {
        self.pending
            .push(InputEvent::new(EventType::KEY.0, key.code(), value));
    }
}

/// Failures once the device exists are not the user's fault — the device
/// vanished, or the kernel buffer misbehaved.
fn write_error(what: std::fmt::Arguments<'_>, detail: std::io::Error) -> Error {
    human_errors::system(
        format!("We were unable to send {what} to the virtual keyboard ({detail})."),
        &[
            "This usually means the virtual keyboard was removed while voice-orders was running.",
            "Please report this issue on GitHub so that we can investigate.",
        ],
    )
}

impl KeySink for UinputSink {
    async fn press(&mut self, key: KeyCode) -> Result<(), Error> {
        self.queue(key, 1);
        Ok(())
    }

    async fn release(&mut self, key: KeyCode) -> Result<(), Error> {
        self.queue(key, 0);
        Ok(())
    }

    async fn synchronize(&mut self) -> Result<(), Error> {
        // `emit` writes the queued events followed by a `SYN_REPORT`. The
        // write is a single non-blocking `write(2)` to the uinput node, so it
        // is done inline rather than on a blocking thread.
        let result = self.device.emit(&self.pending);
        let count = self.pending.len();
        self.pending.clear();

        result.map_err(|e| {
            write_error(
                format_args!("{count} key event(s) and a synchronization event"),
                e,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_errors_spell_out_the_udev_rule() {
        let error = creation_error("Permission denied (os error 13)");
        let message = error.description();

        assert!(
            message.contains(r#"KERNEL=="uinput", GROUP="input", MODE="0660""#),
            "the exact udev rule must be in the message, got: {message}"
        );
        assert!(
            message.contains("'input' group"),
            "the input group requirement must be in the message, got: {message}"
        );
        assert!(
            message.contains("Permission denied (os error 13)"),
            "the underlying cause must be in the message, got: {message}"
        );
        assert!(
            error.is(human_errors::Kind::User),
            "permission problems are actionable by the user"
        );
    }

    #[test]
    fn not_found_is_humanized() {
        let message = open_error(std::io::ErrorKind::NotFound.into()).to_string();
        assert!(message.contains("does not exist"), "got: {message}");
        assert!(message.contains(r#"KERNEL=="uinput""#), "got: {message}");
    }

    #[test]
    fn every_key_is_registered_as_a_capability() {
        // DESIGN.md §Risks 7: every key in the table must be advertised, or the
        // kernel would silently drop its events.
        let capabilities = key_capabilities();
        let missing: Vec<_> = keys::all_codes()
            .iter()
            .filter(|&&code| !capabilities.contains(keys::to_evdev(code)))
            .map(|code| code.to_string())
            .collect();
        assert!(missing.is_empty(), "not registered: {missing:?}");
        assert_eq!(capabilities.iter().count(), keys::all_codes().len());
    }

    #[test]
    fn permission_errors_are_humanized() {
        let error = open_error(std::io::ErrorKind::PermissionDenied.into());
        assert!(error.is(human_errors::Kind::User));
        assert!(error.description().contains(r#"KERNEL=="uinput""#));
    }

    /// Exercises the real device. `/dev/uinput` is frequently unavailable (no
    /// module, no permissions, containerized CI), so this is gated the same way
    /// the Vosk-model tests are.
    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn creates_a_real_virtual_keyboard() {
        let mut sink = match UinputSink::with_name("voice-orders-test").await {
            Ok(sink) => sink,
            Err(e) => {
                eprintln!("skipping: /dev/uinput is unavailable here: {e}");
                return;
            }
        };

        let key = keys::from_name("f24").expect("known key");
        sink.press(key).await.expect("press f24");
        sink.synchronize().await.expect("synchronize");
        sink.release(key).await.expect("release f24");
        sink.synchronize().await.expect("synchronize");
    }
}
