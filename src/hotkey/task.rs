//! The hotkey task: turns an evdev device into an async event stream and
//! publishes listening-state changes onto the `watch` channel which the audio
//! callback and the matcher observe. See DESIGN.md §"Runtime pipeline".

use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use super::{ListenMode, transition};
use crate::errors::HumanizableError;
use crate::output::KeyCode;

/// Watches `device` for the configured hotkey and publishes listening-state
/// changes onto `listening` until `cancel` fires.
///
/// **Privacy:** only events for `key` are examined. Everything else — every
/// other key on the keyboard, every mouse movement, every LED and sync event —
/// is discarded on the event type and code alone, without its value being read
/// or logged. See DESIGN.md §"Risks" 3.
///
/// A device which disappears mid-session (an unplugged keyboard) ends the task
/// with a warning rather than an error: losing the hotkey should never take a
/// game session down with it. The listening state simply stays where it was.
pub async fn hotkey_task(
    device: evdev::Device,
    key: KeyCode,
    mode: ListenMode,
    listening: tokio::sync::watch::Sender<bool>,
    cancel: CancellationToken,
) -> Result<(), crate::Error> {
    let name = device.name().unwrap_or("<unnamed device>").to_string();

    let mut events = device
        .into_event_stream()
        .map_err(|e| e.to_human_error())
        .map_err(|e| {
            human_errors::wrap_user(
                e,
                format!("We could not start watching '{name}' for your listen hotkey."),
                &[
                    "Make sure the device still exists and that you are a member of the 'input' group.",
                ],
            )
        })?;

    debug!(
        "Listening for the {mode} hotkey (evdev code {}) on '{name}'.",
        key.0
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("The hotkey task on '{name}' is shutting down.");
                return Ok(());
            }
            event = events.next_event() => {
                let event = match event {
                    Ok(event) => event,
                    Err(e) => {
                        // Errors from this stream are terminal: the fd is gone
                        // or unreadable, and every subsequent read will fail
                        // the same way.
                        warn!(
                            "We stopped receiving events from '{name}' ({e}); your listen hotkey will no longer work, but everything else keeps running."
                        );
                        return Ok(());
                    }
                };

                // Discard everything which is not the configured hotkey
                // *before* looking at its value.
                if event.event_type() != evdev::EventType::KEY || event.code() != key.0 {
                    continue;
                }

                let current = *listening.borrow();
                if let Some(next) = transition(mode, current, event.value()) {
                    debug!("The listen hotkey turned listening {}.", if next { "on" } else { "off" });
                    // `send_replace` rather than `send`: the state change must
                    // land even in the window where no receiver happens to be
                    // alive, and shutdown is the cancellation token's job.
                    listening.send_replace(next);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{discover_device, initial_listening};

    /// The task must return promptly when the token is cancelled, leaving the
    /// listening state untouched. Needs a real device to build a stream from.
    #[tokio::test]
    #[cfg_attr(feature = "pure_tests", ignore)]
    async fn test_hotkey_task_stops_on_cancel() {
        const KEY_LEFTCTRL: u16 = 29;
        let key = KeyCode(KEY_LEFTCTRL);

        let Ok(device) = discover_device("auto", key) else {
            // No readable keyboard here (a CI container, or no `input` group
            // membership); discovery itself is covered by its own tests.
            return;
        };

        let mode = ListenMode::Toggle;
        let (tx, rx) = tokio::sync::watch::channel(initial_listening(mode));
        let cancel = CancellationToken::new();

        let task = tokio::spawn(hotkey_task(device, key, mode, tx, cancel.clone()));
        cancel.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("the hotkey task should stop promptly once cancelled")
            .expect("the hotkey task should not panic");

        result.expect("a cancelled hotkey task should exit cleanly");
        assert!(
            !*rx.borrow(),
            "cancelling should not have changed the listening state"
        );
    }
}
