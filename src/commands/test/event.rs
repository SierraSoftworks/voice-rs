//! The single stream of things a rehearsal has to say, and the two ways of
//! saying them. See DESIGN.md §"The `test` terminal UI (ratatui)".
//!
//! Everything the pipeline reports during `voice-orders test` — an utterance it
//! heard, a command it matched, a hotkey mute, a failure — becomes one
//! [`TestEvent`]. Exactly one renderer consumes them:
//!
//! - [`EventSink::Plain`] prints the line-per-event report `test` has always
//!   printed, character for character, so piped output and scripts are
//!   unaffected; and
//! - [`EventSink::Channel`] hands them to the terminal UI, which owns the
//!   screen and draws each one as a colored dot with a timestamp.
//!
//! Unifying them here is what keeps the two modes honest: there is one place
//! which decides what an event *says*, and the mode only decides how it looks.

use std::collections::VecDeque;
use std::time::SystemTime;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

/// How many events the terminal UI keeps in its scrollback.
///
/// A rehearsal can be left running for an entire game session, so the log has
/// to be bounded: the oldest event is dropped once this many are held.
pub(crate) const SCROLLBACK: usize = 1000;

/// The dot which leads every line in the terminal UI's log.
pub(crate) const DOT: &str = "●";

/// Everything a rehearsal reports, from whichever part of the pipeline saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TestEvent {
    /// A finalized utterance, as the matcher receives it.
    Heard(String),
    /// A partial result on its way to becoming an utterance
    /// (`--debug-recognition` only).
    Hearing(String),
    /// The recognizer was reset because the hotkey muted us
    /// (`--debug-recognition` only).
    Muted,
    /// A command matched, with the key plan `run` would have played.
    Matched { name: String, plan: String },
    /// A command in flight was cut short because listening stopped.
    Interrupted(String),
    /// A command the matcher had already queued was thrown away unplayed.
    Discarded(String),
    /// The hotkey changed the listening state.
    Listening(bool),
    /// Something in the pipeline failed in a way the user should see.
    Error(String),
}

impl TestEvent {
    /// The line this event prints in plain mode.
    ///
    /// These strings are load-bearing: they are `test`'s piped output, which
    /// scripts and CI parse, so they must not drift.
    pub(crate) fn plain_line(&self) -> String {
        match self {
            TestEvent::Heard(text) => format!("heard: {text:?}"),
            TestEvent::Hearing(text) => format!("hearing: {text:?}"),
            TestEvent::Muted => "hearing: (muted)".to_string(),
            TestEvent::Matched { name, plan } => format!("matched: {name:?} → {plan}"),
            TestEvent::Interrupted(name) => format!("interrupted: {name:?}"),
            TestEvent::Discarded(name) => format!("discarded: {name:?}"),
            TestEvent::Listening(true) => "listening: on".to_string(),
            TestEvent::Listening(false) => "listening: off".to_string(),
            TestEvent::Error(message) => format!("error: {message}"),
        }
    }

    /// The color of the dot this event leads its log line with.
    ///
    /// A heard utterance is grey because at the moment it is reported nothing
    /// has matched it yet: a grey line with no green line under it is exactly
    /// the "it recognized me but nothing fired" case a rehearsal exists to
    /// find — the same reading as the plain report's `heard:` with no
    /// `matched:` after it.
    pub(crate) fn color(&self) -> Color {
        match self {
            TestEvent::Matched { .. } => Color::Green,
            TestEvent::Heard(_) => Color::Gray,
            TestEvent::Hearing(_) | TestEvent::Muted => Color::DarkGray,
            TestEvent::Interrupted(_) | TestEvent::Discarded(_) => Color::Yellow,
            TestEvent::Error(_) => Color::Red,
            TestEvent::Listening(_) => Color::Blue,
        }
    }

    /// The style the event's text itself is drawn in; the dot carries the
    /// severity, so only the quietest and loudest events restyle their text.
    fn text_style(&self) -> Style {
        match self {
            TestEvent::Hearing(_) | TestEvent::Muted => Style::new().fg(Color::DarkGray),
            TestEvent::Error(_) => Style::new().fg(Color::Red),
            _ => Style::new(),
        }
    }

    /// This event as one styled log line: `HH:MM:SS ● <the plain text>`.
    ///
    /// The text is deliberately the same as the plain report's, so the two
    /// modes never disagree about what happened and the dot is a redundant
    /// (rather than the only) cue — colors alone are not something a report
    /// should depend on.
    pub(crate) fn line(&self, at: SystemTime) -> Line<'static> {
        Line::from(vec![
            Span::styled(clock_time(at), Style::new().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::styled(DOT, Style::new().fg(self.color())),
            Span::raw(" "),
            Span::styled(self.plain_line(), self.text_style()),
        ])
    }
}

/// Where a rehearsal's events go.
///
/// Cloned into every part of the pipeline which reports something, so the
/// mode is chosen once (in [`super::ReportMode`]) and never again.
#[derive(Debug, Clone)]
pub(crate) enum EventSink {
    /// Print the plain report to stdout, exactly as `test` always has. This is
    /// also what `run --debug-recognition` uses, so its output is unchanged.
    Plain,
    /// Hand the event to whoever owns the terminal (the UI task).
    Channel(mpsc::UnboundedSender<TestEvent>),
}

impl EventSink {
    /// Reports one event.
    ///
    /// Infallible and non-blocking by construction: a rehearsal must never be
    /// slowed down (or, worse, deadlocked) by its own reporting, so a UI which
    /// has already gone away simply stops receiving events.
    pub(crate) fn send(&self, event: TestEvent) {
        match self {
            EventSink::Plain => println!("{}", event.plain_line()),
            EventSink::Channel(events) => {
                let _ = events.send(event);
            }
        }
    }
}

/// The terminal UI's bounded scrollback: the last [`SCROLLBACK`] events, each
/// with the wall-clock time it was reported.
pub(crate) struct EventLog {
    entries: VecDeque<(SystemTime, TestEvent)>,
    capacity: usize,
}

impl EventLog {
    /// A log which holds at most `capacity` events.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Records an event as having happened now.
    pub(crate) fn push(&mut self, event: TestEvent) {
        self.push_at(SystemTime::now(), event);
    }

    /// Records an event at a given time, dropping the oldest one if that takes
    /// the log past its capacity.
    pub(crate) fn push_at(&mut self, at: SystemTime, event: TestEvent) {
        self.entries.push_back((at, event));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// How many events are held. The UI never asks — it draws the tail it has
    /// room for — but the bound is exactly what has to be asserted.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether anything has been reported yet.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The last `rows` events, oldest first — the newest event is therefore
    /// the bottom line of the body.
    pub(crate) fn tail(&self, rows: usize) -> impl Iterator<Item = Line<'static>> + '_ {
        let skip = self.entries.len().saturating_sub(rows);
        self.entries
            .iter()
            .skip(skip)
            .map(|(at, event)| event.line(*at))
    }
}

/// A wall-clock `HH:MM:SS` in the machine's local timezone.
///
/// `localtime_r` rather than a date crate: the only thing a log line needs is
/// the time of day the user's clock shows, and the C library already knows the
/// timezone this process is running in.
fn clock_time(at: SystemTime) -> String {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let stamp = seconds as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };

    // SAFETY: `localtime_r` reads the timestamp we point it at and writes the
    // broken-down time into the `tm` we own and have zeroed; the reentrant form
    // keeps no global state, so it is safe to call from any thread.
    let converted = unsafe { libc::localtime_r(&raw const stamp, &raw mut parts) };
    if converted.is_null() {
        // A clock the C library cannot break down (a timestamp beyond its
        // range): fall back to UTC arithmetic rather than lose the timestamp.
        let day = seconds % 86_400;
        return format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60);
    }

    format!(
        "{:02}:{:02}:{:02}",
        parts.tm_hour, parts.tm_min, parts.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::time::Duration;

    /// A fixed instant, so a line's timestamp is stable within a test run.
    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[rstest]
    #[case(TestEvent::Heard("salute".into()), "heard: \"salute\"", Color::Gray)]
    #[case(TestEvent::Hearing("sal".into()), "hearing: \"sal\"", Color::DarkGray)]
    #[case(TestEvent::Muted, "hearing: (muted)", Color::DarkGray)]
    #[case(
        TestEvent::Matched { name: "Salute".into(), plan: "x".into() },
        "matched: \"Salute\" → x",
        Color::Green
    )]
    #[case(
        TestEvent::Interrupted("Sprint".into()),
        "interrupted: \"Sprint\"",
        Color::Yellow
    )]
    #[case(
        TestEvent::Discarded("Reload".into()),
        "discarded: \"Reload\"",
        Color::Yellow
    )]
    #[case(TestEvent::Listening(true), "listening: on", Color::Blue)]
    #[case(TestEvent::Listening(false), "listening: off", Color::Blue)]
    #[case(
        TestEvent::Error("the microphone stopped".into()),
        "error: the microphone stopped",
        Color::Red
    )]
    fn test_every_event_renders_the_same_text_in_both_modes(
        #[case] event: TestEvent,
        #[case] expected: &str,
        #[case] dot: Color,
    ) {
        assert_eq!(event.plain_line(), expected, "the plain line for {event:?}");
        assert_eq!(event.color(), dot, "the dot color for {event:?}");

        // The styled line is the same text, behind a timestamp and a dot of
        // the event's own color.
        let line = event.line(at());
        let spans: Vec<&str> = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(spans[2], DOT, "every line leads with a dot");
        assert_eq!(line.spans[2].style.fg, Some(dot));
        assert_eq!(spans[4], expected, "the log line says what the report says");
    }

    #[test]
    fn test_a_line_starts_with_a_wall_clock_time() {
        let line = TestEvent::Listening(true).line(at());
        let stamp = line.spans[0].content.as_ref();

        assert_eq!(stamp.len(), 8, "unexpected timestamp: {stamp:?}");
        let digits: Vec<char> = stamp.chars().collect();
        assert!(
            digits.iter().enumerate().all(|(i, c)| match i {
                2 | 5 => *c == ':',
                _ => c.is_ascii_digit(),
            }),
            "unexpected timestamp: {stamp:?}"
        );
    }

    #[test]
    fn test_the_log_is_bounded_by_its_capacity() {
        let mut log = EventLog::new(3);
        assert!(log.is_empty());

        for i in 0..10 {
            log.push(TestEvent::Heard(format!("utterance {i}")));
        }

        assert_eq!(log.len(), 3, "the scrollback must not grow without bound");

        // And it is the *oldest* which is dropped: the last three survive.
        let lines: Vec<String> = log.tail(10).map(|line| line.to_string()).collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("utterance 7"), "unexpected: {lines:?}");
        assert!(lines[2].contains("utterance 9"), "unexpected: {lines:?}");
    }

    #[test]
    fn test_a_zero_capacity_log_still_holds_the_newest_event() {
        // Nothing constructs one, but a log which panicked or spun on an empty
        // deque would be worse than one which keeps a single line.
        let mut log = EventLog::new(0);
        log.push(TestEvent::Heard("salute".into()));

        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_the_tail_is_the_newest_events_oldest_first() {
        let mut log = EventLog::new(SCROLLBACK);
        for i in 0..5 {
            log.push(TestEvent::Heard(format!("utterance {i}")));
        }

        let lines: Vec<String> = log.tail(2).map(|line| line.to_string()).collect();
        assert_eq!(lines.len(), 2, "only as many lines as there are rows");
        assert!(lines[0].contains("utterance 3"), "unexpected: {lines:?}");
        assert!(
            lines[1].contains("utterance 4"),
            "the newest event is the last line: {lines:?}"
        );
    }

    #[tokio::test]
    async fn test_a_channel_sink_forwards_events_in_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::Channel(tx);

        sink.send(TestEvent::Listening(true));
        sink.send(TestEvent::Heard("salute".into()));

        assert_eq!(rx.recv().await, Some(TestEvent::Listening(true)));
        assert_eq!(rx.recv().await, Some(TestEvent::Heard("salute".into())));
    }

    #[test]
    fn test_a_sink_whose_ui_has_gone_does_not_fail() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = EventSink::Channel(tx);
        drop(rx);

        // Reporting must never be able to break the pipeline reporting it.
        sink.send(TestEvent::Heard("salute".into()));
    }
}
