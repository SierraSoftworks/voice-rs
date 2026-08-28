//! The full-screen rehearsal UI. See DESIGN.md §"The `test` terminal UI
//! (ratatui)".
//!
//! Three regions, and nothing which scrolls sideways:
//!
//! ```text
//! Helldivers 2                 12 command(s) · 48 phrase(s) · rightctrl (push-to-talk)
//! profiles/helldivers2.yaml
//! ──────────────────────────────────────────────────────────────────────────────────
//! 19:04:11 ● listening: on
//! 19:04:13 ● heard: "deploy the autocannon"
//! 19:04:13 ● matched: "Autocannon" → leftctrl+4
//! ──────────────────────────────────────────────────────────────────────────────────
//! ● listening: on  —  q to quit                            vosk-model-small-en-us-0.15
//! ```
//!
//! **On tracing and the alternate screen:** this process logs to stdout (see
//! `telemetry.rs` — for a CLI, `info!`/`warn!` are user-facing output), and
//! stdout is the same handle the alternate screen is drawn on. Rather than
//! reach into the subscriber, the split is by *seam*: everything the rehearsal
//! itself reports travels as a [`TestEvent`] and is drawn in the log below,
//! failures included, while the pipeline's own tracing happens before this UI
//! starts and after it has been restored. A `warn!` from deep inside the audio
//! stack mid-session would still scribble on the screen; it is rare, the next
//! tick redraws over it, and the alternative — swallowing logs which *are* the
//! interface in plain mode — is worse.

use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use crate::commands::run::PipelineSummary;
use crate::config::{Profile, ResolvedSettings};
use crate::hotkey::ListenMode;

use super::event::{DOT, EventLog, SCROLLBACK, TestEvent};

/// How long the UI waits for something to happen before redrawing anyway.
///
/// Everything interesting arrives as an event, so this is only a safety net (a
/// resize crossterm did not report, a redraw over a stray log line); slow
/// enough to cost nothing, quick enough that nothing ever looks stuck.
const TICK: Duration = Duration::from_millis(250);

/// How long the key reader blocks before checking whether it should stop.
///
/// crossterm's `poll`/`read` are blocking, and we deliberately do not enable
/// its `event-stream` feature (which would pull in `futures` for one channel),
/// so a blocking thread does the reading and this bounds how long it lingers
/// once the rehearsal is over.
const KEY_POLL: Duration = Duration::from_millis(100);

/// A key event, or the reason we stopped being able to read them.
type KeyResult = Result<KeyEvent, String>;

/// What the header and footer say about the session: the parts which do not
/// change while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Overview {
    /// The profile's display name.
    pub profile: String,
    /// Where the profile came from: the resolved path, or the URL.
    pub source: String,
    pub commands: usize,
    pub phrases: usize,
    /// The hotkey arrangement, as the header says it.
    pub hotkey: String,
    /// The speech model's name.
    pub model: String,
    /// Whether there is a hotkey at all — a profile without one is always
    /// listening, which the footer says instead of an on/off state.
    pub has_hotkey: bool,
    /// The listening state the pipeline started in.
    pub listening: bool,
}

impl Overview {
    /// What the pipeline just assembled, as the header will describe it.
    pub fn describe(
        profile: &Profile,
        settings: &ResolvedSettings,
        source: &str,
        summary: &PipelineSummary,
    ) -> Self {
        Self {
            profile: profile.display_name().to_string(),
            source: source.to_string(),
            commands: summary.commands,
            phrases: summary.phrases,
            hotkey: hotkey_summary(settings, summary.mode),
            // The model's directory name, not its path: the full path is in the
            // logs, and the footer only has to answer "which model?".
            model: summary.model.file_name().map_or_else(
                || summary.model.display().to_string(),
                |name| name.to_string_lossy().to_string(),
            ),
            has_hotkey: summary.mode.is_some(),
            listening: summary.listening,
        }
    }

    /// The right-hand side of the header: what this profile adds up to.
    fn stats(&self) -> String {
        format!(
            "{} command(s) · {} phrase(s) · {}",
            self.commands, self.phrases, self.hotkey
        )
    }
}

/// How the hotkey reads in the header: which key, in which mode, or the fact
/// that there is nothing to press.
fn hotkey_summary(settings: &ResolvedSettings, mode: Option<ListenMode>) -> String {
    match (mode, settings.hotkey.as_ref()) {
        (Some(mode), Some(hotkey)) => format!("{} ({mode})", hotkey.key),
        _ => "always listening".to_string(),
    }
}

/// The UI's whole state: what it was told at startup, and everything reported
/// since.
pub(super) struct App {
    overview: Overview,
    log: EventLog,
    listening: bool,
}

impl App {
    pub fn new(overview: Overview) -> Self {
        Self {
            listening: overview.listening,
            overview,
            log: EventLog::new(SCROLLBACK),
        }
    }

    /// Folds one reported event into the state.
    pub fn record(&mut self, event: TestEvent) {
        if let TestEvent::Listening(now) = event {
            self.listening = now;
        }

        self.log.push(event);
    }

    /// Records an event at a fixed time, so a rendering test has a stable
    /// screen to assert against.
    #[cfg(test)]
    pub fn record_at(&mut self, at: std::time::SystemTime, event: TestEvent) {
        if let TestEvent::Listening(now) = event {
            self.listening = now;
        }

        self.log.push_at(at, event);
    }
}

/// Runs the terminal UI until the user quits, the rehearsal is stopped, or
/// something goes wrong drawing.
///
/// `quit` is the contract with the rest of the command: this cancels it when
/// the user asks to leave (so the supervisor stops waiting for a signal), and
/// returns when anybody else cancels it (so a Ctrl-C or a SIGTERM takes the
/// screen down before anything is printed over it).
pub(super) async fn run(
    overview: Overview,
    events: mpsc::UnboundedReceiver<TestEvent>,
    quit: CancellationToken,
) -> Result<(), crate::Error> {
    // However we leave — a draw failure, a panic further up — the token is
    // cancelled, so nothing is left waiting on a UI which has already gone.
    let _guard = quit.clone().drop_guard();

    // `try_init` enables raw mode, enters the alternate screen, and installs a
    // panic hook which restores both before the panic is reported: a crash
    // must not leave the user with an unusable terminal.
    let mut terminal = ratatui::try_init().map_err(|e| {
        human_errors::wrap_user(
            e,
            "We could not take over the terminal to show the rehearsal.",
            &[
                "Run `voice-orders test` in an interactive terminal, or pipe its output (`voice-orders test profile.yaml | cat`) to get the plain line-by-line report instead.",
            ],
        )
    })?;

    let (keys_tx, keys) = mpsc::unbounded_channel();
    let reader = tokio::task::spawn_blocking({
        let quit = quit.clone();
        move || read_keys(&keys_tx, &quit)
    });

    let outcome = drive(&mut terminal, App::new(overview), events, keys, &quit).await;

    // Wait for the reader to notice it is done before handing the terminal
    // back, so it cannot swallow a keystroke meant for the shell.
    quit.cancel();
    let _ = reader.await;

    // Restore before anything else can print: the alternate screen must be
    // gone before an error message or a tracing line reaches stdout.
    ratatui::restore();
    outcome
}

/// The event loop: draw, wait for something, fold it in, draw again.
async fn drive<B>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut events: mpsc::UnboundedReceiver<TestEvent>,
    mut keys: mpsc::UnboundedReceiver<KeyResult>,
    quit: &CancellationToken,
) -> Result<(), crate::Error>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut events_open = true;
    let mut keys_open = true;

    loop {
        draw(terminal, &app)?;

        tokio::select! {
            () = quit.cancelled() => {
                debug!("The rehearsal is shutting down, stopping the terminal UI.");
                return Ok(());
            }
            event = recv(&mut events, &mut events_open) => {
                app.record(event);
                // Whatever arrived alongside it is folded in before we draw: a
                // burst of discarded commands is one redraw, not one each.
                while let Ok(event) = events.try_recv() {
                    app.record(event);
                }
            }
            key = recv(&mut keys, &mut keys_open) => {
                match key {
                    Ok(key) if is_quit(&key) => {
                        debug!("The user asked to stop the rehearsal.");
                        quit.cancel();
                        return Ok(());
                    }
                    Ok(_) => {}
                    // The one failure this task can see for itself; it goes
                    // into the log like any other, red dot and all.
                    Err(message) => app.record(TestEvent::Error(message)),
                }
            }
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

/// Receives from a channel, or waits forever once it has closed.
///
/// Without the flag a closed channel would leave its `select!` arm ready
/// immediately and for ever, and the loop would spin instead of ticking.
async fn recv<T>(channel: &mut mpsc::UnboundedReceiver<T>, open: &mut bool) -> T {
    if *open && let Some(value) = channel.recv().await {
        return value;
    }

    *open = false;
    std::future::pending().await
}

/// Reads key events until the rehearsal ends.
///
/// Blocking, on its own thread: `poll` wakes every [`KEY_POLL`] to notice that
/// it should stop, which is what keeps this from needing crossterm's
/// `event-stream` feature.
fn read_keys(keys: &mpsc::UnboundedSender<KeyResult>, quit: &CancellationToken) {
    while !quit.is_cancelled() {
        let ready = match poll(KEY_POLL) {
            Ok(ready) => ready,
            Err(e) => {
                let _ = keys.send(Err(format!("We stopped watching the keyboard ({e}).")));
                return;
            }
        };

        if !ready {
            continue;
        }

        match read() {
            // Resizes and mouse events are picked up by the next draw; only
            // keys mean anything to us.
            Ok(Event::Key(key)) => {
                if keys.send(Ok(key)).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let _ = keys.send(Err(format!("We stopped watching the keyboard ({e}).")));
                return;
            }
        }
    }
}

/// Whether a key press means "stop".
///
/// Ctrl-C is handled here because raw mode is exactly the state in which the
/// terminal stops turning it into a SIGINT for us: the key has to be read, or
/// the most reflexive way out of a full-screen program would do nothing.
fn is_quit(key: &KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }

    match key.code {
        KeyCode::Char('q' | 'Q') => true,
        KeyCode::Char('c' | 'C') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

/// Draws one frame, turning a broken terminal into a reportable error.
fn draw<B>(terminal: &mut Terminal<B>, app: &App) -> Result<(), crate::Error>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal
        .draw(|frame| render(frame, app))
        .map(|_| ())
        .map_err(|e| {
            human_errors::wrap_system(
                e,
                "We could not draw the rehearsal on your terminal.",
                &["Please report this issue on GitHub so that we can investigate."],
            )
        })
}

/// The whole screen: a two-line header, the log, and a one-line footer.
pub(super) fn render(frame: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, header, &app.overview);
    render_body(frame, body, app);
    render_footer(frame, footer, app);
}

/// Splits a row into "everything else" on the left and exactly enough room for
/// `right` on the right, so the right-hand text is never pushed off the end by
/// a long left-hand one.
fn split_row(row: Rect, right: &str) -> [Rect; 2] {
    let width = u16::try_from(right.chars().count()).unwrap_or(u16::MAX);

    Layout::horizontal([Constraint::Fill(1), Constraint::Length(width)]).areas(row)
}

/// The profile's name and stats on the first line, where it came from on the
/// second.
fn render_header(frame: &mut Frame, area: Rect, overview: &Overview) {
    let [title, source] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let stats = overview.stats();
    let [name_area, stats_area] = split_row(title, &stats);

    frame.render_widget(
        Paragraph::new(Span::styled(
            overview.profile.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        name_area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(stats, Style::new().fg(Color::Gray))).right_aligned(),
        stats_area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            overview.source.clone(),
            Style::new().fg(Color::DarkGray),
        )),
        source,
    );
}

/// The event log, newest at the bottom, between two rules.
///
/// Only as many lines as there are rows are built, so a full scrollback costs
/// no more to draw than an empty one. Lines are not wrapped: a wrapped line
/// would push the newest event off the bottom of the region it was measured
/// for, which is the one line that must always be visible.
fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.log.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Speak a command; press q to stop.",
                Style::new().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let lines: Vec<Line<'static>> = app.log.tail(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The live listening state, and which model is doing the listening.
fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let [state_area, model_area] = split_row(area, &app.overview.model);

    let (state, color) = match (app.overview.has_hotkey, app.listening) {
        (false, _) => ("always listening", Color::Green),
        (true, true) => ("listening: on", Color::Green),
        (true, false) => ("listening: off", Color::Gray),
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(DOT, Style::new().fg(color)),
            Span::raw(" "),
            Span::styled(state, Style::new().fg(color)),
            Span::styled("  —  q to quit", Style::new().fg(Color::DarkGray)),
        ])),
        state_area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            app.overview.model.clone(),
            Style::new().fg(Color::DarkGray),
        ))
        .right_aligned(),
        model_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::KeyEventState;
    use rstest::rstest;
    use std::time::SystemTime;

    fn overview() -> Overview {
        Overview {
            profile: "Helldivers 2".to_string(),
            source: "profiles/helldivers2.yaml".to_string(),
            commands: 12,
            phrases: 48,
            hotkey: "rightctrl (push-to-talk)".to_string(),
            model: "vosk-model-small-en-us-0.15".to_string(),
            has_hotkey: true,
            listening: false,
        }
    }

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// A profile, its resolved settings, and the summary a started pipeline
    /// would hand back for them.
    fn assembled(
        yaml: &str,
        mode: Option<ListenMode>,
    ) -> (Profile, ResolvedSettings, PipelineSummary) {
        let profile = Profile::parse(&crate::config::LoadedProfile {
            source: "profiles/helldivers2.yaml".to_string(),
            content: format!(
                "name: Helldivers 2\nmodel: /models/en\n{yaml}commands:\n  - phrase: salute\n    keys: [\"x\"]\n"
            ),
        })
        .expect("the profile should load");
        let settings = ResolvedSettings::resolve(&profile, &crate::config::SystemConfig::default())
            .expect("the settings should resolve");
        let summary = PipelineSummary {
            device: "default".to_string(),
            model: std::path::PathBuf::from("/models/vosk-model-small-en-us-0.15"),
            commands: 12,
            phrases: 48,
            mode,
            listening: false,
        };

        (profile, settings, summary)
    }

    #[test]
    fn test_the_overview_is_what_the_pipeline_assembled() {
        let (profile, settings, summary) = assembled(
            "hotkey:\n  key: rightctrl\n  mode: push-to-talk\n",
            Some(ListenMode::PushToTalk),
        );

        let overview =
            Overview::describe(&profile, &settings, "profiles/helldivers2.yaml", &summary);

        assert_eq!(overview, self::overview());
        assert_eq!(
            overview.stats(),
            "12 command(s) · 48 phrase(s) · rightctrl (push-to-talk)"
        );
    }

    #[test]
    fn test_an_overview_without_a_hotkey_says_so() {
        let (profile, settings, summary) = assembled("", None);

        let overview =
            Overview::describe(&profile, &settings, "profiles/helldivers2.yaml", &summary);

        assert!(!overview.has_hotkey);
        assert_eq!(overview.hotkey, "always listening");
        assert_eq!(
            overview.stats(),
            "12 command(s) · 48 phrase(s) · always listening"
        );
    }

    /// Renders an app into a fixed-size terminal and hands back the buffer.
    fn screen(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("the test backend should start");
        terminal
            .draw(|frame| render(frame, app))
            .expect("the frame should draw");

        terminal.backend().buffer().clone()
    }

    /// One row of a rendered buffer, as text.
    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| {
                buffer
                    .cell((x, y))
                    .expect("the cell should be inside the buffer")
                    .symbol()
            })
            .collect()
    }

    #[test]
    fn test_the_header_names_the_profile_and_where_it_came_from() {
        let buffer = screen(&App::new(overview()), 90, 12);

        let title = row(&buffer, 0);
        assert!(
            title.starts_with("Helldivers 2"),
            "the profile name is left-aligned: {title:?}"
        );
        assert!(
            title
                .trim_end()
                .ends_with("12 command(s) · 48 phrase(s) · rightctrl (push-to-talk)"),
            "the stats are right-aligned: {title:?}"
        );
        assert_eq!(
            row(&buffer, 1).trim_end(),
            "profiles/helldivers2.yaml",
            "the profile's source is the second header line"
        );
    }

    #[test]
    fn test_the_footer_carries_the_state_on_the_left_and_the_model_on_the_right() {
        let mut app = App::new(overview());
        let footer = row(&screen(&app, 90, 12), 11);

        assert!(
            footer.starts_with(&format!("{DOT} listening: off")),
            "the listening state leads the footer: {footer:?}"
        );
        assert!(
            footer.ends_with("vosk-model-small-en-us-0.15"),
            "the model is right-aligned in the footer: {footer:?}"
        );

        // And it is *live*: the hotkey's own event moves it.
        app.record_at(at(), TestEvent::Listening(true));
        let footer = row(&screen(&app, 90, 12), 11);
        assert!(
            footer.starts_with(&format!("{DOT} listening: on")),
            "the footer should follow the hotkey: {footer:?}"
        );
    }

    #[test]
    fn test_a_profile_without_a_hotkey_says_it_is_always_listening() {
        let mut app = App::new(Overview {
            has_hotkey: false,
            listening: true,
            hotkey: "always listening".to_string(),
            ..overview()
        });
        app.record_at(at(), TestEvent::Heard("salute".to_string()));

        let footer = row(&screen(&app, 90, 12), 11);
        assert!(
            footer.starts_with(&format!("{DOT} always listening")),
            "unexpected footer: {footer:?}"
        );
    }

    #[test]
    fn test_the_body_logs_events_newest_at_the_bottom() {
        let mut app = App::new(overview());
        app.record_at(at(), TestEvent::Listening(true));
        app.record_at(at(), TestEvent::Heard("deploy the autocannon".to_string()));
        app.record_at(
            at(),
            TestEvent::Matched {
                name: "Autocannon".to_string(),
                plan: "leftctrl+4".to_string(),
            },
        );

        let buffer = screen(&app, 90, 12);

        // Row 2 is the rule under the header, so the log starts at row 3.
        let lines: Vec<String> = (3..6).map(|y| row(&buffer, y)).collect();
        assert!(
            lines[0].contains(&format!("{DOT} listening: on")),
            "unexpected: {lines:?}"
        );
        assert!(
            lines[1].contains(&format!("{DOT} heard: \"deploy the autocannon\"")),
            "unexpected: {lines:?}"
        );
        assert!(
            lines[2].contains(&format!("{DOT} matched: \"Autocannon\" → leftctrl+4")),
            "the newest event is the last line: {lines:?}"
        );
    }

    #[test]
    fn test_the_body_shows_only_the_newest_events_it_has_room_for() {
        let mut app = App::new(overview());
        for i in 0..50 {
            app.record_at(at(), TestEvent::Heard(format!("utterance {i}")));
        }

        // 12 rows: 2 of header, 2 rules and a footer leave 7 lines of log.
        let buffer = screen(&app, 90, 12);
        let body: Vec<String> = (3..10).map(|y| row(&buffer, y)).collect();

        assert!(
            body[0].contains("utterance 43"),
            "the body should hold the newest 7 events: {body:?}"
        );
        assert!(
            body[6].contains("utterance 49"),
            "the newest event should be the bottom line: {body:?}"
        );
    }

    #[test]
    fn test_an_empty_log_invites_the_user_to_speak() {
        let buffer = screen(&App::new(overview()), 90, 12);

        assert_eq!(
            row(&buffer, 3).trim_end(),
            "Speak a command; press q to stop."
        );
    }

    #[test]
    fn test_the_dot_carries_the_events_colour() {
        let mut app = App::new(overview());
        app.record_at(
            at(),
            TestEvent::Matched {
                name: "Autocannon".to_string(),
                plan: "4".to_string(),
            },
        );

        let buffer = screen(&app, 90, 12);
        let dot = (0..90)
            .map(|x| buffer.cell((x, 3)).expect("inside the buffer"))
            .find(|cell| cell.symbol() == DOT)
            .expect("the line should carry a dot");

        assert_eq!(dot.fg, Color::Green);
    }

    #[test]
    fn test_a_tiny_terminal_still_renders() {
        // Nothing here may panic on an area the layout cannot honour.
        let mut app = App::new(overview());
        app.record_at(at(), TestEvent::Heard("salute".to_string()));

        for (width, height) in [(1, 1), (4, 3), (20, 4), (200, 2)] {
            screen(&app, width, height);
        }
    }

    // --- Keys --------------------------------------------------------------

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[rstest]
    #[case(KeyCode::Char('q'), KeyModifiers::NONE, true)]
    #[case(KeyCode::Char('Q'), KeyModifiers::NONE, true)]
    // Raw mode means the terminal will not turn this into a SIGINT for us.
    #[case(KeyCode::Char('c'), KeyModifiers::CONTROL, true)]
    #[case(KeyCode::Char('C'), KeyModifiers::CONTROL, true)]
    // A plain 'c' is just a letter.
    #[case(KeyCode::Char('c'), KeyModifiers::NONE, false)]
    #[case(KeyCode::Char('x'), KeyModifiers::NONE, false)]
    #[case(KeyCode::Esc, KeyModifiers::NONE, false)]
    fn test_is_quit(
        #[case] code: KeyCode,
        #[case] modifiers: KeyModifiers,
        #[case] expected: bool,
    ) {
        assert_eq!(
            is_quit(&key(code, modifiers, KeyEventKind::Press)),
            expected,
            "{code:?} with {modifiers:?}"
        );
    }

    #[test]
    fn test_releasing_a_key_is_not_quitting() {
        // On a terminal which reports releases (the kitty protocol), letting go
        // of 'q' must not immediately close whatever comes next.
        assert!(!is_quit(&key(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release
        )));
        assert!(is_quit(&key(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat
        )));
    }

    // --- The loop ----------------------------------------------------------

    /// Drives the real loop over synthetic channels, returning the last frame
    /// it drew.
    async fn loop_with(
        events: mpsc::UnboundedReceiver<TestEvent>,
        keys: mpsc::UnboundedReceiver<KeyResult>,
        quit: CancellationToken,
    ) -> Result<Buffer, crate::Error> {
        let mut terminal =
            Terminal::new(TestBackend::new(60, 10)).expect("the test backend should start");
        drive(&mut terminal, App::new(overview()), events, keys, &quit).await?;

        Ok(terminal.backend().buffer().clone())
    }

    #[tokio::test]
    async fn test_the_loop_draws_what_it_is_sent_and_stops_when_asked() {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (_keys_tx, keys) = mpsc::unbounded_channel();
        let quit = CancellationToken::new();

        events_tx
            .send(TestEvent::Heard("salute".to_string()))
            .expect("the UI should be listening");

        let ui = tokio::spawn(loop_with(events, keys, quit.clone()));

        // The shutdown path everything but 'q' takes: somebody else cancels.
        tokio::time::sleep(Duration::from_millis(50)).await;
        quit.cancel();

        let buffer = tokio::time::timeout(Duration::from_secs(5), ui)
            .await
            .expect("the UI should stop when the token is cancelled")
            .expect("the UI should not panic")
            .expect("the UI should stop cleanly");

        assert!(
            (3..8).any(|y| row(&buffer, y).contains("heard: \"salute\"")),
            "the event should have been drawn"
        );
    }

    #[tokio::test]
    async fn test_quitting_stops_the_ui_and_the_rest_of_the_rehearsal() {
        let (_events_tx, events) = mpsc::unbounded_channel();
        let (keys_tx, keys) = mpsc::unbounded_channel();
        let quit = CancellationToken::new();

        let ui = tokio::spawn(loop_with(events, keys, quit.clone()));
        keys_tx
            .send(Ok(key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )))
            .expect("the UI should be listening");

        tokio::time::timeout(Duration::from_secs(5), ui)
            .await
            .expect("'q' should stop the UI")
            .expect("the UI should not panic")
            .expect("the UI should stop cleanly");

        assert!(
            quit.is_cancelled(),
            "quitting the UI must stop the rehearsal with it"
        );
    }

    #[tokio::test]
    async fn test_a_broken_keyboard_is_reported_in_the_log() {
        let (_events_tx, events) = mpsc::unbounded_channel();
        let (keys_tx, keys) = mpsc::unbounded_channel();
        let quit = CancellationToken::new();

        let ui = tokio::spawn(loop_with(events, keys, quit.clone()));
        keys_tx
            .send(Err("We stopped watching the keyboard (broken).".to_string()))
            .expect("the UI should be listening");

        tokio::time::sleep(Duration::from_millis(50)).await;
        quit.cancel();

        let buffer = tokio::time::timeout(Duration::from_secs(5), ui)
            .await
            .expect("the UI should stop when cancelled")
            .expect("the UI should not panic")
            .expect("the UI should stop cleanly");

        assert!(
            (3..8).any(|y| row(&buffer, y).contains("error: We stopped watching the keyboard")),
            "a reader failure should be logged where the user can see it"
        );
    }

    #[tokio::test]
    async fn test_the_loop_keeps_running_when_the_pipeline_stops_reporting() {
        // A closed event channel must leave the UI up (so the log can still be
        // read and quit out of), not spin on a permanently-ready receiver.
        let (events_tx, events) = mpsc::unbounded_channel::<TestEvent>();
        let (keys_tx, keys) = mpsc::unbounded_channel::<KeyResult>();
        let quit = CancellationToken::new();
        drop(events_tx);
        drop(keys_tx);

        let ui = tokio::spawn(loop_with(events, keys, quit.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!ui.is_finished(), "the UI should still be up");

        quit.cancel();
        tokio::time::timeout(Duration::from_secs(5), ui)
            .await
            .expect("the UI should stop when cancelled")
            .expect("the UI should not panic")
            .expect("the UI should stop cleanly");
    }
}
