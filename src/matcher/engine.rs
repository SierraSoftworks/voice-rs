//! The grammar v2 matcher engine: the v1 state machine restated over the
//! automaton's hypothesis walk. See DESIGN.md §"Compilation: a word-level
//! transducer" for how the trie semantics generalize.
//!
//! Everything the v1 matcher promised still holds, word for word — greedy
//! longest-match segmentation with re-sync, the `Pending` completion-timeout
//! machine, eager partial-driven firing, and confidence gating — but the walk
//! position is a [`Walk`] (a set of alive hypotheses) instead of a trie node,
//! *ambiguous* means "some hypothesis accepts while any can still consume a
//! word", and a fired command's output is assembled from its evaluated action
//! program at fire time rather than pre-compiled per command.
//!
//! The v1 matcher in the parent module stays untouched until G6 swaps `run`
//! over to this engine and deletes it; the small helpers duplicated from it
//! (`words_of`, `deadline_elapsed`, `quoted_list`) go with it then.

use std::collections::HashMap;

use crate::grammar::v2::{Accept, Automaton, Walk};
use crate::output::CompiledOutput;
use crate::output::assembly::{Pacing, assemble};
use crate::recognition::{RecognitionEvent, Utterance};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

use super::{CommandAction, MatcherOptions};

/// A command matched somewhere in an utterance: which reading matched, and
/// the index just past its last word. Two matches are the same when their
/// positions and their `(rule, actions)` identities agree — the display name
/// is cosmetic and deliberately not part of the comparison.
#[derive(Clone, Debug)]
struct Match {
    /// The index just past the command's last word in the utterance.
    position: usize,
    accept: Accept,
}

/// Accept identity as the matcher compares it: the rule and what it would
/// press. Displays may differ across revisions of the same reading (captures
/// record matched words), so they carry no weight here.
fn same_accept(a: &Accept, b: &Accept) -> bool {
    a.rule == b.rule && a.actions == b.actions
}

fn same_match(a: &Match, b: &Match) -> bool {
    a.position == b.position && same_accept(&a.accept, &b.accept)
}

/// The completion-timeout state machine's state, over walks instead of trie
/// nodes.
#[derive(Debug)]
enum MatchState<'a> {
    /// Nothing is waiting; the next `Final` walks from a fresh root walk.
    Idle,
    /// An utterance came to rest on an ambiguous accept: `accept` is matched
    /// and ready to fire, but the speaker may still be mid-way through a
    /// longer phrase continuing from `walk`.
    Pending {
        accept: Accept,
        /// The resting walk, forked so a continuation resumes exactly where
        /// this utterance stopped.
        walk: Walk<'a>,
        deadline: Instant,
    },
}

impl MatchState<'_> {
    fn deadline(&self) -> Option<Instant> {
        match self {
            MatchState::Idle => None,
            MatchState::Pending { deadline, .. } => Some(*deadline),
        }
    }
}

/// Where a walk over a finalized utterance came to rest.
#[derive(Debug)]
enum WalkEnd<'a> {
    /// The utterance was fully resolved (fired and/or dropped); go idle.
    Complete,
    /// The utterance rests on an ambiguous accept: ready to fire once the
    /// completion timeout elapses, unless a continuation of `walk` supersedes
    /// it.
    Pending { accept: Accept, walk: Walk<'a> },
}

/// Where one greedy pass came to rest.
struct Rest<'a> {
    /// The walk at the resting position.
    walk: Walk<'a>,
    /// The most recent match crossed on the resting path which has *not* been
    /// resynced past — greedily uncommitted, because the words after it may
    /// still grow into a longer phrase. When the walk itself rests on an
    /// accept, this is that accept.
    trailing: Option<Match>,
    /// Whether the resting walk has consumed nothing since its last reset —
    /// i.e. the pass ended at the root rather than mid-phrase.
    fresh: bool,
}

/// The engine's shared, immutable context: the automaton, the pacing applied
/// at fire time, the options, and the command queue. The mutable state lives
/// in [`engine_task`]'s locals so the borrow of the automaton stays simple.
struct Engine<'a> {
    automaton: &'a Automaton,
    /// Rule name → definition order, for picking deterministically when one
    /// resting position carries several distinct accepting rules.
    rule_order: HashMap<String, usize>,
    pacing: Pacing,
    options: MatcherOptions,
    queue: mpsc::Sender<CommandAction>,
}

impl<'a> Engine<'a> {
    fn new(
        automaton: &'a Automaton,
        pacing: Pacing,
        options: MatcherOptions,
        queue: mpsc::Sender<CommandAction>,
    ) -> Self {
        // `rule_sizes` reports published rules in definition order, which is
        // exactly the tie-break order the multi-accept policy needs.
        let rule_order = automaton
            .rule_sizes()
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (name.clone(), index))
            .collect();
        Self {
            automaton,
            rule_order,
            pacing,
            options,
            queue,
        }
    }

    /// Reports a user-facing warning both to the log and through the injected
    /// sink (the session's UI).
    fn warn_user(&self, message: String) {
        warn!("{message}");
        (self.options.warn)(message);
    }

    /// Assembles `accept`'s action program under the profile's pacing and
    /// sends it onto the queue, returning whether the queue is still open. A
    /// closed queue means the executor is gone and the engine should end.
    ///
    /// Assembly happens here — at fire time — because a capture-carrying
    /// command's program only exists once its words are known; there is no
    /// per-command pre-compiled output to reuse.
    async fn fire(&self, accept: &Accept) -> bool {
        info!(command = %accept.display, "Matched the command '{}'.", accept.display);
        self.queue
            .send(CommandAction {
                command: accept.display.clone(),
                output: CompiledOutput::Keyboard(assemble(&accept.actions, &self.pacing)),
            })
            .await
            .is_ok()
    }

    /// The accept the engine commits to when a walk rests on an accepting
    /// position.
    ///
    /// [`Walk::accepts`] can return several distinct readings at one position.
    /// Several rules resting here with *identical* actions are the deliberate
    /// synonym case (load-time duplicate detection lets identical outputs
    /// collapse silently), so the first in rule-definition order fires without
    /// comment. Readings whose actions *differ* are only reachable past
    /// duplicate detection's documented search budget; the engine still picks
    /// the first-defined rule, and warns naming both so the user learns their
    /// grammar is ambiguous at runtime.
    fn chosen(&self, walk: &Walk<'a>, warn: &mut dyn FnMut(String)) -> Accept {
        let accepts = walk.accepts();
        let mut distinct: Vec<&Accept> = Vec::new();
        for accept in &accepts {
            if !distinct.iter().any(|other| same_accept(other, accept)) {
                distinct.push(accept);
            }
        }
        let order = |accept: &Accept| {
            self.rule_order
                .get(&accept.rule)
                .copied()
                .unwrap_or(usize::MAX)
        };
        let chosen = distinct
            .iter()
            .copied()
            .min_by_key(|accept| order(accept))
            .expect("chosen() is only called on a walk with an accept");
        if let Some(divergent) = distinct
            .iter()
            .find(|accept| accept.actions != chosen.actions)
        {
            warn(format!(
                "ambiguous match: {:?} and {:?} both matched what you said but press different keys — we ran {:?}, which is defined first",
                chosen.display, divergent.display, chosen.display
            ));
        }
        chosen.clone()
    }

    /// One greedy longest-match pass over `words[from..]`, starting from a
    /// clone of `origin`.
    ///
    /// Every match the pass *resyncs past* — an accept was crossed and a later
    /// word then killed the walk — is pushed onto `fired`. Returns where the
    /// pass came to rest ([`Rest`]).
    ///
    /// `passed` seeds the pass with an accept already crossed at `origin` (a
    /// pending command from a previous utterance), so a continuation which
    /// fails to extend it flushes it and replays the words from the root,
    /// exactly like any other greedy resync. `origin_fresh` says whether
    /// `origin` has consumed anything yet — a fresh walk failing a word means
    /// the word itself is chatter to drop, while a mid-phrase failure means
    /// the consumed words led nowhere and the current word deserves a retry
    /// from the root.
    ///
    /// A hypothesis overflow ([`Walk::warning`]) is reported through `warn`
    /// and then treated exactly like a dead walk: the words it was following
    /// are beyond saving, but anything already committed still fires and the
    /// remainder re-syncs from the root.
    #[allow(clippy::too_many_arguments)] // the walk arms share the engine's whole traversal state
    fn greedy_pass(
        &self,
        words: &[String],
        from: usize,
        origin: &Walk<'a>,
        origin_fresh: bool,
        passed: Option<&Accept>,
        fired: &mut Vec<Match>,
        warn: &mut dyn FnMut(String),
    ) -> Rest<'a> {
        let mut walk = origin.clone();
        let mut fresh = origin_fresh;
        // The most recent accept crossed on the current path, and the index of
        // the word right after it — where to re-sync once the greedy walk
        // fails.
        let mut last: Option<Match> = passed.map(|accept| Match {
            position: from,
            accept: accept.clone(),
        });
        let mut i = from;

        while i < words.len() {
            walk.step(&words[i]);
            if let Some(warning) = walk.warning() {
                warn(warning.to_owned());
            }
            if !walk.is_dead() {
                fresh = false;
                if walk.has_accept() {
                    // Greedy longest match: remember the accept and keep
                    // walking in the hope of a longer phrase. The accept is
                    // evaluated *now* because a later word may kill the walk,
                    // after which it can no longer be queried.
                    last = Some(Match {
                        position: i + 1,
                        accept: self.chosen(&walk, warn),
                    });
                }
                i += 1;
            } else if let Some(matched) = last.take() {
                // The hope for a longer phrase just died: emit the longest
                // completed phrase and re-sync the words after it from the
                // root.
                let resume = matched.position;
                fired.push(matched);
                walk.reset();
                fresh = true;
                i = resume;
            } else if fresh {
                // An unrecognized word straight from the root: chatter, drop
                // it.
                debug!(word = %words[i], "Dropping the unmatched word '{}'.", words[i]);
                walk.reset();
                i += 1;
            } else {
                // The consumed words led nowhere; drop them and retry the
                // current word from the root.
                debug!(
                    word = %words[i],
                    "Dropping an incomplete phrase and re-syncing at '{}'.", words[i]
                );
                walk.reset();
                fresh = true;
            }
        }

        Rest {
            walk,
            trailing: last,
            fresh,
        }
    }

    /// Walks a finalized utterance with greedy longest-match segmentation,
    /// returning the matches to fire (in order) and where the walk came to
    /// rest.
    ///
    /// `origin`/`passed` continue a pending walk: `passed` is an accept
    /// already crossed at `origin` (the pending command), so a continuation
    /// which fails to extend it flushes the pending command before re-syncing
    /// from the root.
    fn walk_final(
        &self,
        words: &[String],
        origin: &Walk<'a>,
        origin_fresh: bool,
        passed: Option<&Accept>,
        warn: &mut dyn FnMut(String),
    ) -> (Vec<Match>, WalkEnd<'a>) {
        let mut fired = Vec::new();
        let mut i = 0;
        let mut origin = origin.clone();
        let mut origin_fresh = origin_fresh;
        let mut passed = passed.cloned();

        loop {
            let rest = self.greedy_pass(
                words,
                i,
                &origin,
                origin_fresh,
                passed.as_ref(),
                &mut fired,
                warn,
            );

            // The utterance is exhausted: decide from where the walk rests.
            if rest.walk.has_accept() {
                // A resting accept is always the trailing match: the step
                // that reached it recorded it (or, for an empty continuation,
                // the pending seed *is* it).
                let trailing = rest
                    .trailing
                    .expect("a resting accept is always tracked as the trailing match");
                if rest.walk.is_ambiguous() {
                    // Also extendable into a longer phrase — hold it open for
                    // the completion timeout.
                    return (
                        fired,
                        WalkEnd::Pending {
                            accept: trailing.accept,
                            walk: rest.walk,
                        },
                    );
                }
                fired.push(Match {
                    position: words.len(),
                    accept: trailing.accept,
                });
                return (fired, WalkEnd::Complete);
            }

            let Some(matched) = rest.trailing else {
                if !rest.fresh {
                    debug!("Dropping an incomplete utterance which ended mid-phrase.");
                }
                return (fired, WalkEnd::Complete);
            };

            // The walk crossed an accept and then trailed off mid-phrase at
            // the end of the utterance. The greedy rule applies here too: emit
            // the completed phrase and re-sync whatever followed it from the
            // root (there is no state for "pending mid-phrase", and swallowing
            // a fully spoken command would be worse than dropping the trailing
            // words).
            let resume = matched.position;
            fired.push(matched);
            origin = self.automaton.walk();
            origin_fresh = true;
            passed = None;
            i = resume;
            if i >= words.len() {
                return (fired, WalkEnd::Complete);
            }
        }
    }

    /// Handles a finalized utterance: the full walk, firing, and the pending
    /// transition. Returns whether the command queue is still open.
    async fn on_final(&self, state: &mut MatchState<'a>, utterance: &Utterance) -> bool {
        // The walk origin: the pending state decides exactly as v1 did — a
        // pending command hands over its resting walk and rides along as the
        // already-crossed accept.
        let (origin, origin_fresh, passed) = match &*state {
            MatchState::Pending { accept, walk, .. } => (walk.clone(), false, Some(accept.clone())),
            MatchState::Idle => (self.automaton.walk(), true, None),
        };

        let words = words_of(&utterance.text);
        let mut warn = |message: String| self.warn_user(message);
        let (matched, end) =
            self.walk_final(&words, &origin, origin_fresh, passed.as_ref(), &mut warn);

        for matched in &matched {
            if !self.fire(&matched.accept).await {
                return false;
            }
        }
        *state = match end {
            WalkEnd::Pending { accept, walk } => MatchState::Pending {
                accept,
                walk,
                deadline: Instant::now() + self.options.completion_timeout,
            },
            WalkEnd::Complete => MatchState::Idle,
        };

        true
    }
}

/// Consumes [`RecognitionEvent`]s and produces [`CommandAction`]s onto the
/// command queue, resolving ambiguous prefixes with the completion timeout —
/// the grammar v2 replacement for [`super::matcher_task`].
///
/// Runs until `cancel` fires, the events channel closes (the recognizer shut
/// down), or the command queue closes (the executor shut down) — all of which
/// end the task cleanly.
pub async fn engine_task(
    automaton: Automaton,
    pacing: Pacing,
    options: MatcherOptions,
    mut events: mpsc::Receiver<RecognitionEvent>,
    queue: mpsc::Sender<CommandAction>,
    cancel: CancellationToken,
) -> Result<(), crate::Error> {
    let engine = Engine::new(&automaton, pacing, options, queue);
    let mut state = MatchState::Idle;

    loop {
        let deadline = state.deadline();

        tokio::select! {
            _ = cancel.cancelled() => {
                // Cancellation is a shutdown demanded from outside the
                // pipeline (signal, child exit): like a mute, it must not
                // press keys under the user, so a pending command is dropped.
                if let MatchState::Pending { accept, .. } = &state {
                    debug!(
                        command = %accept.display,
                        "Dropping the pending command '{}': shutdown was requested.",
                        accept.display
                    );
                }
                debug!("Shutdown requested, stopping the matcher.");
                return Ok(());
            }
            _ = deadline_elapsed(deadline) => {
                if let MatchState::Pending { accept, .. } =
                    std::mem::replace(&mut state, MatchState::Idle)
                    && !engine.fire(&accept).await
                {
                    // The speaker paused long enough: the pending command is
                    // the one they meant.
                    return Ok(());
                }
            }
            event = events.recv() => match event {
                Some(RecognitionEvent::Final(utterance)) => {
                    if !engine.on_final(&mut state, &utterance).await {
                        return Ok(());
                    }
                }
                Some(RecognitionEvent::Partial(text)) => {
                    if let MatchState::Pending { walk, deadline, .. } = &mut state {
                        // Eager off: a partial only ever *extends* a pending
                        // deadline. The pending state came from a previously
                        // *finalized* utterance, so any new partial is the
                        // start of a fresh hypothesis — its first word
                        // extending the pending walk means the speaker is
                        // mid-way through the longer phrase, and the short
                        // command must not fire under them.
                        let words = words_of(&text);
                        if let Some(first) = words.first() {
                            // Probed on a fork so the pending walk itself is
                            // never advanced by an unconfirmed hypothesis. A
                            // probe that dies (an overflow included) simply
                            // does not extend.
                            let mut probe = walk.clone();
                            probe.step(first);
                            if !probe.is_dead() {
                                *deadline = Instant::now() + engine.options.completion_timeout;
                            }
                        }
                    }
                }
                Some(RecognitionEvent::Failed) => {
                    // A frame the recognizer could not decode says nothing
                    // about what was said, so it must not disturb a pending
                    // command: the words either side of it still add up to the
                    // phrase the speaker is part-way through. The session's UI
                    // is where this is reported.
                }
                Some(RecognitionEvent::Muted) => {
                    // A half-confirmed command must not fire when listening
                    // resumes.
                    if let MatchState::Pending { accept, .. } = &state {
                        debug!(
                            command = %accept.display,
                            "Dropping the pending command '{}': listening was turned off.",
                            accept.display
                        );
                    }
                    state = MatchState::Idle;
                }
                None => {
                    // The recognizer closed the events channel: the pipeline
                    // is shutting down of its own accord. Unlike cancellation
                    // or a mute, a pending command here was fully spoken and
                    // confirmed by a Final — only its settle time was cut
                    // short — so it fires rather than being swallowed.
                    if let MatchState::Pending { accept, .. } = state {
                        engine.fire(&accept).await;
                    }
                    debug!("The recognition channel was closed, stopping the matcher.");
                    return Ok(());
                }
            }
        }
    }
}

/// Sleeps until `deadline`, or forever when there is none — the timer half of
/// the engine's `select!`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::v2::Grammar;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_millis(300);

    fn pacing() -> Pacing {
        Pacing {
            duration: Duration::from_millis(30),
            interval: Duration::from_millis(25),
        }
    }

    fn compile(source: &str) -> Automaton {
        let grammar = Grammar::parse(source).expect("the test grammar should parse");
        Automaton::compile(&grammar).unwrap_or_else(|diagnostics| {
            panic!(
                "the test grammar should compile:\n{}",
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.render(source))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
    }

    /// The standard test arsenal, matching the v1 suite's shape: "autocannon"
    /// is an ambiguous prefix of "autocannon sentry" *via a different rule*,
    /// "deploy sentry" and "reload" are unambiguous.
    fn arsenal() -> Automaton {
        compile(
            r#"
            Autocannon = "autocannon" { 4 }
            AutocannonSentry = "autocannon sentry" { 5 }
            DeploySentry = "deploy sentry" { 6 }
            Reload = "reload" { r }
            "#,
        )
    }

    /// Lets the spawned engine task run (on the paused clock, without
    /// advancing it) until it has processed everything we've sent.
    async fn settle() {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    struct Harness {
        events: mpsc::Sender<RecognitionEvent>,
        actions: mpsc::Receiver<CommandAction>,
        warnings: Arc<Mutex<Vec<String>>>,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<Result<(), crate::Error>>,
    }

    impl Harness {
        fn start(automaton: Automaton) -> Self {
            Self::start_with(automaton, MatcherOptions::with_timeout(TIMEOUT))
        }

        fn start_with(automaton: Automaton, mut options: MatcherOptions) -> Self {
            // Roomy channels so long utterances can fire many commands
            // without the engine blocking on a full queue mid-test.
            let (events, events_rx) = mpsc::channel(16);
            let (actions_tx, actions) = mpsc::channel(256);
            let cancel = CancellationToken::new();

            // Whatever warnings the engine raises are captured for the tests
            // to assert on.
            let warnings = Arc::new(Mutex::new(Vec::new()));
            options.warn = {
                let warnings = warnings.clone();
                Arc::new(move |message| warnings.lock().unwrap().push(message))
            };

            let handle = tokio::spawn(engine_task(
                automaton,
                pacing(),
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
                .expect("the engine should still be listening");
            settle().await;
        }

        async fn hear_partial(&self, text: &str) {
            self.events
                .send(RecognitionEvent::Partial(text.to_string()))
                .await
                .expect("the engine should still be listening");
            settle().await;
        }

        async fn mute(&self) {
            self.events
                .send(RecognitionEvent::Muted)
                .await
                .expect("the engine should still be listening");
            settle().await;
        }

        async fn fail(&self) {
            self.events
                .send(RecognitionEvent::Failed)
                .await
                .expect("the engine should still be listening");
            settle().await;
        }

        async fn advance(&self, duration: Duration) {
            tokio::time::advance(duration).await;
            settle().await;
        }

        /// Drains every command which has fired so far, in order, as display
        /// names.
        fn fired(&mut self) -> Vec<String> {
            self.fired_actions()
                .into_iter()
                .map(|action| action.command)
                .collect()
        }

        /// Drains every command which has fired so far, in order, with the
        /// assembled outputs.
        fn fired_actions(&mut self) -> Vec<CommandAction> {
            let mut actions = Vec::new();
            while let Ok(action) = self.actions.try_recv() {
                actions.push(action);
            }
            actions
        }

        fn nothing_fired(&mut self) {
            assert_eq!(self.fired(), Vec::<&str>::new());
        }

        /// Drains every warning the engine has raised so far, in order.
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
                .expect("the engine task should not panic")
                .expect("the engine task should end cleanly");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unambiguous_final_fires_immediately() {
        let mut h = Harness::start(arsenal());

        h.hear_final("deploy sentry").await;

        // No time has been advanced: the fire must not wait for any timeout.
        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_decode_failure_leaves_the_engine_exactly_where_it_was() {
        let mut h = Harness::start(arsenal());

        // A failure in the middle of an ambiguous phrase must not fire the
        // pending command early (as a mute would) nor drop it: it carries no
        // words, so there is nothing for the engine to change its mind about.
        h.hear_final("autocannon").await;
        h.fail().await;
        h.nothing_fired();

        h.advance(TIMEOUT).await;
        assert_eq!(h.fired(), vec!["Autocannon"]);

        // And it is not a phrase boundary either: the next utterance matches
        // exactly as it would have without it.
        h.fail().await;
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["DeploySentry"]);
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
        assert_eq!(h.fired(), vec!["Autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn continuation_supersedes_the_pending_command() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final("sentry").await;
        assert_eq!(h.fired(), vec!["AutocannonSentry"]);

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
        assert_eq!(h.fired(), vec!["Autocannon", "Reload"]);

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
        assert_eq!(h.fired(), vec!["Autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn non_extending_partial_leaves_the_deadline_alone() {
        let mut h = Harness::start(arsenal());

        h.hear_final("autocannon").await; // deadline: t0 + 300ms
        h.advance(Duration::from_millis(200)).await;
        h.hear_partial("reload").await; // does not extend the pending walk
        h.nothing_fired();

        // The original deadline still stands.
        h.advance(Duration::from_millis(100)).await; // t0 + 300ms
        assert_eq!(h.fired(), vec!["Autocannon"]);
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
        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn unk_tokens_are_stripped() {
        let mut h = Harness::start(arsenal());

        h.hear_final("[unk] deploy sentry [unk]").await;

        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn greedy_segmentation_fires_multiple_commands_from_one_utterance() {
        let mut h = Harness::start(arsenal());

        h.hear_final("deploy sentry reload").await;

        assert_eq!(h.fired(), vec!["DeploySentry", "Reload"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn resyncs_from_the_root_past_unknown_leading_words() {
        let mut h = Harness::start(arsenal());

        h.hear_final("hello deploy sentry").await;

        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn resyncs_by_retrying_the_failing_word_from_the_root() {
        let mut h = Harness::start(arsenal());

        // "deploy" consumes a step, then "reload" fails mid-walk with no
        // accept crossed: the engine must retry "reload" from the root rather
        // than dropping it along with "deploy".
        h.hear_final("deploy reload").await;

        assert_eq!(h.fired(), vec!["Reload"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_phrase_is_dropped_silently() {
        let mut h = Harness::start(arsenal());

        // Only "deploy sentry" exists; "deploy" alone rests mid-walk with no
        // accept on the path.
        h.hear_final("deploy").await;
        h.nothing_fired();

        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguity_is_per_resting_walk_across_different_rules() {
        // "autocannon" and "autocannon sentry" are different rules: the
        // ambiguity lives on the resting walk, not on either rule.
        let mut h = Harness::start(arsenal());

        // Left alone, Pending holds the short command and the timer fires it.
        h.hear_final("autocannon").await;
        h.nothing_fired();
        h.advance(TIMEOUT).await;
        assert_eq!(h.fired(), vec!["Autocannon"]);

        // Continued in time, the continuation fires the long one (and only
        // the long one).
        h.hear_final("autocannon").await;
        h.hear_final("sentry").await;
        assert_eq!(h.fired(), vec!["AutocannonSentry"]);
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
            .expect("the engine task should not panic")
            .expect("the engine task should end cleanly");

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
            .expect("the engine task should not panic")
            .expect("the engine task should end cleanly");

        assert_eq!(
            actions
                .try_recv()
                .expect("the pending command fires")
                .command,
            "Autocannon"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_continuation_which_stalls_mid_walk_flushes_the_pending_command() {
        // The continuation "sentry" extends the pending walk but never
        // reaches the longer rule's accept. The short command was fully
        // spoken, so it fires; the stray "sentry" is dropped.
        let mut h = Harness::start(compile(
            r#"
            Autocannon = "autocannon" { 4 }
            AutocannonSentryGun = "autocannon sentry gun" { 5 }
            "#,
        ));

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final("sentry").await;
        assert_eq!(h.fired(), vec!["Autocannon"]);

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
        assert_eq!(h.fired(), vec!["Autocannon"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn long_junk_utterances_are_handled_robustly() {
        let mut h = Harness::start(arsenal());

        // 200 words of junk — including grammar-word prefixes ("deploy",
        // "autocannon") which force mid-walk resyncs — then a real command.
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
        // (greedy longest-match crossed its accept); every stray "deploy" is
        // an incomplete phrase and drops. The final "deploy sentry" fires.
        let fired = h.fired();
        assert_eq!(
            fired.last().map(String::as_str),
            Some("DeploySentry"),
            "the trailing command must fire: {fired:?}"
        );
        assert!(
            fired[..fired.len() - 1]
                .iter()
                .all(|name| name == "Autocannon"),
            "only the stray autocannons may fire besides it: {fired:?}"
        );
        h.shutdown().await;
    }
}
