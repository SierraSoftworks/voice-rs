//! The full-screen session UI, shared by `test` and `run`. See DESIGN.md
//! §"The session terminal UI (ratatui)".
//!
//! Three regions, and nothing which scrolls sideways:
//!
//! ```text
//! Helldivers 2                 12 command(s) · 48 phrase(s) · rightctrl (push-to-talk)
//! profiles/helldivers2.yaml                                 wrapping: helldivers2 (pid 4212)
//! ──────────────────────────────────────────────────────────────────────────────────
//! 19:04:13 ● "deploy the autocannon" → Autocannon (leftctrl+4)
//! 19:04:19 ● "reload the thing"
//! 19:04:21 ● helldivers2: Steam initialised
//! ──────────────────────────────────────────────────────────────────────────────────
//! ● listening: on  —  q to quit                            vosk-model-small-en-us-0.15
//! ```
//!
//! A recognition is one entry which upgrades in place (see
//! [`super::event::EventLog::push_at`]): grey while nothing has matched it,
//! green with the command and its key plan once something has. The listening
//! state is not logged at all — the footer shows it live, which is both fewer
//! lines and more current.
//!
//! **Self-update.** Starting this UI also starts a background check for a newer
//! release (see [`crate::update`] and DESIGN.md §"Self-update"); if it finds
//! one, the footer gains a dim `⬆ v1.2.3 — voice-orders update` note on the
//! next draw. Nothing waits for it, a failure says nothing at all, and plain
//! mode — which never reaches this function — never checks.
//!
//! **On tracing and the alternate screen:** this process logs to stdout (see
//! `telemetry.rs` — for a CLI, `info!`/`warn!` are user-facing output), and
//! stdout is the same handle the alternate screen is drawn on, so `main.rs`
//! leaves the console layer out entirely when a TUI is going to own the
//! terminal. The split is by *seam*: everything the session itself reports
//! travels as a [`UiEvent`] and is drawn in the log below, failures included,
//! while the pipeline's own tracing happens before this UI starts and after it
//! has been restored.

use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use crate::commands::run::PipelineSummary;
use crate::config::{Profile, ResolvedSettings};
use crate::hotkey::ListenMode;

use super::event::{DOT, EventLog, SCROLLBACK, UiEvent};

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

/// The glyph which leads the footer's "a newer release is available" note.
const UPDATE_MARK: &str = "⬆";

/// A key event, or the reason we stopped being able to read them.
type KeyResult = Result<KeyEvent, String>;

/// What the header and footer say about the session: the parts which do not
/// change while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Overview {
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
    /// The wrapped application `run` started, if any: its name and pid, as the
    /// header says it. `test` never wraps anything, so this is always [`None`]
    /// there and the header line is the profile's source alone.
    pub wrapping: Option<String>,
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
            wrapping: None,
        }
    }

    /// Notes the application `run` is wrapping, for the header.
    pub fn wrapping(mut self, program: &str, pid: Option<u32>) -> Self {
        self.wrapping = Some(match pid {
            Some(pid) => format!("wrapping: {program} (pid {pid})"),
            None => format!("wrapping: {program}"),
        });
        self
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
pub(crate) struct App {
    overview: Overview,
    log: EventLog,
    listening: bool,
    /// The newest release the background update check found, once it has found
    /// one.
    ///
    /// A watch channel rather than another [`UiEvent`] because the check is not
    /// something the *session* reported: it belongs to the footer, not to the
    /// log, and it needs no redraw machinery of its own — the answer is simply
    /// read on the next draw, which the loop's tick guarantees within a quarter
    /// of a second. [`None`] when nothing is checking at all, which is every
    /// [`App`] but the one [`run`] builds.
    update: Option<watch::Receiver<Option<String>>>,
}

impl App {
    pub fn new(overview: Overview) -> Self {
        Self {
            listening: overview.listening,
            overview,
            log: EventLog::new(SCROLLBACK),
            update: None,
        }
    }

    /// Watches a background update check, so the footer can mention a newer
    /// release once one is found.
    fn watching_for_updates(mut self, updates: watch::Receiver<Option<String>>) -> Self {
        self.update = Some(updates);
        self
    }

    /// The newer release to mention, if the check has finished and found one.
    fn available_update(&self) -> Option<String> {
        self.update.as_ref()?.borrow().clone()
    }

    /// Folds one reported event into the state.
    pub fn record(&mut self, event: UiEvent) {
        self.record_at(std::time::SystemTime::now(), event);
    }

    /// Records an event at a given time, so a rendering test has a stable
    /// screen to assert against.
    ///
    /// A listening change moves the footer and is *not* logged: the footer
    /// already shows the live state, and a hotkey held down through a session
    /// would otherwise fill the log with lines nobody reads.
    pub fn record_at(&mut self, at: std::time::SystemTime, event: UiEvent) {
        if let UiEvent::Listening(now) = event {
            self.listening = now;
            return;
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
pub(crate) async fn run(
    overview: Overview,
    events: mpsc::UnboundedReceiver<UiEvent>,
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

    // The update check lives here, and only here, because this function is
    // exactly the "stdout is a terminal we own" branch of both `test` and
    // `run`: a plain launch — a pipe, CI, Steam — never reaches it and so never
    // makes the request. See DESIGN.md §"Self-update".
    //
    // It runs alongside the session rather than in front of it: nothing waits
    // for it, a failure is silent, and whatever it finds is picked up by the
    // next draw.
    let (found, updates) = watch::channel(None);
    let checker = tokio::spawn({
        let quit = quit.clone();
        async move {
            tokio::select! {
                () = quit.cancelled() => {}
                newer = crate::update::check_for_update() => {
                    if let Some(version) = newer {
                        let _ = found.send(Some(version));
                    }
                }
            }
        }
    });

    let app = App::new(overview).watching_for_updates(updates);
    let outcome = drive(&mut terminal, app, events, keys, &quit).await;

    // Wait for the reader to notice it is done before handing the terminal
    // back, so it cannot swallow a keystroke meant for the shell. The update
    // check is not waited for at all — it is bounded by its own timeout, and a
    // session which is over must not sit here waiting on GitHub.
    quit.cancel();
    checker.abort();
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
    mut events: mpsc::UnboundedReceiver<UiEvent>,
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
                    Err(message) => app.record(UiEvent::Error(message)),
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
pub(crate) fn render(frame: &mut Frame, app: &App) {
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
/// second — with the wrapped application, when `run` has one, on the right of
/// that second line.
fn render_header(frame: &mut Frame, area: Rect, overview: &Overview) {
    let [title, source] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let stats = overview.stats();
    let [name_area, stats_area] = split_row(title, &stats);
    let wrapping = overview.wrapping.clone().unwrap_or_default();
    let [source, wrapping_area] = split_row(source, &wrapping);

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
    frame.render_widget(
        Paragraph::new(Span::styled(wrapping, Style::new().fg(Color::DarkGray))).right_aligned(),
        wrapping_area,
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

/// The live listening state, which model is doing the listening, and — once the
/// background check has found one — the newer release which is waiting.
fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let [state_area, model_area] = split_row(area, &app.overview.model);

    let (state, color) = match (app.overview.has_hotkey, app.listening) {
        (false, _) => ("always listening", Color::Green),
        (true, true) => ("listening: on", Color::Green),
        (true, false) => ("listening: off", Color::Gray),
    };

    let mut footer = vec![
        Span::styled(DOT, Style::new().fg(color)),
        Span::raw(" "),
        Span::styled(state, Style::new().fg(color)),
        Span::styled("  —  q to quit", Style::new().fg(Color::DarkGray)),
    ];

    // Last, and dim: a newer release is worth knowing about but is never the
    // thing the user is watching this screen for, so it is the first thing a
    // narrow terminal truncates away. Nothing is drawn while the check is still
    // running, or when it found nothing.
    if let Some(version) = app.available_update() {
        footer.push(Span::styled(
            format!("  {UPDATE_MARK} v{version} — voice-orders update"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::DIM),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(footer)), state_area);
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
            wrapping: None,
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
                "name: Helldivers 2\nmodel: /models/en\n{yaml}grammar: |\n  Salute = \"salute\" {{ x }}\n"
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
    fn test_the_header_names_the_wrapped_application() {
        // `run`'s one addition to the header: which application this session is
        // wrapping, next to where the profile came from.
        let buffer = screen(
            &App::new(overview().wrapping("helldivers2", Some(4212))),
            90,
            12,
        );

        let source = row(&buffer, 1);
        assert!(
            source.starts_with("profiles/helldivers2.yaml"),
            "the source keeps the left: {source:?}"
        );
        assert!(
            source
                .trim_end()
                .ends_with("wrapping: helldivers2 (pid 4212)"),
            "the wrapped application is right-aligned: {source:?}"
        );

        // `test` wraps nothing, so nothing is added.
        let source = row(&screen(&App::new(overview()), 90, 12), 1);
        assert_eq!(source.trim_end(), "profiles/helldivers2.yaml");
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
        app.record_at(at(), UiEvent::Listening(true));
        let footer = row(&screen(&app, 90, 12), 11);
        assert!(
            footer.starts_with(&format!("{DOT} listening: on")),
            "the footer should follow the hotkey: {footer:?}"
        );
    }

    /// An app whose background update check has already answered: `Some` when
    /// it found a newer release, `None` when it found nothing (or is still
    /// running, which looks the same on screen).
    ///
    /// The sender is dropped immediately on purpose — a watch receiver keeps
    /// serving the last value it was sent, which is exactly the shape of the
    /// real check: it reports once and goes away.
    fn checked(version: Option<&str>) -> App {
        let (_found, updates) = watch::channel(version.map(ToString::to_string));

        App::new(overview()).watching_for_updates(updates)
    }

    #[test]
    fn test_the_footer_mentions_a_newer_release_once_the_check_finds_one() {
        let footer = row(&screen(&checked(Some("1.2.3")), 90, 12), 11);

        assert!(
            footer.contains(&format!("{UPDATE_MARK} v1.2.3 — voice-orders update")),
            "the newer release and how to get it belong in the footer: {footer:?}"
        );
        assert!(
            footer.starts_with(&format!("{DOT} listening: off")),
            "and they must not displace the listening state: {footer:?}"
        );
        assert!(
            footer.ends_with("vosk-model-small-en-us-0.15"),
            "nor the model: {footer:?}"
        );
    }

    #[test]
    fn test_the_footer_says_nothing_when_there_is_no_newer_release() {
        // The two silent cases — the check found nothing, and no check is
        // running at all (a plain-mode session, or any other `App`) — must draw
        // exactly the footer they always have.
        let quiet = row(&screen(&checked(None), 90, 12), 11);
        let unchecked = row(&screen(&App::new(overview()), 90, 12), 11);

        assert_eq!(
            quiet, unchecked,
            "a check which found nothing changes nothing"
        );
        assert!(
            !quiet.contains(UPDATE_MARK),
            "nothing should be drawn: {quiet:?}"
        );
    }

    #[test]
    fn test_the_update_note_is_dim_cyan() {
        // Deliberately the quietest thing on the screen: a session which is
        // mid-game must not have its eye pulled by it.
        let buffer = screen(&checked(Some("1.2.3")), 90, 12);
        let mark = (0..90)
            .map(|x| buffer.cell((x, 11)).expect("inside the buffer"))
            .find(|cell| cell.symbol() == UPDATE_MARK)
            .expect("the footer should carry the update mark");

        assert_eq!(mark.fg, Color::Cyan);
        assert!(mark.modifier.contains(Modifier::DIM));
    }

    #[test]
    fn test_a_tiny_terminal_drops_the_update_note_rather_than_the_state() {
        // The note is last on the line, so a narrow terminal truncates it away
        // and keeps the thing the footer is actually for. (50 columns: the
        // model name claims 27 of them, leaving 23 for the state.)
        let footer = row(&screen(&checked(Some("1.2.3")), 50, 8), 7);

        assert!(
            footer.starts_with(&format!("{DOT} listening: off")),
            "the listening state survives: {footer:?}"
        );
        assert!(
            !footer.contains("voice-orders update"),
            "there is no room for the note: {footer:?}"
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
        app.record_at(at(), UiEvent::Heard("salute".to_string()));

        let footer = row(&screen(&app, 90, 12), 11);
        assert!(
            footer.starts_with(&format!("{DOT} always listening")),
            "unexpected footer: {footer:?}"
        );
    }

    #[test]
    fn test_the_body_logs_events_newest_at_the_bottom() {
        let mut app = App::new(overview());
        // The listening change belongs to the footer, not the log.
        app.record_at(at(), UiEvent::Listening(true));
        app.record_at(at(), UiEvent::Heard("deploy the autocannon".to_string()));
        app.record_at(
            at(),
            UiEvent::Matched {
                name: "Autocannon".to_string(),
                plan: "leftctrl+4".to_string(),
            },
        );
        app.record_at(
            at(),
            UiEvent::Child {
                program: "helldivers2".to_string(),
                line: "Steam initialised".to_string(),
            },
        );

        let buffer = screen(&app, 90, 12);

        // Row 2 is the rule under the header, so the log starts at row 3.
        let lines: Vec<String> = (3..6).map(|y| row(&buffer, y)).collect();
        assert!(
            lines[0].contains(&format!(
                "{DOT} \"deploy the autocannon\" → Autocannon (leftctrl+4)"
            )),
            "the utterance and its match share one upgraded entry: {lines:?}"
        );
        assert!(
            lines[1].contains(&format!("{DOT} helldivers2: Steam initialised")),
            "the newest event is the last line: {lines:?}"
        );
        assert!(
            lines[2].trim().is_empty(),
            "the listening change should not have been logged: {lines:?}"
        );
    }

    #[test]
    fn test_the_footer_follows_listening_without_logging_it() {
        let mut app = App::new(overview());
        for on in [true, false, true] {
            app.record_at(at(), UiEvent::Listening(on));
        }

        let buffer = screen(&app, 90, 12);
        assert_eq!(
            row(&buffer, 3).trim_end(),
            "Speak a command; press q to stop.",
            "a session which has only toggled the hotkey has logged nothing"
        );
        assert!(
            row(&buffer, 11).starts_with(&format!("{DOT} listening: on")),
            "the footer should still carry the live state"
        );
    }

    #[test]
    fn test_the_body_shows_only_the_newest_events_it_has_room_for() {
        let mut app = App::new(overview());
        for i in 0..50 {
            app.record_at(at(), UiEvent::Heard(format!("utterance {i}")));
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
            UiEvent::Matched {
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
        app.record_at(at(), UiEvent::Heard("salute".to_string()));

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
        events: mpsc::UnboundedReceiver<UiEvent>,
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
            .send(UiEvent::Heard("salute".to_string()))
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
            (3..8).any(|y| row(&buffer, y).contains("\"salute\"")),
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
        let (events_tx, events) = mpsc::unbounded_channel::<UiEvent>();
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
