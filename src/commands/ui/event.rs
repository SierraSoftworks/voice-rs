//! The single stream of things a session has to say, and the two ways of
//! saying them. See DESIGN.md §"The session terminal UI (ratatui)".
//!
//! Everything the pipeline reports during `voice-orders test` or
//! `voice-orders run` — an utterance it heard, a command it matched, a hotkey
//! mute, a line the wrapped application printed, a failure — becomes one
//! [`UiEvent`]. Exactly one renderer consumes them:
//!
//! - [`EventSink::Plain`] prints the line-per-event report `test` has always
//!   printed, character for character, so piped output and scripts are
//!   unaffected; and
//! - [`EventSink::Channel`] hands them to the terminal UI, which owns the
//!   screen and draws them into a bounded [`EventLog`].
//!
//! Unifying them here is what keeps the two modes honest: there is one place
//! which decides what an event *says*, and the mode only decides how it looks.
//!
//! The log is not a transcript of the stream, though: a recognition is **one**
//! entry which upgrades in place, so an utterance and the command it resolved
//! to share a line rather than filling two. See [`EventLog::push_at`].

use std::collections::VecDeque;
use std::time::SystemTime;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

/// How many entries the terminal UI keeps in its scrollback.
///
/// A session can be left running for an entire game, so the log has to be
/// bounded: the oldest entry is dropped once this many are held.
pub(crate) const SCROLLBACK: usize = 1000;

/// The dot which leads every line in the terminal UI's log.
pub(crate) const DOT: &str = "●";

/// Everything a session reports, from whichever part of the pipeline saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiEvent {
    /// A finalized utterance, as the matcher receives it.
    Heard(String),
    /// A partial result on its way to becoming an utterance
    /// (`--debug-recognition` only).
    Hearing(String),
    /// The recognizer was reset because the hotkey muted us
    /// (`--debug-recognition` only).
    Muted,
    /// A command matched: in `run` it is being played, in `test` it is what
    /// would have been played.
    Matched { name: String, plan: String },
    /// A command in flight was cut short because listening stopped.
    Interrupted(String),
    /// A command the matcher had already queued was thrown away unplayed.
    Discarded(String),
    /// The hotkey changed the listening state. The terminal UI shows this in
    /// its footer rather than its log; plain mode prints it as a line.
    Listening(bool),
    /// A line the wrapped application wrote to stdout or stderr (`run` under
    /// the terminal UI, where its stdio is piped rather than inherited).
    Child { program: String, line: String },
    /// The wrapped application exited.
    ChildExited { program: String, code: i32 },
    /// Something went wrong which does not stop the session, but which the
    /// user should know about — most often the recognizer failing to decode.
    Warning(String),
    /// Something in the pipeline failed in a way the user should see.
    Error(String),
}

impl UiEvent {
    /// The line this event prints in plain mode.
    ///
    /// These strings are load-bearing: they are `test`'s piped output, which
    /// scripts and CI parse, so they must not drift.
    pub(crate) fn plain_line(&self) -> String {
        match self {
            UiEvent::Heard(text) => format!("heard: {text:?}"),
            UiEvent::Hearing(text) => format!("hearing: {text:?}"),
            UiEvent::Muted => "hearing: (muted)".to_string(),
            UiEvent::Matched { name, plan } => format!("matched: {name:?} → {plan}"),
            UiEvent::Interrupted(name) => format!("interrupted: {name:?}"),
            UiEvent::Discarded(name) => format!("discarded: {name:?}"),
            UiEvent::Listening(true) => "listening: on".to_string(),
            UiEvent::Listening(false) => "listening: off".to_string(),
            UiEvent::Child { program, line } => format!("{program}: {line}"),
            UiEvent::ChildExited { program, code } => {
                format!("{program} exited with code {code}")
            }
            UiEvent::Warning(message) => format!("warning: {message}"),
            UiEvent::Error(message) => format!("error: {message}"),
        }
    }

    /// The color of the dot this event leads its log line with.
    fn color(&self) -> Color {
        match self {
            UiEvent::Matched { .. } => Color::Green,
            UiEvent::Heard(_) => Color::Gray,
            UiEvent::Hearing(_) | UiEvent::Muted => Color::DarkGray,
            UiEvent::Interrupted(_) | UiEvent::Discarded(_) | UiEvent::Warning(_) => Color::Yellow,
            UiEvent::Error(_) => Color::Red,
            UiEvent::Child { .. } => Color::White,
            // The listening state is drawn in the footer rather than logged;
            // the wrapped application's exit inherits its blue dot.
            UiEvent::Listening(_) | UiEvent::ChildExited { .. } => Color::Blue,
        }
    }

    /// The style the event's text itself is drawn in; the dot carries the
    /// severity, so only the quietest and loudest events restyle their text.
    fn text_style(&self) -> Style {
        match self {
            UiEvent::Hearing(_) | UiEvent::Muted => Style::new().fg(Color::DarkGray),
            UiEvent::Child { .. } | UiEvent::ChildExited { .. } => Style::new().fg(Color::Gray),
            UiEvent::Warning(_) => Style::new().fg(Color::Yellow),
            UiEvent::Error(_) => Style::new().fg(Color::Red),
            _ => Style::new(),
        }
    }

    /// This event as one styled log line: `HH:MM:SS ● <the plain text>`.
    ///
    /// The text is deliberately the same as the plain report's, so the two
    /// modes never disagree about what happened and the dot is a redundant
    /// (rather than the only) cue — colors alone are not something a report
    /// should depend on. The one exception is a recognition, which the log
    /// merges into a single upgrading entry ([`Entry::Recognition`]).
    fn line(&self, at: SystemTime) -> Line<'static> {
        log_line(at, self.color(), self.plain_line(), self.text_style())
    }
}

/// One log line: the time it happened, a dot in the entry's color, and its
/// text.
fn log_line(at: SystemTime, dot: Color, text: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(clock_time(at), Style::new().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(DOT, Style::new().fg(dot)),
        Span::raw(" "),
        Span::styled(text, style),
    ])
}

/// Where a session's events go.
///
/// Cloned into every part of the pipeline which reports something, so the
/// mode is chosen once (in [`super::ReportMode`]) and never again.
#[derive(Debug, Clone)]
pub(crate) enum EventSink {
    /// Print the plain report to stdout, exactly as `test` always has. This is
    /// also what `run --debug-recognition` uses without a terminal, so its
    /// output is unchanged.
    Plain,
    /// Hand the event to whoever owns the terminal (the UI task).
    Channel(mpsc::UnboundedSender<UiEvent>),
}

impl EventSink {
    /// Reports one event.
    ///
    /// Infallible and non-blocking by construction: a session must never be
    /// slowed down (or, worse, deadlocked) by its own reporting, so a UI which
    /// has already gone away simply stops receiving events.
    pub(crate) fn send(&self, event: UiEvent) {
        match self {
            EventSink::Plain => println!("{}", event.plain_line()),
            EventSink::Channel(events) => {
                let _ = events.send(event);
            }
        }
    }
}

/// One command a recognition resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    name: String,
    plan: String,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A recognition: what was heard, and every command it resolved to. Starts
    /// empty (grey) and upgrades in place (green) as the matcher resolves it —
    /// an entry which never upgrades is the "it heard me but nothing fired"
    /// signal a rehearsal exists to find.
    Recognition {
        text: String,
        matches: Vec<Resolved>,
    },
    /// Anything else, exactly as it was reported.
    Other(UiEvent),
}

impl Entry {
    /// Whether this is a recognition nothing has matched yet.
    fn is_unresolved(&self) -> bool {
        matches!(self, Entry::Recognition { matches, .. } if matches.is_empty())
    }

    fn line(&self, at: SystemTime) -> Line<'static> {
        match self {
            Entry::Other(event) => event.line(at),
            Entry::Recognition { text, matches } if matches.is_empty() => {
                log_line(at, Color::Gray, format!("{text:?}"), Style::new())
            }
            Entry::Recognition { text, matches } => {
                let resolved = matches
                    .iter()
                    .map(|m| format!("{} ({})", m.name, m.plan))
                    .collect::<Vec<_>>()
                    .join(", ");

                log_line(
                    at,
                    Color::Green,
                    format!("{text:?} → {resolved}"),
                    Style::new(),
                )
            }
        }
    }
}

/// The terminal UI's bounded scrollback: the last [`SCROLLBACK`] entries, each
/// with the wall-clock time it started.
pub(crate) struct EventLog {
    entries: VecDeque<(SystemTime, Entry)>,
    capacity: usize,
}

impl EventLog {
    /// A log which holds at most `capacity` entries.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Records an event at a given time, dropping the oldest entry if that
    /// takes the log past its capacity.
    ///
    /// A recognition is **one** entry rather than two lines: the utterance
    /// arrives first and is logged grey, and the match which resolves it
    /// upgrades that same entry rather than appending under it. Correlating
    /// them is a question of *which* utterance a match belongs to:
    ///
    /// - Matches always follow their utterance's `Final` in event order, but
    ///   the matcher's completion timeout means a *later* utterance can be
    ///   logged before an earlier one's match fires (say "autocannon", pause,
    ///   say "reload": the second utterance is heard before the first settles).
    ///   So a match belongs to the **oldest** recognition nothing has matched
    ///   yet, not the newest.
    /// - When every recognition has already been matched, the match is a
    ///   second (greedy multi-match) result for the newest one — one utterance
    ///   can contain several commands — so it joins that entry's line.
    /// - And with no recognition at all to attach to (nothing narrates
    ///   utterances, or the log has scrolled past it), it is logged on its own
    ///   rather than dropped.
    pub(crate) fn push_at(&mut self, at: SystemTime, event: UiEvent) {
        match event {
            UiEvent::Heard(text) => self.entries.push_back((
                at,
                Entry::Recognition {
                    text,
                    matches: Vec::new(),
                },
            )),
            UiEvent::Matched { name, plan } => {
                let resolved = Resolved { name, plan };
                match self.recognition_for_next_match() {
                    Some(matches) => matches.push(resolved),
                    None => self.entries.push_back((
                        at,
                        Entry::Other(UiEvent::Matched {
                            name: resolved.name,
                            plan: resolved.plan,
                        }),
                    )),
                }
            }
            other => self.entries.push_back((at, Entry::Other(other))),
        }

        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// The match list the next match should join: the oldest recognition
    /// nothing has matched yet, or failing that the newest recognition of all.
    fn recognition_for_next_match(&mut self) -> Option<&mut Vec<Resolved>> {
        let index = self
            .entries
            .iter()
            .position(|(_, entry)| entry.is_unresolved())
            .or_else(|| {
                self.entries
                    .iter()
                    .rposition(|(_, entry)| matches!(entry, Entry::Recognition { .. }))
            })?;

        match &mut self.entries[index] {
            (_, Entry::Recognition { matches, .. }) => Some(matches),
            _ => None,
        }
    }

    /// How many entries are held. The UI never asks — it draws the tail it has
    /// room for — but the bound is exactly what has to be asserted.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether anything has been reported yet.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The last `rows` entries, oldest first — the newest entry is therefore
    /// the bottom line of the body.
    pub(crate) fn tail(&self, rows: usize) -> impl Iterator<Item = Line<'static>> + '_ {
        let skip = self.entries.len().saturating_sub(rows);
        self.entries
            .iter()
            .skip(skip)
            .map(|(at, entry)| entry.line(*at))
    }
}

/// A wall-clock `HH:MM:SS`, in the machine's local timezone where we can work
/// out what that is.
fn clock_time(at: SystemTime) -> String {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(local) = local_clock(seconds) {
        return local;
    }

    // A clock we cannot localize (a timestamp beyond the C library's range, or
    // a platform we have no localization for yet): fall back to UTC arithmetic
    // rather than lose the timestamp.
    let day = seconds % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

/// The local `HH:MM:SS` for a Unix timestamp, or [`None`] when the C library
/// cannot break it down.
///
/// `localtime_r` rather than a date crate: the only thing a log line needs is
/// the time of day the user's clock shows, and the C library already knows the
/// timezone this process is running in.
#[cfg(target_os = "linux")]
fn local_clock(seconds: u64) -> Option<String> {
    let stamp = seconds as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };

    // SAFETY: `localtime_r` reads the timestamp we point it at and writes the
    // broken-down time into the `tm` we own and have zeroed; the reentrant form
    // keeps no global state, so it is safe to call from any thread.
    let converted = unsafe { libc::localtime_r(&raw const stamp, &raw mut parts) };
    if converted.is_null() {
        return None;
    }

    Some(format!(
        "{:02}:{:02}:{:02}",
        parts.tm_hour, parts.tm_min, parts.tm_sec
    ))
}

/// Windows has no `localtime_r`; localizing the session log's timestamps is
/// W5's job, and until then [`clock_time`]'s UTC fallback carries them.
#[cfg(not(target_os = "linux"))]
fn local_clock(_seconds: u64) -> Option<String> {
    None
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

    /// The text of a rendered line, without its timestamp or dot.
    fn text(line: &Line<'static>) -> String {
        line.spans[4].content.to_string()
    }

    /// Every entry the log holds, as `(dot color, text)`.
    fn rendered(log: &EventLog) -> Vec<(Color, String)> {
        log.tail(SCROLLBACK)
            .map(|line| {
                (
                    line.spans[2].style.fg.expect("the dot is colored"),
                    text(&line),
                )
            })
            .collect()
    }

    #[rstest]
    #[case(UiEvent::Heard("salute".into()), "heard: \"salute\"", Color::Gray)]
    #[case(UiEvent::Hearing("sal".into()), "hearing: \"sal\"", Color::DarkGray)]
    #[case(UiEvent::Muted, "hearing: (muted)", Color::DarkGray)]
    #[case(
        UiEvent::Matched { name: "Salute".into(), plan: "x".into() },
        "matched: \"Salute\" → x",
        Color::Green
    )]
    #[case(
        UiEvent::Interrupted("Sprint".into()),
        "interrupted: \"Sprint\"",
        Color::Yellow
    )]
    #[case(
        UiEvent::Discarded("Reload".into()),
        "discarded: \"Reload\"",
        Color::Yellow
    )]
    #[case(UiEvent::Listening(true), "listening: on", Color::Blue)]
    #[case(UiEvent::Listening(false), "listening: off", Color::Blue)]
    #[case(
        UiEvent::Child { program: "sh".into(), line: "hello".into() },
        "sh: hello",
        Color::White
    )]
    #[case(
        UiEvent::ChildExited { program: "sh".into(), code: 3 },
        "sh exited with code 3",
        Color::Blue
    )]
    #[case(
        UiEvent::Warning("the speech recognizer could not decode the audio".into()),
        "warning: the speech recognizer could not decode the audio",
        Color::Yellow
    )]
    #[case(
        UiEvent::Error("the microphone stopped".into()),
        "error: the microphone stopped",
        Color::Red
    )]
    fn test_every_event_renders_the_same_text_in_both_modes(
        #[case] event: UiEvent,
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
    fn test_the_wrapped_applications_output_is_dim_and_named_after_it() {
        // The game's log is context, not the point of the screen: it is drawn
        // quietly (a white dot, grey text) and prefixed with the program's
        // name, so it can never be mistaken for something voice-orders said.
        let line = UiEvent::Child {
            program: "helldivers2".to_string(),
            line: "Steam initialised".to_string(),
        }
        .line(at());

        assert_eq!(line.spans[2].style.fg, Some(Color::White), "the dot");
        assert_eq!(line.spans[4].style.fg, Some(Color::Gray), "the text");
        assert_eq!(
            line.spans[4].content.as_ref(),
            "helldivers2: Steam initialised"
        );

        // A matched command, by contrast, is drawn in the terminal's own
        // foreground: it is the thing the user is watching for.
        let line = UiEvent::Matched {
            name: "Salute".to_string(),
            plan: "x".to_string(),
        }
        .line(at());
        assert_eq!(line.spans[4].style.fg, None);
    }

    #[test]
    fn test_a_line_starts_with_a_wall_clock_time() {
        let line = UiEvent::Listening(true).line(at());
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

    // --- One entry per recognition ----------------------------------------

    fn matched(name: &str, plan: &str) -> UiEvent {
        UiEvent::Matched {
            name: name.to_string(),
            plan: plan.to_string(),
        }
    }

    #[test]
    fn test_a_recognition_is_one_entry_which_upgrades_in_place() {
        let mut log = EventLog::new(SCROLLBACK);

        log.push_at(at(), UiEvent::Heard("auto cannon sentry".to_string()));
        assert_eq!(
            rendered(&log),
            vec![(Color::Gray, "\"auto cannon sentry\"".to_string())],
            "an utterance nothing has matched yet is grey, and says only itself"
        );

        log.push_at(at(), matched("Autocannon sentry", "4"));
        assert_eq!(
            rendered(&log),
            vec![(
                Color::Green,
                "\"auto cannon sentry\" → Autocannon sentry (4)".to_string()
            )],
            "the match upgrades the utterance rather than adding a line under it"
        );
    }

    #[test]
    fn test_an_utterance_which_never_matches_stays_grey() {
        // The whole point of the merge: a grey line is the "it heard me but
        // nothing fired" signal, and it is one line rather than an absence.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), UiEvent::Heard("salute".to_string()));
        log.push_at(at(), matched("Salute", "x"));
        log.push_at(at(), UiEvent::Heard("deploy the thing".to_string()));

        assert_eq!(
            rendered(&log),
            vec![
                (Color::Green, "\"salute\" → Salute (x)".to_string()),
                (Color::Gray, "\"deploy the thing\"".to_string()),
            ]
        );
    }

    #[test]
    fn test_a_match_cannot_tell_a_late_match_from_an_unmatched_utterance() {
        // The cost of correlating by order alone, pinned so it is a decision
        // rather than a surprise: these two sequences are indistinguishable in
        // the event stream, and the log resolves both the same way.
        //
        //  1. "autocannon" rested on an ambiguous terminal and its match fired
        //     late, after the next utterance was heard — the case this rule
        //     exists for, and the one it gets right.
        //  2. "deploy the thing" matched nothing at all, and the *next*
        //     utterance's match arrives with two grey entries waiting.
        //
        // Telling them apart needs the matcher to say which utterance a
        // command came from, which is not something the event stream carries
        // today.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), UiEvent::Heard("deploy the thing".to_string()));
        log.push_at(at(), UiEvent::Heard("salute".to_string()));
        log.push_at(at(), matched("Salute", "x"));

        assert_eq!(
            rendered(&log),
            vec![
                (
                    Color::Green,
                    "\"deploy the thing\" → Salute (x)".to_string()
                ),
                (Color::Gray, "\"salute\"".to_string()),
            ],
            "the oldest utterance waiting for a match takes it"
        );
    }

    #[test]
    fn test_a_match_upgrades_the_oldest_utterance_waiting_for_one() {
        // The completion-timeout interleaving: "autocannon" rests on an
        // ambiguous terminal, so the *next* utterance is heard before the
        // first one's match fires. The match belongs to the older entry.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), UiEvent::Heard("autocannon".to_string()));
        log.push_at(at(), UiEvent::Heard("reload".to_string()));
        log.push_at(at(), matched("Autocannon", "4"));
        log.push_at(at(), matched("Reload", "r"));

        assert_eq!(
            rendered(&log),
            vec![
                (Color::Green, "\"autocannon\" → Autocannon (4)".to_string()),
                (Color::Green, "\"reload\" → Reload (r)".to_string()),
            ],
            "each match should land on the utterance which produced it"
        );
    }

    #[test]
    fn test_a_greedy_multi_match_joins_one_entry() {
        // One utterance, two commands: the second match has no un-upgraded
        // entry to claim, so it joins the newest recognition instead of
        // stealing a later one.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), UiEvent::Heard("salute reload".to_string()));
        log.push_at(at(), matched("Salute", "x"));
        log.push_at(at(), matched("Reload", "r"));

        assert_eq!(
            rendered(&log),
            vec![(
                Color::Green,
                "\"salute reload\" → Salute (x), Reload (r)".to_string()
            )]
        );
    }

    #[test]
    fn test_a_match_with_nothing_to_upgrade_is_logged_on_its_own() {
        // Nothing narrates utterances (or the log has scrolled past the one
        // this belongs to): the match must still be visible.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), matched("Salute", "x"));

        assert_eq!(
            rendered(&log),
            vec![(Color::Green, "matched: \"Salute\" → x".to_string())]
        );
    }

    #[test]
    fn test_other_events_keep_their_own_entries() {
        // Interrupted and discarded commands are their own yellow entries;
        // they say something about a command which already fired, not about
        // the utterance which produced it.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), UiEvent::Heard("sprint".to_string()));
        log.push_at(at(), matched("Sprint", "w (held)"));
        log.push_at(at(), UiEvent::Interrupted("Sprint".to_string()));
        log.push_at(at(), UiEvent::Discarded("Reload".to_string()));

        assert_eq!(
            rendered(&log),
            vec![
                (Color::Green, "\"sprint\" → Sprint (w (held))".to_string()),
                (Color::Yellow, "interrupted: \"Sprint\"".to_string()),
                (Color::Yellow, "discarded: \"Reload\"".to_string()),
            ]
        );
    }

    #[test]
    fn test_the_log_is_bounded_by_its_capacity() {
        let mut log = EventLog::new(3);
        assert!(log.is_empty());

        for i in 0..10 {
            log.push_at(at(), UiEvent::Heard(format!("utterance {i}")));
        }

        assert_eq!(log.len(), 3, "the scrollback must not grow without bound");

        // And it is the *oldest* which is dropped: the last three survive.
        let lines: Vec<String> = log.tail(10).map(|line| line.to_string()).collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("utterance 7"), "unexpected: {lines:?}");
        assert!(lines[2].contains("utterance 9"), "unexpected: {lines:?}");
    }

    #[test]
    fn test_a_zero_capacity_log_still_holds_the_newest_entry() {
        // Nothing constructs one, but a log which panicked or spun on an empty
        // deque would be worse than one which keeps a single line.
        let mut log = EventLog::new(0);
        log.push_at(at(), UiEvent::Heard("salute".into()));

        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_the_tail_is_the_newest_entries_oldest_first() {
        let mut log = EventLog::new(SCROLLBACK);
        for i in 0..5 {
            log.push_at(at(), UiEvent::Heard(format!("utterance {i}")));
        }

        let lines: Vec<String> = log.tail(2).map(|line| line.to_string()).collect();
        assert_eq!(lines.len(), 2, "only as many lines as there are rows");
        assert!(lines[0].contains("utterance 3"), "unexpected: {lines:?}");
        assert!(
            lines[1].contains("utterance 4"),
            "the newest entry is the last line: {lines:?}"
        );
    }

    #[tokio::test]
    async fn test_a_channel_sink_forwards_events_in_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::Channel(tx);

        sink.send(UiEvent::Listening(true));
        sink.send(UiEvent::Heard("salute".into()));

        assert_eq!(rx.recv().await, Some(UiEvent::Listening(true)));
        assert_eq!(rx.recv().await, Some(UiEvent::Heard("salute".into())));
    }

    #[test]
    fn test_a_sink_whose_ui_has_gone_does_not_fail() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = EventSink::Channel(tx);
        drop(rx);

        // Reporting must never be able to break the pipeline reporting it.
        sink.send(UiEvent::Heard("salute".into()));
    }
}
