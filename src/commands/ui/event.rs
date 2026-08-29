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
//! entry which lives through the utterance — created at its first partial,
//! updated in place as the hypothesis grows, and settled by its `Final` (or
//! abandoned by a mute) — so an utterance and the commands it resolved to
//! share a line rather than filling several. See [`EventLog::push_at`].

use std::collections::VecDeque;
use std::time::SystemTime;

use ratatui::style::{Color, Modifier, Style};
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
    /// A finalized utterance, as the matcher receives it, with the sequence
    /// number of the utterance slot it closed (the narrator counts `Final`s
    /// and mutes exactly as the matcher does, so this meets the stamp a
    /// [`UiEvent::Matched`] carries).
    Heard { text: String, seq: u64 },
    /// A partial result on its way to becoming an utterance, with the slot of
    /// the utterance it belongs to (the one the stream has not closed yet).
    /// The terminal UI opens (and live-updates) that utterance's entry from
    /// these; plain mode prints them under `--debug-recognition` only.
    Hearing { text: String, seq: u64 },
    /// The recognizer was reset because the hotkey muted us, consuming
    /// utterance slot `seq`. The terminal UI settles that slot's live entry
    /// as abandoned; plain mode prints it under `--debug-recognition` only.
    Muted { seq: u64 },
    /// A command matched: in `run` it is being played, in `test` it is what
    /// would have been played. `utterance` is the slot of the utterance the
    /// matcher heard it in, when the source knows it — the log uses it to put
    /// the command on the transcript line it belongs to.
    Matched {
        name: String,
        plan: String,
        utterance: Option<u64>,
    },
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
            UiEvent::Heard { text, .. } => format!("heard: {text:?}"),
            UiEvent::Hearing { text, .. } => format!("hearing: {text:?}"),
            UiEvent::Muted { .. } => "hearing: (muted)".to_string(),
            UiEvent::Matched { name, plan, .. } => format!("matched: {name:?} → {plan}"),
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

    fn extra_text(&self) -> Option<String> {
        match self {
            UiEvent::Matched { utterance, .. } => utterance.map(|seq| format!("# {seq}")),
            UiEvent::Heard { seq, .. } | UiEvent::Hearing { seq, .. } | UiEvent::Muted { seq } => {
                Some(format!("# {seq}"))
            }
            _ => None,
        }
    }

    /// The color of the dot this event leads its log line with.
    fn color(&self) -> Color {
        match self {
            UiEvent::Matched { .. } => Color::Green,
            UiEvent::Heard { .. } => Color::Gray,
            UiEvent::Hearing { .. } | UiEvent::Muted { .. } => Color::DarkGray,
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
            UiEvent::Hearing { .. } | UiEvent::Muted { .. } => Style::new().fg(Color::DarkGray),
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
    /// merges into a single live entry ([`Entry::Recognition`]).
    fn line(&self, at: SystemTime) -> Line<'static> {
        log_line(
            at,
            self.color(),
            self.plain_line(),
            self.text_style(),
            self.extra_text(),
        )
    }
}

/// One log line: the time it happened, a dot in the entry's color, and its
/// text.
fn log_line(
    at: SystemTime,
    dot: Color,
    text: String,
    style: Style,
    extra_text: Option<String>,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(clock_time(at), Style::new().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(DOT, Style::new().fg(dot)),
        Span::raw(" "),
        Span::styled(text, style),
        Span::raw(" "),
        Span::styled(
            extra_text.unwrap_or_default(),
            style.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// The columns a log line spends before its text starts: `HH:MM:SS`, a space,
/// the dot, a space. Continuation lines of a wrapped entry indent this far,
/// so the timestamp+dot gutter keeps entries visually distinct.
const GUTTER: usize = 11;

/// Wraps one log line to `width` columns: the text is word-wrapped (long
/// words hard-broken) and every continuation line is indented past the
/// timestamp+dot gutter, keeping the entry's style on every piece.
///
/// A terminal too narrow to fit anything past the gutter gets the line
/// unwrapped — ratatui truncates it, which is the best a six-column terminal
/// can hope for.
pub(crate) fn wrap_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    let Some(room) = width.checked_sub(GUTTER).filter(|room| *room >= 1) else {
        return vec![line];
    };

    // Every log line is built by `log_line`: gutter spans first, the text
    // last. Anything else (defensively) passes through unwrapped.
    let Some((text_span, gutter_spans)) = line.spans.split_last() else {
        return vec![line];
    };
    if text_span.content.chars().count() <= room {
        return vec![line];
    }

    let pieces = wrap_text(&text_span.content, room);
    let style = text_span.style;
    let mut lines = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = if index == 0 {
            gutter_spans.to_vec()
        } else {
            vec![Span::raw(" ".repeat(GUTTER))]
        };
        spans.push(Span::styled(piece, style));
        lines.push(Line::from(spans));
    }
    lines
}

/// Greedy word wrap to `width` columns: breaks at spaces where it can, and
/// hard-breaks a word longer than a whole line where it must.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        let needed = if current_len == 0 {
            word_len
        } else {
            current_len + 1 + word_len
        };

        if needed <= width {
            if current_len > 0 {
                current.push(' ');
                current_len += 1;
            }
            current.push_str(word);
            current_len += word_len;
            continue;
        }

        if current_len > 0 {
            lines.push(std::mem::take(&mut current));
        }

        // The word alone fits a fresh line, or it has to be broken.
        if word_len <= width {
            current.push_str(word);
            current_len = word_len;
        } else {
            let mut chunk = String::new();
            let mut chunk_len = 0usize;
            for character in word.chars() {
                if chunk_len == width {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_len = 0;
                }
                chunk.push(character);
                chunk_len += 1;
            }
            current = chunk;
            current_len = chunk_len;
        }
    }

    if current_len > 0 || lines.is_empty() {
        lines.push(current);
    }
    lines
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

/// Where a recognition entry is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Still being spoken: the text is a partial hypothesis the recognizer
    /// may yet revise, drawn dim and italic so a guess never reads as a
    /// transcript.
    Live,
    /// Finalized: the text is the settled transcript.
    Settled,
    /// Muted mid-utterance: the endpointer never finalized it, so the text
    /// stays the last hypothesis and the entry says so.
    Abandoned,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A recognition: what is being (or was) heard, and every command it has
    /// resolved to. Created **live** at the utterance's first partial, its
    /// text updating in place as the hypothesis grows; matches attach the
    /// moment they fire; the `Final` settles the text and style (or a mute
    /// abandons it). An entry which settles without ever matching stays grey
    /// — the "it heard me but nothing fired" signal a rehearsal exists to
    /// find.
    Recognition {
        text: String,
        /// The utterance slot this recognition occupies, for meeting the
        /// matcher's stamps.
        seq: u64,
        stage: Stage,
        matches: Vec<Resolved>,
    },
    /// Anything else, exactly as it was reported.
    Other(UiEvent),
}

impl Entry {
    fn line(&self, at: SystemTime) -> Line<'static> {
        let Entry::Recognition {
            text,
            stage,
            matches,
            ..
        } = self
        else {
            let Entry::Other(event) = self else {
                unreachable!()
            };
            return event.line(at);
        };

        let extra_text = match (stage, matches) {
            (Stage::Abandoned, _) => Some("(muted)".to_string()),
            (_, matches) if !matches.is_empty() => {
                let resolved = matches
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!("({resolved})"))
            }
            _ => None,
        };

        // The dot keeps the severity vocabulary: green the moment anything
        // fires, grey while nothing has, yellow for an utterance a mute cut
        // short (the same color interrupted commands carry). The text style
        // carries the *certainty*: a live hypothesis is dim and italic, a
        // settled transcript is plain, an abandoned one stays dim.
        let (dot, style) = match (stage, matches.is_empty()) {
            (Stage::Live, true) => (
                Color::DarkGray,
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            (Stage::Live, false) => (
                Color::Green,
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            (Stage::Settled, true) => (Color::Gray, Style::new()),
            (Stage::Settled, false) => (Color::Green, Style::new()),
            (Stage::Abandoned, _) => (Color::Yellow, Style::new().fg(Color::DarkGray)),
        };
        log_line(at, dot, format!("{text:?}"), style, extra_text)
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
    /// A recognition is **one** entry rather than a transcript of events: its
    /// first partial creates the entry live, later partials revise its text
    /// in place, its `Final` settles it (or a mute abandons it), and every
    /// command it fires attaches the moment the match is reported. Events
    /// meet by **utterance sequence**: the matcher stamps every command with
    /// the slot of the utterance it was heard in, and the narrator stamps
    /// every partial, recognition and mute the same way, so the log never has
    /// to guess from order:
    ///
    /// - a match lands on the entry with its own slot, no matter how many
    ///   later utterances have been heard since (the completion timeout
    ///   regularly fires after the next utterance) — and because the entry
    ///   exists from the first partial, an eager fire is *visible the moment
    ///   the keys press* rather than waiting for the `Final`;
    /// - a `Final` for a slot with no live entry (partials suppressed, or the
    ///   entry scrolled away) still gets its settled entry;
    /// - a mute for a slot with no entry — listening turned off before
    ///   anything was heard — logs nothing at all;
    /// - and a match with no stamp, or whose entry has scrolled away, is
    ///   logged on its own rather than dropped.
    pub(crate) fn push_at(&mut self, at: SystemTime, event: UiEvent) {
        match event {
            UiEvent::Hearing { text, seq } => match self.recognition_with_seq(seq) {
                // Only a live entry follows the hypothesis; partials cannot
                // reopen a settled or abandoned slot.
                Some(Entry::Recognition {
                    text: entry_text,
                    stage: Stage::Live,
                    ..
                }) => *entry_text = text,
                Some(_) => {}
                None => self.entries.push_back((
                    at,
                    Entry::Recognition {
                        text,
                        seq,
                        stage: Stage::Live,
                        matches: Vec::new(),
                    },
                )),
            },
            UiEvent::Heard { text, seq } => match self.recognition_with_seq(seq) {
                Some(Entry::Recognition {
                    text: entry_text,
                    stage,
                    ..
                }) => {
                    *entry_text = text;
                    *stage = Stage::Settled;
                }
                _ => self.entries.push_back((
                    at,
                    Entry::Recognition {
                        text,
                        seq,
                        stage: Stage::Settled,
                        matches: Vec::new(),
                    },
                )),
            },
            UiEvent::Muted { seq } => {
                // The mute abandons the slot's live entry, keeping whatever
                // hypothesis (and fires) it had; an utterance muted before
                // any partial was heard never existed as far as the log is
                // concerned.
                if let Some(Entry::Recognition {
                    stage: stage @ Stage::Live,
                    ..
                }) = self.recognition_with_seq(seq)
                {
                    *stage = Stage::Abandoned;
                }
            }
            UiEvent::Matched {
                name,
                plan,
                utterance,
            } => {
                let mut resolved = Some(Resolved { name, plan });
                if let Some(seq) = utterance
                    && let Some(Entry::Recognition { matches, .. }) = self.recognition_with_seq(seq)
                {
                    matches.push(resolved.take().expect("the match attaches only once"));
                }
                // The entry has scrolled out of the log (or the source
                // carried no stamp at all); the match must still be visible.
                if let Some(resolved) = resolved {
                    self.push_standalone(at, resolved, utterance);
                }
            }
            other => self.entries.push_back((at, Entry::Other(other))),
        }

        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// Logs a match on its own line, for when there is no recognition entry
    /// it could upgrade.
    fn push_standalone(&mut self, at: SystemTime, resolved: Resolved, utterance: Option<u64>) {
        self.entries.push_back((
            at,
            Entry::Other(UiEvent::Matched {
                name: resolved.name,
                plan: resolved.plan,
                utterance,
            }),
        ));
    }

    /// The logged recognition entry for utterance slot `seq`, if it has not
    /// scrolled away.
    fn recognition_with_seq(&mut self, seq: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().rev().map(|(_, entry)| entry).find(
            |entry| matches!(entry, Entry::Recognition { seq: entry_seq, .. } if *entry_seq == seq),
        )
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
    time_of_day(seconds as i64)
}

/// The `HH:MM:SS` of a Unix timestamp, with no timezone applied.
///
/// Pure, and shared by both platforms: [`clock_time`]'s UTC fallback is this
/// with no offset, and the Windows clock below is this with one.
fn time_of_day(seconds: i64) -> String {
    let day = seconds.rem_euclid(86_400);

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

/// The local `HH:MM:SS` for a Unix timestamp on Windows.
///
/// Windows has no `localtime_r`, and — more to the point — `GetLocalTime`
/// answers a question we are not asking: it breaks down *now*, whereas a log
/// line has to render the moment its entry was recorded, over and over, as the
/// screen redraws. Using it directly would stamp every line in the scrollback
/// with the current time.
///
/// So the timezone is what we take from Windows, not the time: `GetLocalTime`
/// and `GetSystemTime` are read back to back and the difference between them is
/// this machine's current offset from UTC, daylight saving included, which is
/// then applied to the timestamp we were given. The two readings can straddle a
/// second boundary, and it does not matter — [`zone_offset`] rounds to the
/// minute, which is the finest granularity any real timezone has.
#[cfg(windows)]
fn local_clock(seconds: u64) -> Option<String> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

    /// The seconds-since-midnight a `SYSTEMTIME` reads.
    fn seconds_of_day(at: &SYSTEMTIME) -> i64 {
        i64::from(at.wHour) * 3600 + i64::from(at.wMinute) * 60 + i64::from(at.wSecond)
    }

    let (mut local, mut utc) = (SYSTEMTIME::default(), SYSTEMTIME::default());

    // SAFETY: both calls only write a `SYSTEMTIME` into the storage we own and
    // point them at; neither can fail and neither keeps the pointer.
    unsafe {
        GetLocalTime(&raw mut local);
        GetSystemTime(&raw mut utc);
    }

    let offset = zone_offset(seconds_of_day(&local), seconds_of_day(&utc));

    Some(time_of_day(seconds as i64 + offset))
}

/// Platforms we have no localization story for yet render timestamps in UTC:
/// [`clock_time`] falls back to plain UTC arithmetic when this returns `None`.
#[cfg(not(any(target_os = "linux", windows)))]
fn local_clock(_seconds: u64) -> Option<String> {
    None
}

/// This machine's offset from UTC, in seconds, given the two wall clocks read
/// at (as near as makes no difference) the same moment.
///
/// Both readings are seconds-since-midnight, so a clock which has already
/// rolled over into another date shows up as a difference of nearly a whole
/// day. That is folded back to the shortest way round, `-12:00 ..= +12:00`,
/// which means the far-eastern zones are reported as their western
/// equivalent — `+13:00` comes back as `-11:00`. It makes no difference to the
/// only thing this feeds: a time of day is taken modulo a day, so an offset
/// which is wrong by exactly 24 hours renders exactly the same clock.
///
/// Rounded to the minute because a stray second between the two readings must
/// not turn `+02:00` into `+01:59:59`.
///
/// Compiled on Linux under `cfg(test)` so that the arithmetic the Windows clock
/// depends on is tested on the platform this project is developed on.
#[cfg(any(windows, test))]
fn zone_offset(local: i64, utc: i64) -> i64 {
    /// Half a day, the point at which the other way round is shorter. `-12:00`
    /// itself is a real timezone, so only the eastern end is exclusive.
    const HALF_DAY: i64 = 12 * 3600;

    let difference = match local - utc {
        raw if raw > HALF_DAY => raw - 86_400,
        raw if raw < -HALF_DAY => raw + 86_400,
        raw => raw,
    };

    // To the nearest minute: a second may have ticked over between the two
    // readings, and no timezone is offset by one.
    (difference + 30 * difference.signum()) / 60 * 60
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

    /// A heard utterance closing slot `seq`.
    fn heard(text: &str, seq: u64) -> UiEvent {
        UiEvent::Heard {
            text: text.to_string(),
            seq,
        }
    }

    /// A partial hypothesis belonging to slot `seq`.
    fn hearing(text: &str, seq: u64) -> UiEvent {
        UiEvent::Hearing {
            text: text.to_string(),
            seq,
        }
    }

    #[rstest]
    #[case(heard("salute", 1), "heard: \"salute\"", Color::Gray)]
    #[case(hearing("sal", 1), "hearing: \"sal\"", Color::DarkGray)]
    #[case(UiEvent::Muted { seq: 1 }, "hearing: (muted)", Color::DarkGray)]
    #[case(
        UiEvent::Matched { name: "Salute".into(), plan: "x".into(), utterance: Some(1) },
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
            utterance: Some(1),
        }
        .line(at());
        assert_eq!(line.spans[4].style.fg, None);
    }

    #[rstest]
    #[case(0, "00:00:00")]
    #[case(1, "00:00:01")]
    #[case(1_700_000_000, "22:13:20")]
    // A day boundary in either direction: the clock wraps, it never goes
    // negative and it never overflows past 23:59:59.
    #[case(86_399, "23:59:59")]
    #[case(86_400, "00:00:00")]
    #[case(-1, "23:59:59")]
    fn test_the_time_of_day_is_a_wrapped_wall_clock(#[case] seconds: i64, #[case] expected: &str) {
        assert_eq!(time_of_day(seconds), expected);
    }

    #[rstest]
    // Both clocks in the same day: the plain difference, west and east.
    #[case(12 * 3600, 12 * 3600, 0)]
    #[case(14 * 3600, 12 * 3600, 2 * 3600)]
    #[case(12 * 3600 + 1800, 12 * 3600, 1800)]
    #[case(7 * 3600, 12 * 3600, -5 * 3600)]
    // Local has already rolled into tomorrow (UTC+13, at 23:30 UTC)...
    #[case(30 * 60, 23 * 3600 + 30 * 60, 60 * 60)]
    // ...and local is still in yesterday (UTC-5, at 00:30 UTC).
    #[case(19 * 3600 + 30 * 60, 30 * 60, -5 * 3600)]
    // A second ticked over between the two readings: an offset is a whole
    // number of minutes, so the odd second is rounded away rather than
    // reported as 01:59:59.
    #[case(14 * 3600 - 1, 12 * 3600, 2 * 3600)]
    #[case(7 * 3600 + 1, 12 * 3600, -5 * 3600)]
    // UTC-12, the westernmost zone there is, is not folded away.
    #[case(0, 12 * 3600, -12 * 3600)]
    fn test_the_zone_offset_is_the_difference_between_the_two_clocks(
        #[case] local: i64,
        #[case] utc: i64,
        #[case] expected: i64,
    ) {
        assert_eq!(zone_offset(local, utc), expected);
    }

    #[test]
    fn test_a_far_eastern_zone_is_reported_the_short_way_round() {
        // Kiritimati is UTC+14: at 12:00 UTC its clocks read 02:00 tomorrow.
        // The offset comes back as -10:00, the shortest way round — and it has
        // to render the same time of day, which is the only thing it feeds.
        let offset = zone_offset(2 * 3600, 12 * 3600);

        assert_eq!(offset, -10 * 3600);
        assert_eq!(
            time_of_day(1_700_000_000 + offset),
            time_of_day(1_700_000_000 + 14 * 3600),
            "an offset a whole day out renders the same clock"
        );
    }

    #[test]
    fn test_a_localized_stamp_is_the_offset_applied_to_the_timestamp() {
        // What the Windows clock does, with the syscalls' answers supplied:
        // 22:13:20 UTC is a quarter past midnight in a UTC+02:00 zone.
        let seconds = 1_700_000_000_i64;

        assert_eq!(time_of_day(seconds), "22:13:20");
        assert_eq!(time_of_day(seconds + zone_offset(2 * 3600, 0)), "00:13:20");
        assert_eq!(
            time_of_day(seconds + zone_offset(19 * 3600, 0)),
            "17:13:20",
            "UTC-05:00, expressed as a clock which is still on yesterday"
        );
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

    // --- Wrapping -----------------------------------------------------------

    #[test]
    fn test_a_long_line_wraps_within_the_width_with_the_gutter_indented() {
        let line = UiEvent::Warning(
            "eager mismatch: fired \"Autocannon\" from a partial hypothesis, but the utterance settled as \"auto cannon sentry\"".to_string(),
        )
        .line(at());

        let wrapped = wrap_line(line, 40);

        assert!(wrapped.len() > 1, "the line should need several rows");
        for (index, row) in wrapped.iter().enumerate() {
            let text = row.to_string();
            assert!(
                text.chars().count() <= 40,
                "row {index} overflows the width: {text:?}"
            );
            if index > 0 {
                assert!(
                    text.starts_with("           "),
                    "continuation rows indent past the timestamp+dot gutter: {text:?}"
                );
            }
        }
        assert!(
            wrapped[0].to_string().contains(DOT),
            "the first row keeps the timestamp and dot"
        );

        // Nothing is lost: the rows re-join into the original text.
        let rejoined = wrapped
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let text = row.to_string();
                if index == 0 {
                    text.chars().skip(11).collect::<String>()
                } else {
                    text.trim_start().to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            rejoined.contains("the utterance settled as \"auto cannon sentry\""),
            "the tail of the message must survive the wrap: {rejoined:?}"
        );
    }

    #[test]
    fn test_wrapped_continuations_keep_the_entrys_style() {
        let line = UiEvent::Warning("w".repeat(120)).line(at());

        let wrapped = wrap_line(line, 40);

        assert!(wrapped.len() > 1);
        for row in &wrapped[1..] {
            let text_span = row.spans.last().expect("a continuation has text");
            assert_eq!(
                text_span.style.fg,
                Some(Color::Yellow),
                "a warning stays yellow on every row"
            );
        }
    }

    #[test]
    fn test_a_short_line_and_a_tiny_terminal_pass_through_unwrapped() {
        let line = UiEvent::Warning("short".to_string()).line(at());
        assert_eq!(wrap_line(line.clone(), 80), vec![line.clone()]);

        // Too narrow to fit anything past the gutter: unwrapped (and left to
        // the terminal to truncate) beats an empty body.
        assert_eq!(wrap_line(line.clone(), 6), vec![line]);
    }

    #[rstest]
    #[case("a b c", 10, &["a b c"])]
    #[case("one two three four", 9, &["one two", "three", "four"])]
    // A word longer than the whole line is hard-broken rather than dropped.
    #[case("abcdefghij", 4, &["abcd", "efgh", "ij"])]
    #[case("hi abcdefgh", 5, &["hi", "abcde", "fgh"])]
    // Exact fits stay whole.
    #[case("exact", 5, &["exact"])]
    fn test_wrap_text(#[case] text: &str, #[case] width: usize, #[case] expected: &[&str]) {
        assert_eq!(wrap_text(text, width), expected);
    }

    // --- The live recognition entry ----------------------------------------

    fn matched(name: &str, plan: &str, utterance: u64) -> UiEvent {
        UiEvent::Matched {
            name: name.to_string(),
            plan: plan.to_string(),
            utterance: Some(utterance),
        }
    }

    /// The style of the newest entry's text span.
    fn newest_style(log: &EventLog) -> Style {
        let lines: Vec<Line<'static>> = log.tail(SCROLLBACK).collect();
        lines.last().expect("the log has an entry").spans[4].style
    }

    #[test]
    fn test_a_partial_creates_a_live_entry_and_later_partials_revise_it() {
        // Realtime feedback: the utterance is on screen from its very first
        // partial, dim and italic so a hypothesis never reads as a settled
        // transcript.
        let mut log = EventLog::new(SCROLLBACK);

        log.push_at(at(), hearing("auto", 1));
        assert_eq!(
            rendered(&log),
            vec![(Color::DarkGray, "\"auto\"".to_string())],
            "the first partial opens the entry"
        );
        assert!(
            newest_style(&log).add_modifier.contains(Modifier::ITALIC),
            "a live hypothesis is italic"
        );

        // A changed hypothesis revises the same entry rather than adding one.
        log.push_at(at(), hearing("auto cannon", 1));
        assert_eq!(
            rendered(&log),
            vec![(Color::DarkGray, "\"auto cannon\"".to_string())]
        );
    }

    #[test]
    fn test_the_final_settles_the_live_entry() {
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), hearing("deploy the", 1));
        log.push_at(at(), heard("deploy the thing", 1));

        // The finalized transcript replaces the hypothesis, the style
        // settles, and an unmatched final stays grey — the "it heard me but
        // nothing fired" signal keeps its meaning.
        assert_eq!(
            rendered(&log),
            vec![(Color::Gray, "\"deploy the thing\"".to_string())]
        );
        assert_eq!(
            newest_style(&log),
            Style::new(),
            "a settled transcript drops the live styling"
        );

        // And a late partial for a settled slot cannot reopen it.
        log.push_at(at(), hearing("stray", 1));
        assert_eq!(
            rendered(&log),
            vec![(Color::Gray, "\"deploy the thing\"".to_string())]
        );
    }

    #[test]
    fn test_an_eager_match_attaches_to_the_live_entry_the_moment_it_fires() {
        // The field-measured latency: the engine fires an unambiguous phrase
        // ~600ms before its Final. The entry exists from the first partial,
        // so the fire is visible immediately — nothing waits for the
        // endpointer.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), hearing("auto cannon sentry", 1));
        log.push_at(at(), matched("AutocannonSentry", "5", 1));

        assert_eq!(
            rendered(&log),
            vec![(
                Color::Green,
                "\"auto cannon sentry\" → AutocannonSentry (5)".to_string()
            )],
            "the fire lands on the live entry at once"
        );
        assert!(
            newest_style(&log).add_modifier.contains(Modifier::ITALIC),
            "the text is still a hypothesis until the Final settles it"
        );

        // The Final then settles text and style, keeping the match.
        log.push_at(at(), heard("auto cannon sentry", 1));
        assert_eq!(
            rendered(&log),
            vec![(
                Color::Green,
                "\"auto cannon sentry\" → AutocannonSentry (5)".to_string()
            )]
        );
        assert_eq!(newest_style(&log), Style::new());
    }

    #[test]
    fn test_a_mute_settles_a_live_entry_as_abandoned() {
        // Muted mid-utterance: the hypothesis text is kept, the entry says
        // what happened to it, and the interrupted-yellow vocabulary marks
        // it. An eager fire it had already attached stays visible — the keys
        // pressed, and pretending otherwise would be a lie.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), hearing("deploy sentry reload", 1));
        log.push_at(at(), matched("DeploySentry", "6", 1));
        log.push_at(at(), UiEvent::Muted { seq: 1 });

        assert_eq!(
            rendered(&log),
            vec![(
                Color::Yellow,
                "\"deploy sentry reload\" (muted) → DeploySentry (6)".to_string()
            )]
        );

        // An unmatched live entry abandons the same way, minus the match.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), hearing("auto can", 1));
        log.push_at(at(), UiEvent::Muted { seq: 1 });
        assert_eq!(
            rendered(&log),
            vec![(Color::Yellow, "\"auto can\" (muted)".to_string())]
        );
    }

    #[test]
    fn test_a_mute_before_any_partial_logs_nothing() {
        // Listening toggled off between utterances: the slot closes on the
        // narrator's count, but there was never anything to show for it.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), UiEvent::Muted { seq: 1 });

        assert!(log.is_empty(), "an unheard mute must not invent an entry");
    }

    #[test]
    fn test_interleaved_utterances_stay_correctly_attributed() {
        // Entry N live while entry N-1 is settled: the completion-timeout
        // interleaving, with the next utterance already being spoken when the
        // previous one's match fires. Every event carries its slot, so
        // nothing lands one line up.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), hearing("autocannon", 1));
        log.push_at(at(), heard("autocannon", 1)); // settled, still unmatched
        log.push_at(at(), hearing("re", 2)); // the next one is live
        log.push_at(at(), matched("Autocannon", "4", 1)); // timeout fire for 1
        log.push_at(at(), hearing("reload", 2));
        log.push_at(at(), matched("Reload", "r", 2)); // eager fire for 2
        log.push_at(at(), heard("reload", 2));

        assert_eq!(
            rendered(&log),
            vec![
                (Color::Green, "\"autocannon\" → Autocannon (4)".to_string()),
                (Color::Green, "\"reload\" → Reload (r)".to_string()),
            ],
            "each match lands on the utterance which produced it"
        );
    }

    #[test]
    fn test_the_field_transcripts_render_correctly() {
        // The two recordings from the field report, replayed as the narrator
        // and reporter emit them (timings measured in
        // src/matcher/recorded.rs): the unambiguous phrase fires off its
        // stable partial well before its Final, and the ambiguous prefix
        // fires off its armed completion timeout — also before its Final.
        let mut log = EventLog::new(SCROLLBACK);

        // "auto cannon sentry.wav"
        log.push_at(at(), hearing("auto", 1)); // 1.300s
        log.push_at(at(), hearing("auto cannon", 1)); // 1.500s
        log.push_at(at(), hearing("auto cannon sentry", 1)); // 2.000s
        log.push_at(at(), matched("AutocannonSentry", "5", 1)); // 2.102s
        assert_eq!(
            rendered(&log),
            vec![(
                Color::Green,
                "\"auto cannon sentry\" → AutocannonSentry (5)".to_string()
            )],
            "the fire is visible ~600ms before the Final arrives"
        );
        log.push_at(at(), heard("auto cannon sentry", 1)); // 2.700s

        // "auto cannon.wav"
        log.push_at(at(), hearing("auto", 2)); // 1.003s
        log.push_at(at(), hearing("auto cannon", 2)); // 1.303s
        log.push_at(at(), matched("Autocannon", "4", 2)); // 1.805s
        log.push_at(at(), heard("auto cannon", 2)); // 2.003s

        assert_eq!(
            rendered(&log),
            vec![
                (
                    Color::Green,
                    "\"auto cannon sentry\" → AutocannonSentry (5)".to_string()
                ),
                (Color::Green, "\"auto cannon\" → Autocannon (4)".to_string()),
            ]
        );
    }

    #[test]
    fn test_a_recognition_is_one_entry_which_upgrades_in_place() {
        let mut log = EventLog::new(SCROLLBACK);

        log.push_at(at(), heard("auto cannon sentry", 1));
        assert_eq!(
            rendered(&log),
            vec![(Color::Gray, "\"auto cannon sentry\"".to_string())],
            "an utterance nothing has matched yet is grey, and says only itself"
        );

        log.push_at(at(), matched("Autocannon sentry", "4", 1));
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
        log.push_at(at(), heard("salute", 1));
        log.push_at(at(), matched("Salute", "x", 1));
        log.push_at(at(), heard("deploy the thing", 2));

        assert_eq!(
            rendered(&log),
            vec![
                (Color::Green, "\"salute\" → Salute (x)".to_string()),
                (Color::Gray, "\"deploy the thing\"".to_string()),
            ]
        );
    }

    #[test]
    fn test_a_late_match_never_lands_on_an_unmatched_earlier_utterance() {
        // The sequence stamp resolves what order alone never could: an
        // utterance which matched nothing, followed by one whose match
        // arrives with two grey entries waiting. The match names its own
        // utterance, so the unmatched one stays grey — the "it heard me but
        // nothing fired" signal survives.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), heard("deploy the thing", 1));
        log.push_at(at(), heard("salute", 2));
        log.push_at(at(), matched("Salute", "x", 2));

        assert_eq!(
            rendered(&log),
            vec![
                (Color::Gray, "\"deploy the thing\"".to_string()),
                (Color::Green, "\"salute\" → Salute (x)".to_string()),
            ],
            "the match belongs to the utterance which produced it"
        );
    }

    #[test]
    fn test_a_match_for_an_unheard_slot_never_lands_on_the_previous_entry() {
        // A match whose slot has no entry at all — its partials were never
        // reported (a scrolled log, a source with no live narration). It must
        // be visible, but on its own line: attaching it to the previous
        // utterance painted a partial as a completed command and cascaded
        // every later match one line up, which is the bug the sequence
        // stamps exist to prevent.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), heard("auto cannon sentry", 1));
        log.push_at(at(), matched("AutocannonSentry", "down up right", 1));
        log.push_at(at(), matched("Autocannon", "down left down", 2));

        assert_eq!(
            rendered(&log),
            vec![
                (
                    Color::Green,
                    "\"auto cannon sentry\" → AutocannonSentry (down up right)".to_string()
                ),
                (
                    Color::Green,
                    "matched: \"Autocannon\" → down left down".to_string()
                ),
            ],
            "the stray match keeps its own line rather than stealing entry 1"
        );
    }

    #[test]
    fn test_a_match_upgrades_the_oldest_utterance_waiting_for_one() {
        // The completion-timeout interleaving: "autocannon" rests on an
        // ambiguous terminal, so the *next* utterance is heard before the
        // first one's match fires. The match belongs to the older entry.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(at(), heard("autocannon", 1));
        log.push_at(at(), heard("reload", 2));
        log.push_at(at(), matched("Autocannon", "4", 1));
        log.push_at(at(), matched("Reload", "r", 2));

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
        log.push_at(at(), heard("salute reload", 1));
        log.push_at(at(), matched("Salute", "x", 1));
        log.push_at(at(), matched("Reload", "r", 1));

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
        // The log has scrolled past the utterance this match belongs to (or
        // the source carried no stamp at all): the match must still be
        // visible.
        let mut log = EventLog::new(2);
        log.push_at(at(), heard("salute", 1));
        log.push_at(at(), heard("deploy the thing", 2));
        log.push_at(at(), heard("reload", 3)); // "salute" scrolls out
        log.push_at(at(), matched("Salute", "x", 1));

        let lines = rendered(&log);
        assert_eq!(
            lines.last(),
            Some(&(Color::Green, "matched: \"Salute\" → x".to_string())),
            "unexpected log: {lines:?}"
        );

        // And a stampless match (no matcher in the loop) does the same.
        let mut log = EventLog::new(SCROLLBACK);
        log.push_at(
            at(),
            UiEvent::Matched {
                name: "Salute".to_string(),
                plan: "x".to_string(),
                utterance: None,
            },
        );
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
        log.push_at(at(), heard("sprint", 1));
        log.push_at(at(), matched("Sprint", "w (held)", 1));
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
            log.push_at(at(), heard(&format!("utterance {i}"), i + 1));
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
        log.push_at(at(), heard("salute", 1));

        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_the_tail_is_the_newest_entries_oldest_first() {
        let mut log = EventLog::new(SCROLLBACK);
        for i in 0..5 {
            log.push_at(at(), heard(&format!("utterance {i}"), i + 1));
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
        sink.send(heard("salute", 1));

        assert_eq!(rx.recv().await, Some(UiEvent::Listening(true)));
        assert_eq!(rx.recv().await, Some(heard("salute", 1)));
    }

    #[test]
    fn test_a_sink_whose_ui_has_gone_does_not_fail() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = EventSink::Channel(tx);
        drop(rx);

        // Reporting must never be able to break the pipeline reporting it.
        sink.send(heard("salute", 1));
    }
}
