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
use crate::errors::HumanizableError;
use crate::output::{KeyCode, KeySink, keys};
use tracing_batteries::prelude::*;
use uinput_tokio::event::keyboard::Keyboard as UKeyboard;

/// The name the virtual keyboard reports to the kernel, and therefore the name
/// SDL, libinput and games will see.
pub const DEVICE_NAME: &str = "voice-orders";

/// `EV_KEY`, borrowed from evdev so we do not hard-code the constant.
const EV_KEY: libc::c_int = evdev::EventType::KEY.0 as libc::c_int;

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

impl HumanizableError for uinput_tokio::Error {
    fn to_human_error(self) -> Error {
        match self {
            // `NotFound` means udev could not find the uinput device at all,
            // which almost always means the kernel module is not loaded.
            uinput_tokio::Error::NotFound => creation_error("the uinput device does not exist"),
            other => creation_error(other),
        }
    }
}

/// A [`KeySink`] backed by a `/dev/uinput` virtual keyboard.
pub struct UinputSink {
    device: uinput_tokio::Device,
}

impl UinputSink {
    /// Creates the virtual keyboard, registering the full key capability set
    /// from [`keys`].
    ///
    /// Registering every key we can ever emit (rather than only the ones a
    /// profile happens to use) is deliberate: SDL and several anti-cheat
    /// systems filter devices which do not look like real keyboards
    /// (DESIGN.md §Risks 4).
    pub async fn new() -> Result<Self, Error> {
        Self::with_name(DEVICE_NAME).await
    }

    /// Creates the virtual keyboard under a specific name. Tests use this to
    /// avoid colliding with a running instance.
    pub async fn with_name(name: &str) -> Result<Self, Error> {
        let mut builder = open_builder()?
            .name(name)
            .map_err(|e| e.to_human_error())?
            // Present as a USB keyboard; SDL is happier with a plausible bus.
            .bus(0x03)
            .vendor(0x1d6b)
            .product(0x0001)
            .version(1);

        let mut unsupported = Vec::new();
        for &code in keys::all_codes() {
            match keys::to_uinput(code) {
                Some(event) => {
                    builder = builder.event(event).map_err(|e| {
                        creation_error(format!("we could not register the '{code}' key: {e}"))
                    })?;
                }
                // `uinput-tokio` does not model this key. We can still write
                // its raw EV_KEY code, but we cannot advertise the capability,
                // so the kernel will drop the events — worth a warning.
                None => unsupported.push(code),
            }
        }

        if !unsupported.is_empty() {
            let names: Vec<String> = unsupported.iter().map(|c| c.to_string()).collect();
            warn!(
                keys = %names.join(", "),
                "uinput-tokio has no event for {} key(s); they cannot be registered as capabilities.",
                unsupported.len()
            );
        }

        let device = builder
            .create()
            .await
            .map_err(|e| creation_error(format!("the kernel rejected the device: {e}")))?;

        info!(
            device = DEVICE_NAME,
            keys = keys::all_codes().len(),
            "Created the '{name}' virtual keyboard with {} keys.",
            keys::all_codes().len()
        );

        Ok(Self { device })
    }

    /// Writes a single `EV_KEY` event, preferring the typed `uinput-tokio`
    /// event and falling back to the raw code for keys the crate does not
    /// model. This is the one place in the crate which knows that fallback
    /// exists (DESIGN.md §Risks 7).
    async fn emit(&mut self, key: KeyCode, value: libc::c_int) -> Result<(), Error> {
        let result = match keys::to_uinput(key) {
            Some(event) => self.send_typed(event, value).await,
            None => {
                self.device
                    .write(EV_KEY, key.code() as libc::c_int, value)
                    .await
            }
        };

        result.map_err(|e| write_error(format_args!("key '{key}'"), e))
    }

    async fn send_typed(
        &mut self,
        event: UKeyboard,
        value: libc::c_int,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use uinput_tokio::event::Code;
        self.device.write(EV_KEY, event.code(), value).await
    }
}

/// Opens the uinput builder, preferring udev's view of where the device lives
/// and falling back to the conventional path (udev enumeration is unavailable
/// in some containers).
fn open_builder() -> Result<uinput_tokio::device::Builder, Error> {
    match uinput_tokio::default() {
        Ok(builder) => Ok(builder),
        Err(udev_error) => {
            debug!("udev could not locate the uinput device ({udev_error}), trying /dev/uinput.");
            uinput_tokio::open("/dev/uinput").map_err(|e| e.to_human_error())
        }
    }
}

/// Failures once the device exists are not the user's fault — the device
/// vanished, or the kernel buffer misbehaved.
fn write_error(what: std::fmt::Arguments<'_>, detail: Box<dyn std::error::Error>) -> Error {
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
        self.emit(key, 1).await
    }

    async fn release(&mut self, key: KeyCode) -> Result<(), Error> {
        self.emit(key, 0).await
    }

    async fn synchronize(&mut self) -> Result<(), Error> {
        self.device
            .synchronize()
            .await
            .map_err(|e| write_error(format_args!("a synchronization event"), e))
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
        let message = uinput_tokio::Error::NotFound.to_human_error().to_string();
        assert!(message.contains("does not exist"), "got: {message}");
        assert!(message.contains(r#"KERNEL=="uinput""#), "got: {message}");
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
