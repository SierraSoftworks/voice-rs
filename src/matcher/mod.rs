//! The matcher: walks recognized utterances through the phrase trie and
//! resolves ambiguous prefixes with the completion-timeout state machine.
//! See DESIGN.md §"Matcher: trie + completion timeout".
//!
//! With eager matching off ([`MatcherOptions::eager`]), commands fire on
//! `Final` results only and partials are used solely to hold a pending timer
//! open — exactly the original state machine. With it on (the default),
//! commands additionally fire from *stable partial hypotheses*: a command the
//! greedy walk has resynced past fires immediately, an unambiguous resting
//! match fires once the hypothesis holds still for `eager_delay`, and an
//! ambiguous resting match starts its completion wait at the partial rather
//! than at finalization. The eventual `Final` is reconciled against what
//! already fired (see [`matcher_task`]'s `on_final`).

// Consumed by `commands/run.rs` when the pipeline assembly lands.
#![allow(dead_code)]

pub mod trie;

pub use trie::{CommandId, CompiledCommand, PhraseTrie};

use std::sync::Arc;
use std::time::Duration;

use crate::output::CompiledOutput;
use crate::recognition::{RecognitionEvent, Utterance};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

/// Where the matcher's user-facing warnings go (eager mismatches, suppressed
/// utterances). The pipeline adapts this onto the session's event sink so the
/// warnings reach the terminal UI and plain reports as `warning:` lines; the
/// matcher itself stays ignorant of any UI.
pub type WarningSink = Arc<dyn Fn(String) + Send + Sync>;

/// How the matcher resolves what it hears — the profile's `recognition:`
/// block plus `completion_timeout`, threaded through as one struct so the
/// eager escape hatch is a field rather than a bool soup.
#[derive(Clone)]
pub struct MatcherOptions {
    /// How long an ambiguous match waits for the speaker to continue.
    pub completion_timeout: Duration,
    /// Whether commands may fire from stable partial hypotheses at all. With
    /// this off the matcher behaves exactly as it originally did: `Final`-only
    /// firing, partials only ever extending a pending deadline.
    pub eager: bool,
    /// How long an unambiguous partial must hold still before it fires.
    pub eager_delay: Duration,
    /// Suppress an utterance whose n-best list contains a different-command
    /// alternative within this confidence margin of the winner.
    pub confidence_margin: f32,
    /// Where user-facing warnings are reported.
    pub warn: WarningSink,
}

impl MatcherOptions {
    /// The baseline configuration: `Final`-only firing with the given
    /// completion timeout, no eager path, no confidence gating, warnings
    /// only logged. What every pre-existing call site and test means.
    pub fn with_timeout(completion_timeout: Duration) -> Self {
        Self {
            completion_timeout,
            eager: false,
            eager_delay: Duration::from_millis(100),
            confidence_margin: 3.0,
            warn: Arc::new(|_| {}),
        }
    }

    /// The options a profile's `completion_timeout` and `recognition:` block
    /// add up to.
    pub fn from_profile(profile: &crate::config::Profile, warn: WarningSink) -> Self {
        Self {
            completion_timeout: profile.completion_timeout,
            eager: profile.recognition.eager(),
            eager_delay: profile.recognition.eager_delay,
            confidence_margin: profile.recognition.confidence_margin,
            warn,
        }
    }
}

/// A recognized command ready for execution, flowing through the command
/// queue from the matcher to the executor.
#[derive(Debug, Clone)]
pub struct CommandAction {
    /// The command's display name, for logging.
    pub command: String,
    /// The pre-compiled output plan to execute.
    pub output: CompiledOutput,
}

/// The completion-timeout state machine's state.
#[derive(Debug, Clone, Copy)]
enum MatchState {
    /// Nothing is waiting; the next `Final` walks from the trie root.
    Idle,
    /// An utterance came to rest on an ambiguous terminal: `command` is
    /// matched and ready to fire, but the speaker may still be mid-way
    /// through a longer phrase continuing from `node`.
    Pending {
        command: CommandId,
        node: usize,
        deadline: Instant,
    },
}

impl MatchState {
    fn deadline(&self) -> Option<Instant> {
        match self {
            MatchState::Idle => None,
            MatchState::Pending { deadline, .. } => Some(*deadline),
        }
    }
}

/// The eager path's per-utterance context: `Some` from an utterance's first
/// partial until its `Final` resolves (or a mute clears it), and only when
/// eager matching is on.
///
/// Invariant: while a context is open, `state` is `Idle` — a command pending
/// from the previous utterance is absorbed into `start`/`passed` when the
/// context opens, and the continuation logic drives its fate from there.
#[derive(Debug)]
struct EagerContext {
    /// The trie node this utterance's walks start from: the pending node when
    /// the utterance opened against a Pending command, else the root.
    start: usize,
    /// The pending command carried in from the previous utterance, if any. It
    /// was confirmed by its own `Final`; whether it fires, flushes or is
    /// superseded is decided by walking this utterance from `start`.
    passed: Option<CommandId>,
    /// `(position, command)` pairs already fired from this utterance's
    /// partials, in firing order — `position` is the index just past the
    /// command's last word. `Final` reconciliation checks these against the
    /// finalized walk, and repeated partial walks use them to never fire the
    /// same position twice.
    fired: Vec<(usize, CommandId)>,
    /// The terminal the latest partial's walk rests on, with its armed
    /// deadline. `None` when the walk rests mid-trie or at the root.
    resting: Option<EagerResting>,
}

/// A terminal a partial hypothesis is resting on, waiting out a deadline.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EagerResting {
    command: CommandId,
    node: usize,
    /// The index just past the command's last word in the partial.
    position: usize,
    /// Whether the node is ambiguous. Ambiguous rests wait out the completion
    /// timeout (armed when the walk *first* rests here); unambiguous rests
    /// wait out `eager_delay` (re-armed on every changed partial).
    ambiguous: bool,
    deadline: Instant,
}

/// Where a walk over a finalized utterance came to rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkEnd {
    /// The utterance was fully resolved (fired and/or dropped); go idle.
    Complete,
    /// The utterance rests on an ambiguous terminal: `command` is ready to
    /// fire once the completion timeout elapses, unless a continuation from
    /// `node` supersedes it.
    Pending { command: CommandId, node: usize },
}

/// Consumes [`RecognitionEvent`]s and produces [`CommandAction`]s onto the
/// command queue, resolving ambiguous prefixes with `completion_timeout`.
///
/// Runs until `cancel` fires, the events channel closes (the recognizer shut
/// down), or the command queue closes (the executor shut down) — all of which
/// end the task cleanly.
pub async fn matcher_task(
    trie: PhraseTrie,
    commands: Vec<CompiledCommand>,
    options: MatcherOptions,
    mut events: mpsc::Receiver<RecognitionEvent>,
    queue: mpsc::Sender<CommandAction>,
    cancel: CancellationToken,
) -> Result<(), crate::Error> {
    let mut state = MatchState::Idle;
    // The eager path's open utterance, when there is one. See [`EagerContext`]
    // for the invariant tying it to `state`.
    let mut eager: Option<EagerContext> = None;

    loop {
        // At most one deadline is armed at a time: an open eager context's
        // resting deadline (state is Idle then), or a pending command's
        // completion deadline.
        let deadline = eager
            .as_ref()
            .and_then(|ctx| ctx.resting.as_ref().map(|rest| rest.deadline))
            .or(state.deadline());

        tokio::select! {
            _ = cancel.cancelled() => {
                // Cancellation is a shutdown demanded from outside the
                // pipeline (signal, child exit): like a mute, it must not
                // press keys under the user, so a pending command (or an open
                // eager hypothesis) is dropped.
                if let MatchState::Pending { command, .. } = state {
                    debug!(
                        command = %commands[command.0].name,
                        "Dropping the pending command '{}': shutdown was requested.",
                        commands[command.0].name
                    );
                }
                if eager.is_some() {
                    debug!("Dropping an open eager hypothesis: shutdown was requested.");
                }
                debug!("Shutdown requested, stopping the matcher.");
                return Ok(());
            }
            _ = deadline_elapsed(deadline) => {
                if let Some(ctx) = eager.as_mut() {
                    // The partial hypothesis held still long enough: the
                    // resting command fires, and the utterance stays open —
                    // later partials may still extend it, and the eventual
                    // Final reconciles against `fired`.
                    if let Some(rest) = ctx.resting.take() {
                        ctx.fired.push((rest.position, rest.command));
                        if !fire(&commands, rest.command, &queue).await {
                            return Ok(());
                        }
                    }
                } else if let MatchState::Pending { command, .. } =
                    std::mem::replace(&mut state, MatchState::Idle)
                    && !fire(&commands, command, &queue).await
                {
                    // The speaker paused long enough: the pending command is
                    // the one they meant.
                    return Ok(());
                }
            }
            event = events.recv() => match event {
                Some(RecognitionEvent::Final(utterance)) => {
                    if !on_final(
                        &trie, &commands, &options, &mut state, &mut eager, &utterance, &queue,
                    )
                    .await
                    {
                        return Ok(());
                    }
                }
                Some(RecognitionEvent::Partial(text)) => {
                    if options.eager {
                        if !on_eager_partial(
                            &trie, &commands, &options, &mut state, &mut eager, &text, &queue,
                        )
                        .await
                        {
                            return Ok(());
                        }
                    } else if let MatchState::Pending { node, deadline, .. } = &mut state {
                        // Eager off: a partial only ever *extends* a pending
                        // deadline. The pending state came from a previously
                        // *finalized* utterance, so any new partial is the
                        // start of a fresh hypothesis — its first word
                        // extending from the pending node means the speaker is
                        // mid-way through the longer phrase, and the short
                        // command must not fire under them.
                        let words = words_of(&text);
                        if let Some(first) = words.first()
                            && trie.step(*node, first).is_some()
                        {
                            *deadline = Instant::now() + options.completion_timeout;
                        }
                    }
                }
                Some(RecognitionEvent::Failed) => {
                    // A frame the recognizer could not decode says nothing
                    // about what was said, so it must not disturb a pending
                    // command or an open eager hypothesis: the words either
                    // side of it still add up to the phrase the speaker is
                    // part-way through. The session's UI is where this is
                    // reported.
                }
                Some(RecognitionEvent::Muted) => {
                    // A half-confirmed command must not fire when listening
                    // resumes — and neither may a partial hypothesis.
                    if let MatchState::Pending { command, .. } = state {
                        debug!(
                            command = %commands[command.0].name,
                            "Dropping the pending command '{}': listening was turned off.",
                            commands[command.0].name
                        );
                    }
                    if let Some(ctx) = &eager
                        && (ctx.passed.is_some() || ctx.resting.is_some())
                    {
                        debug!("Dropping an open eager hypothesis: listening was turned off.");
                    }
                    state = MatchState::Idle;
                    eager = None;
                }
                None => {
                    // The recognizer closed the events channel: the pipeline
                    // is shutting down of its own accord. Unlike cancellation
                    // or a mute, a pending command here was fully spoken and
                    // confirmed by a Final — only its settle time was cut
                    // short — so it fires rather than being swallowed. A bare
                    // partial hypothesis was *not* confirmed by anything and
                    // is dropped.
                    if let Some(ctx) = eager.take() {
                        // The absorbed pending command fires only while its
                        // fate is still undecided: anything this utterance
                        // already fired either flushed it (it is in `fired`
                        // at position 0) or superseded it.
                        if let Some(command) = ctx.passed
                            && ctx.fired.is_empty()
                        {
                            fire(&commands, command, &queue).await;
                        }
                        if ctx.resting.is_some() {
                            debug!("Dropping an unconfirmed partial hypothesis: the recognition channel closed.");
                        }
                    } else if let MatchState::Pending { command, .. } = state {
                        fire(&commands, command, &queue).await;
                    }
                    debug!("The recognition channel was closed, stopping the matcher.");
                    return Ok(());
                }
            }
        }
    }
}

/// Handles a finalized utterance: confidence gating, the full walk, and — when
/// the eager path already fired from this utterance's partials —
/// reconciliation. Returns whether the command queue is still open.
#[allow(clippy::too_many_arguments)] // the matcher loop's split-out arms share its whole state
async fn on_final(
    trie: &PhraseTrie,
    commands: &[CompiledCommand],
    options: &MatcherOptions,
    state: &mut MatchState,
    eager: &mut Option<EagerContext>,
    utterance: &Utterance,
    queue: &mpsc::Sender<CommandAction>,
) -> bool {
    // The walk origin: an open eager context pins it (this utterance's
    // partials already walked from there, and may have fired), otherwise the
    // pending state decides exactly as it always has. Either way the utterance
    // is resolved by this Final, so the context ends here.
    let context = eager.take();
    let (start, passed) = match &context {
        Some(ctx) => (ctx.start, ctx.passed),
        None => match *state {
            MatchState::Pending { command, node, .. } => (node, Some(command)),
            MatchState::Idle => (PhraseTrie::ROOT, None),
        },
    };

    // Confidence gating first: when a close runner-up would have run
    // different commands, the whole utterance is suppressed — firing nothing
    // beats firing a coin-flip. (Config validation keeps `alternatives` and
    // eager firing apart, so no eager fire can precede this check.)
    if let Some((top, competitor, margin)) =
        close_ambiguity(trie, utterance, start, passed, options.confidence_margin)
    {
        let message = format!("ambiguous: {top:?} vs {competitor:?} (margin {margin:.1})");
        warn!("Suppressing an utterance: {message}");
        (options.warn)(message);

        // A suppressed utterance cannot supersede a *previously confirmed*
        // pending command, and cannot be trusted to extend it either. It
        // therefore follows the existing non-extending rule: the pending
        // command — fully spoken and confirmed by its own Final — flushes
        // now, and the matcher goes idle. (The `fired`-is-empty guard only
        // matters if gating and eager firing are ever combined, which config
        // validation currently forbids.)
        if let Some(command) = passed
            && context.as_ref().is_none_or(|ctx| ctx.fired.is_empty())
            && !fire(commands, command, queue).await
        {
            return false;
        }
        *state = MatchState::Idle;
        return true;
    }

    let words = words_of(&utterance.text);
    let (matched, end) = walk(trie, &words, start, passed);

    let (already, resting) = match context {
        Some(ctx) => (ctx.fired, ctx.resting),
        None => (Vec::new(), None),
    };

    if already.len() <= matched.len() && matched[..already.len()] == already[..] {
        // What the partials fired is a prefix of what the utterance says
        // (trivially so with no eager context): fire the remainder.
        for &(_, command) in &matched[already.len()..] {
            if !fire(commands, command, queue).await {
                return false;
            }
        }
        *state = match end {
            WalkEnd::Pending { command, node } => MatchState::Pending {
                command,
                node,
                deadline: pending_deadline(options, resting, command, node, words.len()),
            },
            WalkEnd::Complete => MatchState::Idle,
        };
    } else if let WalkEnd::Pending { command, .. } = end
        && already.len() == matched.len() + 1
        && already[..matched.len()] == matched[..]
        && already[matched.len()] == (words.len(), command)
    {
        // The one extra eager fire is the utterance's own resting command:
        // its completion deadline elapsed mid-utterance, so the ambiguous
        // choice was already made. Nothing fires twice and nothing is held
        // pending — a continuation can no longer supersede a pressed key.
        *state = MatchState::Idle;
    } else {
        // An eager mismatch: keys were pressed for a hypothesis the finalized
        // utterance does not begin with. Nothing can be un-pressed, and
        // firing the "correct" remainder on top of a wrong prefix would only
        // compound the damage — so the rest of the utterance is dropped, and
        // the user is told what happened.
        let pressed: Vec<String> = already
            .iter()
            .map(|&(_, command)| commands[command.0].name.clone())
            .collect();
        let message = format!(
            "eager mismatch: fired {} from a partial hypothesis, but the utterance settled as {:?} — the keys were already pressed, and the rest of the utterance was dropped",
            quoted_list(&pressed),
            utterance.text
        );
        warn!("{message}");
        (options.warn)(message);
        *state = MatchState::Idle;
    }

    true
}

/// The deadline for a command left pending by a `Final`: `completion_timeout`
/// from now — unless the partial path already armed the *same* wait, in which
/// case the earlier deadline stands. The whole point of arming from partials
/// is that the wait starts when the hypothesis first came to rest, not when
/// the endpointer got around to finalizing it.
fn pending_deadline(
    options: &MatcherOptions,
    resting: Option<EagerResting>,
    command: CommandId,
    node: usize,
    position: usize,
) -> Instant {
    let fresh = Instant::now() + options.completion_timeout;
    match resting {
        Some(rest)
            if rest.ambiguous
                && rest.command == command
                && rest.node == node
                && rest.position == position =>
        {
            rest.deadline.min(fresh)
        }
        _ => fresh,
    }
}

/// Handles a partial hypothesis with eager matching on. Returns whether the
/// command queue is still open.
#[allow(clippy::too_many_arguments)] // the matcher loop's split-out arms share its whole state
async fn on_eager_partial(
    trie: &PhraseTrie,
    commands: &[CompiledCommand],
    options: &MatcherOptions,
    state: &mut MatchState,
    eager: &mut Option<EagerContext>,
    text: &str,
    queue: &mpsc::Sender<CommandAction>,
) -> bool {
    let words = words_of(text);

    let ctx = eager.get_or_insert_with(|| {
        // A new utterance is opening. A command still pending from the
        // previous one hands its trie position over: this hypothesis walks on
        // from that node, and the continuation logic itself decides the
        // pending command's fate — which subsumes the old rule where an
        // extending partial merely pushed the pending deadline out.
        let (start, passed) = match std::mem::replace(state, MatchState::Idle) {
            MatchState::Pending { command, node, .. } => (node, Some(command)),
            MatchState::Idle => (PhraseTrie::ROOT, None),
        };
        EagerContext {
            start,
            passed,
            fired: Vec::new(),
            resting: None,
        }
    });

    let (certain, resting) = walk_partial(trie, &words, ctx.start, ctx.passed);

    // Fire whatever the greedy walk has passed and resynced beyond: those
    // commands ended strictly before the partial's last word, so no revision
    // of the words still being spoken can take them back. `fired` keeps a
    // revision which repeats the walk from firing the same position twice.
    for entry in certain {
        if !ctx.fired.contains(&entry) {
            ctx.fired.push(entry);
            if !fire(commands, entry.1, queue).await {
                return false;
            }
        }
    }

    // Where the hypothesis rests decides what is armed:
    // - an unambiguous terminal arms `eager_delay`, re-armed by every changed
    //   partial (the hypothesis must hold perfectly still to be trusted);
    // - an ambiguous terminal arms the completion timeout when the walk FIRST
    //   rests there — a later text change which does not move the resting
    //   point (a trailing "[unk]", say) keeps the armed deadline, because the
    //   wait starting early is the whole latency win;
    // - mid-trie or the root arms nothing.
    ctx.resting = match resting {
        Some((command, node)) => {
            let position = words.len();
            if ctx.fired.contains(&(position, command)) {
                // This exact match already fired from an earlier deadline; a
                // hypothesis merely holding still must not fire it again.
                None
            } else {
                let ambiguous = trie.is_ambiguous(node);
                let unchanged = ctx.resting.as_ref().is_some_and(|prev| {
                    prev.command == command
                        && prev.node == node
                        && prev.position == position
                        && prev.ambiguous == ambiguous
                });
                let deadline = match ctx.resting {
                    Some(prev) if ambiguous && unchanged => prev.deadline,
                    _ if ambiguous => Instant::now() + options.completion_timeout,
                    _ => Instant::now() + options.eager_delay,
                };
                Some(EagerResting {
                    command,
                    node,
                    position,
                    ambiguous,
                    deadline,
                })
            }
        }
        None => None,
    };

    true
}

/// When the utterance's n-best list contains a close competitor which would
/// run *different* commands, returns `(top text, competitor text, margin)` —
/// the utterance should then be suppressed. `None` whenever fewer than two
/// alternatives are present (gating off, or nothing to compare).
///
/// Alternatives resolving to the same command sequence (homophones of the same
/// phrase) or to nothing at all are ignored: they would not have changed what
/// was pressed.
fn close_ambiguity(
    trie: &PhraseTrie,
    utterance: &Utterance,
    start: usize,
    passed: Option<CommandId>,
    margin: f32,
) -> Option<(String, String, f32)> {
    let (top, rest) = utterance.alternatives.split_first()?;
    if rest.is_empty() {
        return None;
    }

    let chosen = resolve(trie, &top.0, start, passed);
    for (text, confidence) in rest {
        let gap = top.1 - confidence;
        if gap > margin {
            continue;
        }
        let competitor = resolve(trie, text, start, passed);
        if !competitor.is_empty() && competitor != chosen {
            return Some((top.0.clone(), text.clone(), gap.abs()));
        }
    }
    None
}

/// The command sequence an alternative's text would run, resolved from the
/// same walk origin the real utterance will use: the full walk's fires plus a
/// resting pending command, positions ignored — two texts which would press
/// the same keys in the same order are the same interpretation.
fn resolve(
    trie: &PhraseTrie,
    text: &str,
    start: usize,
    passed: Option<CommandId>,
) -> Vec<CommandId> {
    let words = words_of(text);
    let (fired, end) = walk(trie, &words, start, passed);
    let mut sequence: Vec<CommandId> = fired.into_iter().map(|(_, command)| command).collect();
    if let WalkEnd::Pending { command, .. } = end {
        sequence.push(command);
    }
    sequence
}

/// Renders a list of command names for a warning line: `"a", "b"` —
/// `(nothing)` when empty, which cannot happen for a mismatch but should not
/// panic if it somehow does.
fn quoted_list(names: &[String]) -> String {
    if names.is_empty() {
        return "(nothing)".to_string();
    }
    names
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sleeps until `deadline`, or forever when there is none — the timer half of
/// the matcher's `select!`.
async fn deadline_elapsed(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Splits recognized text into matchable words: whitespace-separated,
/// defensively lowercased (the recognizer's output already is), with Vosk's
/// out-of-grammar `[unk]` tokens stripped.
fn words_of(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(str::to_lowercase)
        .filter(|word| word != "[unk]")
        .collect()
}

/// Sends `command` onto the queue, returning whether the queue is still open.
/// A closed queue means the executor is gone and the matcher should end.
async fn fire(
    commands: &[CompiledCommand],
    command: CommandId,
    queue: &mpsc::Sender<CommandAction>,
) -> bool {
    let command = &commands[command.0];
    info!(command = %command.name, "Matched the command '{}'.", command.name);
    queue
        .send(CommandAction {
            command: command.name.clone(),
            output: command.output.clone(),
        })
        .await
        .is_ok()
}

/// A command emitted by a walk: the index just past its last word in the
/// utterance, and which command it was.
type Fire = (usize, CommandId);

/// One greedy longest-match pass over `words[from..]`, starting at `start`.
///
/// Every command the pass *resyncs past* — its terminal was crossed and a
/// later word then killed the path — is pushed onto `fired` with the index
/// just past its last word. Returns where the pass came to rest: the final
/// node, plus the most recent terminal crossed on the resting path which has
/// **not** been resynced past (the "trailing" terminal — greedily uncommitted,
/// because the words after it may still be the start of a longer phrase).
///
/// `passed` seeds the pass with a terminal already crossed at `start` (a
/// pending command from a previous utterance), resuming at `from` — so a
/// continuation which fails to extend it flushes it and replays the words
/// from the root, exactly like any other greedy resync.
fn greedy_pass(
    trie: &PhraseTrie,
    words: &[String],
    from: usize,
    start: usize,
    passed: Option<CommandId>,
    fired: &mut Vec<Fire>,
) -> (usize, Option<Fire>) {
    let mut node = start;
    // The most recent terminal crossed on the current path, and the index of
    // the word right after it — where to re-sync once the greedy walk fails.
    let mut last_terminal: Option<(CommandId, usize)> = passed.map(|command| (command, from));
    let mut i = from;

    while i < words.len() {
        if let Some(next) = trie.step(node, &words[i]) {
            node = next;
            if let Some(command) = trie.terminal(next) {
                last_terminal = Some((command, i + 1));
            }
            i += 1;
        } else if let Some((command, resume)) = last_terminal.take() {
            // Greedy longest match: the path crossed a terminal and kept
            // going hoping for a longer phrase; that hope just died, so
            // emit the longest completed phrase and re-sync the words
            // after it from the root.
            fired.push((resume, command));
            node = PhraseTrie::ROOT;
            i = resume;
        } else if node == PhraseTrie::ROOT {
            // An unrecognized word at the root: chatter, drop it.
            debug!(word = %words[i], "Dropping the unmatched word '{}'.", words[i]);
            i += 1;
        } else {
            // The consumed words led nowhere; drop them and retry the
            // current word from the root.
            debug!(
                word = %words[i],
                "Dropping an incomplete phrase and re-syncing at '{}'.", words[i]
            );
            node = PhraseTrie::ROOT;
        }
    }

    (node, last_terminal.map(|(command, pos)| (pos, command)))
}

/// Walks a finalized utterance through the trie with greedy longest-match
/// segmentation, returning the commands to fire (in order, each with the
/// index just past its last word) and where the walk came to rest.
///
/// `start`/`passed` continue a pending walk: `passed` is a terminal already
/// crossed at `start` (the pending command), so a continuation which fails to
/// extend it flushes the pending command before re-syncing from the root.
fn walk(
    trie: &PhraseTrie,
    words: &[String],
    start: usize,
    passed: Option<CommandId>,
) -> (Vec<Fire>, WalkEnd) {
    let mut fired = Vec::new();
    let mut i = 0;
    let mut start = start;
    let mut passed = passed;

    loop {
        let (node, trailing) = greedy_pass(trie, words, i, start, passed, &mut fired);

        // The utterance is exhausted: decide from where the walk rests.
        if let Some(command) = trie.terminal(node) {
            if trie.is_ambiguous(node) {
                // Also a strict prefix of a longer phrase — hold it open for
                // the completion timeout.
                return (fired, WalkEnd::Pending { command, node });
            }
            fired.push((words.len(), command));
            return (fired, WalkEnd::Complete);
        }

        let Some((resume, command)) = trailing else {
            if node != PhraseTrie::ROOT {
                debug!("Dropping an incomplete utterance which ended mid-phrase.");
            }
            return (fired, WalkEnd::Complete);
        };

        // The walk crossed a terminal and then trailed off mid-trie at the
        // end of the utterance. The spec's greedy rule applies here too: emit
        // the completed phrase and re-sync whatever followed it from the root
        // (there is no state for "pending mid-trie", and swallowing a fully
        // spoken command would be worse than dropping the trailing words).
        fired.push((resume, command));
        start = PhraseTrie::ROOT;
        passed = None;
        i = resume;
        if i >= words.len() {
            return (fired, WalkEnd::Complete);
        }
    }
}

/// Walks an in-progress (partial) hypothesis. Unlike [`walk`], only commands
/// the greedy pass has *resynced past* come back as fired — they ended
/// strictly before a later word, so no revision of the words still being
/// spoken can take them back. Everything else stays uncommitted:
///
/// - a terminal the walk crossed and continued beyond without failing (the
///   next words may still grow into the longer phrase) fires nothing;
/// - the resting node is returned when it is itself a terminal, for the
///   caller to arm a deadline on;
/// - a walk resting mid-trie or at the root returns no resting terminal.
fn walk_partial(
    trie: &PhraseTrie,
    words: &[String],
    start: usize,
    passed: Option<CommandId>,
) -> (Vec<Fire>, Option<(CommandId, usize)>) {
    let mut fired = Vec::new();
    let (node, _trailing) = greedy_pass(trie, words, 0, start, passed, &mut fired);
    (fired, trie.terminal(node).map(|command| (command, node)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const TIMEOUT: Duration = Duration::from_millis(300);
    const EAGER_DELAY: Duration = Duration::from_millis(100);

    /// The eager configuration the eager suite runs under.
    fn eager_options() -> MatcherOptions {
        MatcherOptions {
            eager: true,
            eager_delay: EAGER_DELAY,
            ..MatcherOptions::with_timeout(TIMEOUT)
        }
    }

    fn cmd(name: &str, phrases: &[&str]) -> CompiledCommand {
        CompiledCommand {
            name: name.to_string(),
            output: CompiledOutput::Keyboard(Vec::new()),
            phrases: phrases
                .iter()
                .map(|phrase| phrase.split_whitespace().map(str::to_string).collect())
                .collect(),
        }
    }

    /// The standard test arsenal: "autocannon" is an ambiguous prefix of
    /// "autocannon sentry" *via a different command*, "deploy sentry" and
    /// "reload" are unambiguous.
    fn arsenal() -> Vec<CompiledCommand> {
        vec![
            cmd("autocannon", &["autocannon"]),
            cmd("autocannon sentry", &["autocannon sentry"]),
            cmd("deploy sentry", &["deploy sentry"]),
            cmd("reload", &["reload"]),
        ]
    }

    /// Lets the spawned matcher task run (on the paused clock, without
    /// advancing it) until it has processed everything we've sent.
    async fn settle() {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    struct Harness {
        events: mpsc::Sender<RecognitionEvent>,
        actions: mpsc::Receiver<CommandAction>,
        warnings: std::sync::Arc<Mutex<Vec<String>>>,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<Result<(), crate::Error>>,
    }

    impl Harness {
        fn start(commands: Vec<CompiledCommand>) -> Self {
            Self::start_with(commands, MatcherOptions::with_timeout(TIMEOUT))
        }

        fn start_with(commands: Vec<CompiledCommand>, mut options: MatcherOptions) -> Self {
            let trie = PhraseTrie::build(&commands).expect("the test command set should build");
            // Roomy channels so long utterances can fire many commands
            // without the matcher blocking on a full queue mid-test.
            let (events, events_rx) = mpsc::channel(16);
            let (actions_tx, actions) = mpsc::channel(256);
            let cancel = CancellationToken::new();

            // Whatever warnings the matcher raises are captured for the tests
            // to assert on.
            let warnings = std::sync::Arc::new(Mutex::new(Vec::new()));
            options.warn = {
                let warnings = warnings.clone();
                Arc::new(move |message| warnings.lock().unwrap().push(message))
            };

            let handle = tokio::spawn(matcher_task(
                trie,
                commands,
                options,
                events_rx,
                actions_tx,
                cancel.clone(),
            ));

            Harness {
                events,
                actions,
                warnings,
                cancel,
                handle,
            }
        }

        async fn hear_final(&self, text: &str) {
            self.events
                .send(RecognitionEvent::Final(Utterance::plain(text)))
                .await
                .expect("the matcher should still be listening");
            settle().await;
        }

        async fn hear_final_with_alternatives(&self, alternatives: &[(&str, f32)]) {
            let utterance = Utterance {
                text: alternatives
                    .first()
                    .map(|(text, _)| (*text).to_string())
                    .unwrap_or_default(),
                alternatives: alternatives
                    .iter()
                    .map(|&(text, confidence)| (text.to_string(), confidence))
                    .collect(),
            };
            self.events
                .send(RecognitionEvent::Final(utterance))
                .await
                .expect("the matcher should still be listening");
            settle().await;
        }

        async fn hear_partial(&self, text: &str) {
            self.events
                .send(RecognitionEvent::Partial(text.to_string()))
                .await
                .expect("the matcher should still be listening");
            settle().await;
        }

        async fn mute(&self) {
            self.events
                .send(RecognitionEvent::Muted)
                .await
                .expect("the matcher should still be listening");
            settle().await;
        }

        async fn fail(&self) {
            self.events
                .send(RecognitionEvent::Failed)
                .await
                .expect("the matcher should still be listening");
            settle().await;
        }

        async fn advance(&self, duration: Duration) {
            tokio::time::advance(duration).await;
            settle().await;
        }

        /// Drains every command which has fired so far, in order.
        fn fired(&mut self) -> Vec<String> {
            let mut names = Vec::new();
            while let Ok(action) = self.actions.try_recv() {
                names.push(action.command);
            }
            names
        }

        fn nothing_fired(&mut self) {
            assert_eq!(self.fired(), Vec::<&str>::new());
        }

        /// Drains every warning the matcher has raised so far, in order.
        fn warnings(&mut self) -> Vec<String> {
            std::mem::take(&mut *self.warnings.lock().unwrap())
        }

        fn no_warnings(&mut self) {
            assert_eq!(self.warnings(), Vec::<String>::new());
        }

        async fn shutdown(self) {
            self.cancel.cancel();
            self.handle
                .await
                .expect("the matcher task should not panic")
                .expect("the matcher task should end cleanly");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unambiguous_final_fires_immediately() {
        let mut h = Harness::start(arsenal());

        h.hear_final("deploy sentry").await;

        // No time has been advanced: the fire must not wait for any timeout.
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_decode_failure_leaves_the_matcher_exactly_where_it_was() {
        let mut h = Harness::start(arsenal());

        // A failure in the middle of an ambiguous phrase must not fire the
        // pending command early (as a mute would) nor drop it: it carries no
        // words, so there is nothing for the matcher to change its mind about.
        h.hear_final("autocannon").await;
        h.fail().await;
        h.nothing_fired();

        h.advance(TIMEOUT).await;
        assert_eq!(h.fired(), vec!["autocannon"]);

        // And it is not a phrase boundary either: the next utterance matches
        // exactly as it would have without it.
        h.fail().await;
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_final_fires_exactly_after_the_completion_timeout() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.advance(TIMEOUT - Duration::from_millis(1)).await;
        h.nothing_fired();

        h.advance(Duration::from_millis(1)).await;
        assert_eq!(h.fired(), vec!["autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn continuation_supersedes_the_pending_command() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final("sentry").await;
        assert_eq!(h.fired(), vec!["autocannon sentry"]);

        // The superseded short command must not fire later.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn non_extending_final_flushes_the_pending_command_first() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final("reload").await;
        assert_eq!(h.fired(), vec!["autocannon", "reload"]);

        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn extending_partial_pushes_the_deadline_out() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await; // deadline: t0 + 300ms
        h.advance(Duration::from_millis(200)).await;
        h.hear_partial("sentry").await; // extends: deadline now t0 + 500ms

        // Advance past the ORIGINAL deadline: nothing may fire.
        h.advance(Duration::from_millis(150)).await; // t0 + 350ms
        h.nothing_fired();

        // Let the pushed-out deadline pass: the short command fires.
        h.advance(Duration::from_millis(150)).await; // t0 + 500ms
        assert_eq!(h.fired(), vec!["autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn non_extending_partial_leaves_the_deadline_alone() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await; // deadline: t0 + 300ms
        h.advance(Duration::from_millis(200)).await;
        h.hear_partial("reload").await; // does not extend from the pending node
        h.nothing_fired();

        // The original deadline still stands.
        h.advance(Duration::from_millis(100)).await; // t0 + 300ms
        assert_eq!(h.fired(), vec!["autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn muted_clears_the_pending_command_and_all_walk_state() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await;
        h.mute().await;

        // The half-confirmed command must not fire when the timer would have.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();

        // The walk state was reset too: "sentry" no longer continues
        // anything, and matching still works from a clean root.
        h.hear_final("sentry").await;
        h.nothing_fired();
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn unk_tokens_are_stripped() {
        let mut h = Harness::start(arsenal());

        h.hear_final("[unk] deploy sentry [unk]").await;

        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn greedy_segmentation_fires_multiple_commands_from_one_utterance() {
        let mut h = Harness::start(arsenal());

        h.hear_final("deploy sentry reload").await;

        assert_eq!(h.fired(), vec!["deploy sentry", "reload"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn resyncs_from_the_root_past_unknown_leading_words() {
        let mut h = Harness::start(arsenal());

        h.hear_final("hello deploy sentry").await;

        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn resyncs_by_retrying_the_failing_word_from_the_root() {
        let mut h = Harness::start(arsenal());

        // "deploy" consumes a step, then "reload" fails mid-trie with no
        // terminal crossed: the walk must retry "reload" from the root
        // rather than dropping it along with "deploy".
        h.hear_final("deploy reload").await;

        assert_eq!(h.fired(), vec!["reload"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_phrase_is_dropped_silently() {
        let mut h = Harness::start(arsenal());

        // Only "deploy sentry" exists; "deploy" alone rests mid-trie with no
        // terminal on the path.
        h.hear_final("deploy").await;
        h.nothing_fired();

        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguity_is_per_node_across_different_commands() {
        // Command A = "autocannon", command B = "autocannon sentry": the
        // ambiguity lives on the node, not on either command.
        let mut h = Harness::start(arsenal());

        // Left alone, Pending holds A and the timer fires A.
        h.hear_final("autocannon").await;
        h.nothing_fired();
        h.advance(TIMEOUT).await;
        assert_eq!(h.fired(), vec!["autocannon"]);

        // Continued in time, the continuation fires B (and only B).
        h.hear_final("autocannon").await;
        h.hear_final("sentry").await;
        assert_eq!(h.fired(), vec!["autocannon sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_does_not_fire_a_pending_command() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.cancel.cancel();
        settle().await;

        let Harness {
            mut actions,
            handle,
            events: _events,
            cancel: _cancel,
            warnings: _warnings,
        } = h;
        handle
            .await
            .expect("the matcher task should not panic")
            .expect("the matcher task should end cleanly");

        assert!(
            actions.try_recv().is_err(),
            "a pending command must not fire on cancellation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn closed_events_channel_fires_a_confirmed_pending_command() {
        // Unlike cancellation or a mute, the events channel closing means the
        // pipeline is winding down normally — the pending command was fully
        // spoken and confirmed, so it must not be swallowed.
        let h = Harness::start(arsenal());
        h.hear_final("autocannon").await;

        let Harness {
            events,
            mut actions,
            cancel: _cancel,
            warnings: _warnings,
            handle,
        } = h;
        drop(events);

        handle
            .await
            .expect("the matcher task should not panic")
            .expect("the matcher task should end cleanly");

        assert_eq!(
            actions
                .try_recv()
                .expect("the pending command fires")
                .command,
            "autocannon"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_continuation_which_stalls_mid_trie_flushes_the_pending_command() {
        // A = "autocannon", B = "autocannon sentry gun": the continuation
        // "sentry" extends from the pending node but never reaches B's
        // terminal. A was fully spoken, so it fires; the stray "sentry" is
        // dropped.
        let mut h = Harness::start(vec![
            cmd("autocannon", &["autocannon"]),
            cmd("autocannon sentry gun", &["autocannon sentry gun"]),
        ]);

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final("sentry").await;
        assert_eq!(h.fired(), vec!["autocannon"]);

        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn noise_final_keeps_the_pending_command_waiting() {
        // An all-[unk] Final means the recognizer heard something that wasn't
        // a grammar word; the speaker may still be finishing the longer
        // phrase, so the pending command re-arms rather than firing or
        // being dropped.
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await; // deadline: t0 + 300ms
        h.advance(Duration::from_millis(200)).await;
        h.hear_final("[unk]").await; // re-arms: deadline now t0 + 500ms

        h.advance(Duration::from_millis(150)).await; // t0 + 350ms
        h.nothing_fired();

        h.advance(Duration::from_millis(150)).await; // t0 + 500ms
        assert_eq!(h.fired(), vec!["autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn long_junk_utterances_are_handled_robustly() {
        let mut h = Harness::start(arsenal());

        // 200 words of junk — including grammar-word prefixes ("deploy",
        // "autocannon") which force mid-trie resyncs — then a real command.
        let mut words: Vec<String> = (0..200)
            .map(|i| match i % 5 {
                0 => "deploy".to_string(),
                1 => "autocannon".to_string(),
                _ => format!("junk{i}"),
            })
            .collect();
        words.push("deploy".to_string());
        words.push("sentry".to_string());
        let utterance = words.join(" ");

        h.hear_final(&utterance).await;

        // Every stray "autocannon" followed by junk fires the short command
        // (greedy longest-match crossed its terminal); every stray "deploy"
        // is an incomplete phrase and drops. The final "deploy sentry" fires.
        let fired = h.fired();
        assert_eq!(
            fired.last().map(String::as_str),
            Some("deploy sentry"),
            "the trailing command must fire: {fired:?}"
        );
        assert!(
            fired[..fired.len() - 1]
                .iter()
                .all(|name| name == "autocannon"),
            "only the stray autocannons may fire besides it: {fired:?}"
        );
        h.shutdown().await;
    }

    // --- Eager (partial-driven) matching ----------------------------------

    #[tokio::test(start_paused = true)]
    async fn eager_unambiguous_partial_fires_after_the_delay_with_no_final() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_partial("deploy sentry").await;
        h.nothing_fired();

        h.advance(EAGER_DELAY - Duration::from_millis(1)).await;
        h.nothing_fired();

        // The command fires without any Final ever arriving.
        h.advance(Duration::from_millis(1)).await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);

        // When the Final does arrive, reconciliation skips what already
        // fired — nothing fires twice, and the matcher is left clean.
        h.hear_final("deploy sentry").await;
        h.nothing_fired();
        h.no_warnings();
        h.hear_final("reload").await;
        assert_eq!(h.fired(), vec!["reload"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_certain_prefix_fires_immediately_on_the_partial() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // "deploy sentry" was passed and resynced beyond by "reload": it is
        // certain the moment the partial arrives, with no delay at all.
        h.hear_partial("deploy sentry reload").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);

        // The resting "reload" needs its stability delay.
        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["reload"]);

        h.hear_final("deploy sentry reload").await;
        h.nothing_fired();
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_ambiguous_partial_arms_the_completion_timeout_from_now() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // "autocannon" rests on an ambiguous terminal: the completion wait
        // starts at the partial, not at finalization.
        h.hear_partial("autocannon").await;
        h.nothing_fired();

        h.advance(TIMEOUT - Duration::from_millis(1)).await;
        h.nothing_fired();
        h.advance(Duration::from_millis(1)).await;
        assert_eq!(h.fired(), vec!["autocannon"]);

        // The Final confirms the choice already made: nothing more fires and
        // nothing is held pending.
        h.hear_final("autocannon").await;
        h.nothing_fired();
        h.no_warnings();
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_final_keeps_the_earlier_partial_armed_deadline() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_partial("autocannon").await; // deadline armed: t0 + 300ms
        h.advance(Duration::from_millis(200)).await;
        h.hear_final("autocannon").await; // must NOT re-arm to t0 + 500ms

        // The partial-armed deadline stands: the command fires at t0 + 300ms.
        h.advance(Duration::from_millis(100)).await;
        assert_eq!(h.fired(), vec!["autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_changed_partial_rearms_the_delay() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_partial("deploy sentry").await;
        h.advance(EAGER_DELAY - Duration::from_millis(10)).await;

        // The hypothesis changed (even though the matchable words did not:
        // "[unk]" is stripped) — it has to hold still all over again.
        h.hear_partial("deploy sentry [unk]").await;
        h.advance(EAGER_DELAY - Duration::from_millis(10)).await;
        h.nothing_fired();

        h.advance(Duration::from_millis(10)).await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_revised_partial_disarms_the_deadline() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_partial("deploy sentry").await;
        // The recognizer changed its mind before the delay elapsed: the
        // revised hypothesis rests mid-trie and nothing may fire.
        h.hear_partial("deploy").await;

        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();

        // The Final matches the revised hypothesis; the dropped words fire
        // nothing and warn about nothing.
        h.hear_final("deploy").await;
        h.nothing_fired();
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_final_fires_the_remainder_immediately() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // The partial rests mid-trie: nothing is armed...
        h.hear_partial("deploy").await;
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();

        // ...but the Final completes the phrase and fires it at once.
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_mismatch_on_a_divergent_final_warns_and_drops() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // "reload" is resynced past ("deploy" does not extend it): certain,
        // fired immediately.
        h.hear_partial("reload deploy").await;
        assert_eq!(h.fired(), vec!["reload"]);

        // The finalized utterance says something the fired prefix does not
        // begin with: the keys are already down, so the matcher warns and
        // drops the rest rather than compounding the mistake.
        h.hear_final("deploy sentry").await;
        h.nothing_fired();

        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "one mismatch, one warning: {warnings:?}");
        assert!(
            warnings[0].contains("\"reload\"") && warnings[0].contains("deploy sentry"),
            "the warning should name what fired and what was heard: {}",
            warnings[0]
        );

        // The mismatch leaves no state behind.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_completion_fire_followed_by_a_continuation_is_reported() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // The speaker pauses longer than the completion timeout mid-utterance:
        // the ambiguous choice is made and "autocannon" fires...
        h.hear_partial("autocannon").await;
        h.advance(TIMEOUT).await;
        assert_eq!(h.fired(), vec!["autocannon"]);

        // ...and then they continue after all. The longer command fires too —
        // its keys are what they now asked for — and the Final reports the
        // overrun, because "autocannon" was pressed and cannot be taken back.
        h.hear_partial("autocannon sentry").await;
        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["autocannon sentry"]);

        h.hear_final("autocannon sentry").await;
        h.nothing_fired();
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "the overrun warns once: {warnings:?}");
        assert!(
            warnings[0].contains("\"autocannon\""),
            "the warning should name the early fire: {}",
            warnings[0]
        );
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_muted_clears_the_context_without_firing() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_partial("deploy sentry").await;
        h.mute().await;

        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.no_warnings();

        // Matching still works from a clean slate.
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_multi_command_utterance_fires_in_order() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // A growing hypothesis fires each command as it becomes certain.
        h.hear_partial("deploy").await;
        h.nothing_fired();
        h.hear_partial("deploy sentry").await;
        h.nothing_fired(); // resting, not yet stable
        h.hear_partial("deploy sentry reload").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]); // resynced past: certain

        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["reload"]);

        h.hear_final("deploy sentry reload").await;
        h.nothing_fired();
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_partial_extending_a_pending_command_supersedes_it() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_final("autocannon").await; // Pending, deadline t0 + 300ms
        h.advance(Duration::from_millis(250)).await;

        // The next utterance's partial extends the pending node: the
        // continuation logic takes over, the pending deadline is disarmed,
        // and the longer command fires after its own stability delay.
        h.hear_partial("sentry").await;
        h.advance(Duration::from_millis(99)).await; // t0 + 349ms: past the old deadline
        h.nothing_fired();

        h.advance(Duration::from_millis(1)).await; // partial + EAGER_DELAY
        assert_eq!(h.fired(), vec!["autocannon sentry"]);

        // The superseded short command never fires, and the Final agrees.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.hear_final("sentry").await;
        h.nothing_fired();
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_non_extending_partial_flushes_the_pending_command() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_final("autocannon").await;
        h.nothing_fired();

        // "reload" cannot extend the pending node: the pending command was
        // fully spoken, so it flushes immediately — the eager equivalent of
        // the non-extending Final rule, minus the wait.
        h.hear_partial("reload").await;
        assert_eq!(h.fired(), vec!["autocannon"]);

        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["reload"]);

        h.hear_final("reload").await;
        h.nothing_fired();
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_closed_channel_fires_an_undecided_absorbed_pending() {
        // The channel closing mid-utterance: the pending command absorbed into
        // the open context was confirmed by its Final, and its fate is still
        // undecided — it fires, exactly as it does without the eager path.
        let h = Harness::start_with(arsenal(), eager_options());
        h.hear_final("autocannon").await;
        h.hear_partial("sentry").await; // absorbed; nothing decided yet

        let Harness {
            events,
            mut actions,
            cancel: _cancel,
            warnings: _warnings,
            handle,
        } = h;
        drop(events);

        handle
            .await
            .expect("the matcher task should not panic")
            .expect("the matcher task should end cleanly");

        assert_eq!(
            actions
                .try_recv()
                .expect("the absorbed pending command fires")
                .command,
            "autocannon"
        );
        assert!(
            actions.try_recv().is_err(),
            "the unconfirmed hypothesis must not fire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn eager_closed_channel_does_not_refire_a_superseded_pending() {
        let mut h = Harness::start_with(arsenal(), eager_options());
        h.hear_final("autocannon").await;
        h.hear_partial("sentry").await;
        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["autocannon sentry"]);

        let Harness {
            events,
            mut actions,
            cancel: _cancel,
            warnings: _warnings,
            handle,
        } = h;
        drop(events);

        handle
            .await
            .expect("the matcher task should not panic")
            .expect("the matcher task should end cleanly");

        assert!(
            actions.try_recv().is_err(),
            "the superseded pending command must not fire on close"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn eager_off_is_untouched_by_partials_beyond_the_deadline_push() {
        // The compatibility escape hatch: with eager off, a stable partial
        // fires nothing, ever — only the Final does.
        let mut h = Harness::start(arsenal());

        h.hear_partial("deploy sentry").await;
        h.advance(TIMEOUT * 4).await;
        h.nothing_fired();

        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["deploy sentry"]);
        h.shutdown().await;
    }

    // --- Confidence gating (alternatives) ---------------------------------

    /// Commands for the gating suite: two acoustically-confusable different
    /// commands, and one command with homophone phrases.
    fn gating_arsenal() -> Vec<CompiledCommand> {
        vec![
            cmd("mortar sentry", &["mortar sentry"]),
            cmd("rocket sentry", &["rocket sentry"]),
            cmd("one up", &["one up", "won up"]),
        ]
    }

    #[rstest::rstest]
    // A close competitor resolving to a different command: suppress.
    #[case(&[("mortar sentry", 240.0), ("rocket sentry", 238.8)], Some(1.2))]
    // The same competitor, outside the margin: the winner is clear.
    #[case(&[("mortar sentry", 240.0), ("rocket sentry", 235.0)], None)]
    // Homophones of the same command at identical confidence: same sequence,
    // never suppressed.
    #[case(&[("one up", 240.0), ("won up", 240.0)], None)]
    // A competitor resolving to nothing at all is ignored.
    #[case(&[("mortar sentry", 240.0), ("more tar sen tree", 239.9)], None)]
    // A single alternative has nothing to compete with.
    #[case(&[("mortar sentry", 240.0)], None)]
    // No alternatives at all (gating disabled): nothing to do.
    #[case(&[], None)]
    fn gating_margin_table(#[case] alternatives: &[(&str, f32)], #[case] expected: Option<f32>) {
        let commands = gating_arsenal();
        let trie = PhraseTrie::build(&commands).expect("the gating command set should build");
        let utterance = Utterance {
            text: alternatives
                .first()
                .map(|(text, _)| (*text).to_string())
                .unwrap_or_default(),
            alternatives: alternatives
                .iter()
                .map(|&(text, confidence)| (text.to_string(), confidence))
                .collect(),
        };

        let ambiguity = close_ambiguity(&trie, &utterance, PhraseTrie::ROOT, None, 3.0);

        match expected {
            None => assert!(ambiguity.is_none(), "unexpected suppression: {ambiguity:?}"),
            Some(margin) => {
                let (top, competitor, gap) = ambiguity.expect("the utterance should be suppressed");
                assert_eq!(top, utterance.text);
                assert_ne!(competitor, top);
                assert!(
                    (gap - margin).abs() < 0.01,
                    "unexpected margin {gap} (expected {margin})"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gating_suppresses_a_close_call_and_warns() {
        let mut h = Harness::start_with(gating_arsenal(), MatcherOptions::with_timeout(TIMEOUT));

        h.hear_final_with_alternatives(&[("mortar sentry", 240.0), ("rocket sentry", 238.8)])
            .await;

        h.nothing_fired();
        let warnings = h.warnings();
        assert_eq!(
            warnings,
            vec!["ambiguous: \"mortar sentry\" vs \"rocket sentry\" (margin 1.2)"]
        );

        // Suppression leaves the matcher idle: the next utterance matches
        // exactly as it would from scratch.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.hear_final_with_alternatives(&[("mortar sentry", 240.0), ("rocket sentry", 230.0)])
            .await;
        assert_eq!(h.fired(), vec!["mortar sentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn gating_same_command_homophones_do_not_suppress() {
        // "one"/"won" return byte-identical confidences from Vosk; both
        // phrases belong to the same command, so there is nothing ambiguous
        // about what to press.
        let mut h = Harness::start_with(gating_arsenal(), MatcherOptions::with_timeout(TIMEOUT));

        h.hear_final_with_alternatives(&[("one up", 240.0), ("won up", 240.0)])
            .await;

        assert_eq!(h.fired(), vec!["one up"]);
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn gating_suppression_flushes_a_prior_pending_command() {
        // A pending command from a previous, trusted utterance follows the
        // existing non-extending rule when the next utterance is suppressed:
        // it was fully spoken and confirmed, so it fires; only the suppressed
        // utterance's own commands are withheld.
        let mut commands = gating_arsenal();
        commands.push(cmd("autocannon", &["autocannon"]));
        commands.push(cmd("autocannon sentry", &["autocannon sentry"]));
        let mut h = Harness::start_with(commands, MatcherOptions::with_timeout(TIMEOUT));

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final_with_alternatives(&[("mortar sentry", 240.0), ("rocket sentry", 238.8)])
            .await;
        assert_eq!(h.fired(), vec!["autocannon"]);
        assert_eq!(h.warnings().len(), 1);

        // And the pending state is gone for good.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }
}
