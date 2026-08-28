//! The terminal UI both `test` and `run` render, and the single event stream
//! which feeds it. See DESIGN.md §"The session terminal UI (ratatui)".
//!
//! The two commands report the same things — an utterance, the command it
//! matched, a hotkey mute, a failure — so they say them the same way: everything
//! travels as one [`UiEvent`] (`event.rs`) and is consumed by exactly one
//! renderer, chosen once by [`ReportMode`]:
//!
//! - the **terminal UI** (`tui.rs`) when stdout is an interactive terminal; and
//! - the **plain line-printed report** otherwise, unchanged to the character,
//!   because piped output is something scripts and CI already read — and for
//!   `run`, a non-TTY launch (Steam, a pipe, CI) is the wrapper contract itself.
//!
//! What differs between the commands is only what they *put into* the stream:
//! `test` reports what it would have typed, `run` reports what it typed and the
//! lines its wrapped application printed.

use tokio::sync::mpsc;

pub(super) mod event;
pub(super) mod plan;
pub(super) mod tui;

pub(super) use event::{EventSink, UiEvent};
pub(super) use plan::render_plan;

/// How a session reports itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportMode {
    /// The full-screen terminal UI.
    Tui,
    /// One line per event on stdout.
    Plain,
}

impl ReportMode {
    /// The UI is only ever taken out when stdout is a terminal we own: piped
    /// output (a script, a CI job, `| head`, a Steam launch) gets the plain
    /// report, because escape sequences and an alternate screen are worse than
    /// useless there.
    ///
    /// Taken as an argument rather than read here so the choice — the one part
    /// of the decision which could ever be wrong — is testable.
    pub(crate) fn of(stdout_is_terminal: bool) -> Self {
        if stdout_is_terminal {
            ReportMode::Tui
        } else {
            ReportMode::Plain
        }
    }

    /// Where events go under this mode, and the receiving end when there is a
    /// UI to hand them to.
    pub(crate) fn sink(self) -> (EventSink, Option<mpsc::UnboundedReceiver<UiEvent>>) {
        match self {
            ReportMode::Plain => (EventSink::Plain, None),
            ReportMode::Tui => {
                let (events_tx, events_rx) = mpsc::unbounded_channel();
                (EventSink::Channel(events_tx), Some(events_rx))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // A terminal we own: the full-screen UI DESIGN.md describes.
    #[case(true, ReportMode::Tui)]
    // Piped, redirected, launched by Steam, or running under CI: the plain
    // report, because that is what scripts (and the tests) read.
    #[case(false, ReportMode::Plain)]
    fn test_the_report_mode_follows_stdout(
        #[case] is_terminal: bool,
        #[case] expected: ReportMode,
    ) {
        assert_eq!(ReportMode::of(is_terminal), expected);
    }

    #[test]
    fn test_only_the_terminal_ui_gets_an_event_channel() {
        // Plain mode prints from wherever the event was reported, so there is
        // nothing to receive and no UI task to start.
        let (sink, ui) = ReportMode::Plain.sink();
        assert!(matches!(sink, EventSink::Plain));
        assert!(ui.is_none(), "plain mode has no UI to hand events to");

        let (sink, ui) = ReportMode::Tui.sink();
        assert!(matches!(sink, EventSink::Channel(_)));
        let mut ui = ui.expect("the UI needs the receiving end");

        sink.send(UiEvent::Heard("salute".to_string()));
        assert_eq!(
            ui.try_recv(),
            Ok(UiEvent::Heard("salute".to_string())),
            "everything reported should reach the UI"
        );
    }
}
