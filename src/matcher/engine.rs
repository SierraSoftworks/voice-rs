//! The matcher engine: the completion-timeout state machine restated over the
//! automaton's hypothesis walk. See DESIGN.md §"Compilation: a word-level
//! transducer".
//!
//! Everything the original trie-based matcher promised still holds, word for
//! word — greedy longest-match segmentation with re-sync, the `Pending`
//! completion-timeout machine, eager partial-driven firing, and confidence
//! gating — but the walk position is a [`Walk`] (a set of alive hypotheses)
//! instead of a trie node, *ambiguous* means "some hypothesis accepts while
//! any can still consume a word", and a fired command's output is assembled
//! from its evaluated action program at fire time rather than pre-compiled per
//! command.

use std::collections::HashMap;

use crate::grammar::v2::{Accept, Automaton, Walk};
use crate::output::assembly::{Pacing, assemble};
use crate::output::{CompiledOutput, KeyEvent};
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

/// The eager path's per-utterance context: `Some` from an utterance's first
/// partial until its `Final` resolves (or a mute clears it), and only when
/// eager matching is on.
///
/// Invariant: while a context is open, the match state is `Idle` — a command
/// pending from the previous utterance is absorbed into `origin`/`passed` when
/// the context opens, and the continuation logic drives its fate from there.
#[derive(Debug)]
struct EagerContext<'a> {
    /// The walk this utterance's passes start from: the pending walk when the
    /// utterance opened against a Pending command, else a fresh root walk.
    /// `passed` is `Some` exactly when it is the pending walk.
    origin: Walk<'a>,
    /// The pending accept carried in from the previous utterance, if any. It
    /// was confirmed by its own `Final`; whether it fires, flushes or is
    /// superseded is decided by walking this utterance from `origin`.
    passed: Option<Accept>,
    /// Matches already fired from this utterance's partials, in firing order.
    /// `Final` reconciliation checks these against the finalized walk, and
    /// repeated partial walks use them to never fire the same position twice.
    fired: Vec<Match>,
    /// The accept the latest partial's walk rests on, with its armed
    /// deadline. `None` when the walk rests mid-phrase or at the root.
    resting: Option<EagerResting>,
    /// Walk warnings (hypothesis overflows, runtime-ambiguous accepts)
    /// already reported for this utterance. Every changed partial re-walks
    /// the same growing hypothesis, so without this the same overflow would
    /// reach the user once per revision instead of once per utterance.
    warned: Vec<String>,
}

/// An accept a partial hypothesis is resting on, waiting out a deadline.
///
/// The resting *walk* is not kept: a `Final` which pends here re-walks the
/// utterance from the context's own origin, so only the accept's identity and
/// its armed deadline matter.
#[derive(Debug)]
struct EagerResting {
    accept: Accept,
    /// The index just past the accept's last word in the partial.
    position: usize,
    /// Whether the resting walk is ambiguous. Ambiguous rests wait out the
    /// completion timeout (armed when the walk *first* rests here);
    /// unambiguous rests wait out `eager_delay` (re-armed on every changed
    /// partial).
    ambiguous: bool,
    deadline: Instant,
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

    /// Handles a finalized utterance: the full walk, and — when the eager
    /// path already fired from this utterance's partials — reconciliation.
    /// Returns whether the command queue is still open.
    async fn on_final(
        &self,
        state: &mut MatchState<'a>,
        eager: &mut Option<EagerContext<'a>>,
        utterance: &Utterance,
    ) -> bool {
        // The walk origin: an open eager context pins it (this utterance's
        // partials already walked from there, and may have fired), otherwise
        // the pending state decides exactly as v1 did. Either way the
        // utterance is resolved by this Final, so the context ends here.
        let context = eager.take();
        let (origin, passed) = match &context {
            Some(ctx) => (ctx.origin.clone(), ctx.passed.clone()),
            None => match &*state {
                MatchState::Pending { accept, walk, .. } => (walk.clone(), Some(accept.clone())),
                MatchState::Idle => (self.automaton.walk(), None),
            },
        };
        // The origin has consumed words exactly when a pending accept rode in
        // on it.
        let origin_fresh = passed.is_none();

        // Confidence gating first: when a close runner-up would have run
        // different commands, the whole utterance is suppressed — firing
        // nothing beats firing a coin-flip. (Config validation keeps
        // `alternatives` and eager firing apart, so no eager fire can precede
        // this check.)
        if let Some((top, competitor, margin)) =
            self.close_ambiguity(utterance, &origin, origin_fresh, passed.as_ref())
        {
            let message = format!("ambiguous: {top:?} vs {competitor:?} (margin {margin:.1})");
            warn!("Suppressing an utterance: {message}");
            (self.options.warn)(message);

            // A suppressed utterance cannot supersede a *previously
            // confirmed* pending command, and cannot be trusted to extend it
            // either. It therefore follows the existing non-extending rule:
            // the pending command — fully spoken and confirmed by its own
            // Final — flushes now, and the engine goes idle. (The
            // `fired`-is-empty guard only matters if gating and eager firing
            // are ever combined, which config validation currently forbids.)
            if let Some(accept) = &passed
                && context.as_ref().is_none_or(|ctx| ctx.fired.is_empty())
                && !self.fire(accept).await
            {
                return false;
            }
            *state = MatchState::Idle;
            return true;
        }

        let words = words_of(&utterance.text);
        let mut warn = |message: String| self.warn_user(message);
        let (matched, end) =
            self.walk_final(&words, &origin, origin_fresh, passed.as_ref(), &mut warn);

        let (already, resting) = match context {
            Some(ctx) => (ctx.fired, ctx.resting),
            None => (Vec::new(), None),
        };

        if already.len() <= matched.len()
            && already.iter().zip(&matched).all(|(a, b)| same_match(a, b))
        {
            // What the partials fired is a prefix of what the utterance says
            // (trivially so with no eager context): fire the remainder.
            for matched in &matched[already.len()..] {
                if !self.fire(&matched.accept).await {
                    return false;
                }
            }
            *state = match end {
                WalkEnd::Pending { accept, walk } => {
                    let deadline = self.pending_deadline(resting.as_ref(), &accept, words.len());
                    MatchState::Pending {
                        accept,
                        walk,
                        deadline,
                    }
                }
                WalkEnd::Complete => MatchState::Idle,
            };
        } else if let WalkEnd::Pending { accept, .. } = &end
            && already.len() == matched.len() + 1
            && matched.iter().zip(&already).all(|(a, b)| same_match(a, b))
            && already[matched.len()].position == words.len()
            && same_accept(&already[matched.len()].accept, accept)
        {
            // The one extra eager fire is the utterance's own resting accept:
            // its completion deadline elapsed mid-utterance, so the ambiguous
            // choice was already made. Nothing fires twice and nothing is held
            // pending — a continuation can no longer supersede a pressed key.
            *state = MatchState::Idle;
        } else {
            // An eager mismatch: keys were pressed for a hypothesis the
            // finalized utterance does not begin with. Nothing can be
            // un-pressed, and firing the "correct" remainder on top of a
            // wrong prefix would only compound the damage — so the rest of
            // the utterance is dropped, and the user is told what happened.
            let pressed: Vec<String> = already
                .iter()
                .map(|matched| matched.accept.display.clone())
                .collect();
            let message = format!(
                "eager mismatch: fired {} from a partial hypothesis, but the utterance settled as {:?} — the keys were already pressed, and the rest of the utterance was dropped",
                quoted_list(&pressed),
                utterance.text
            );
            self.warn_user(message);
            *state = MatchState::Idle;
        }

        true
    }

    /// The deadline for a command left pending by a `Final`:
    /// `completion_timeout` from now — unless the partial path already armed
    /// the *same* wait, in which case the earlier deadline stands. The whole
    /// point of arming from partials is that the wait starts when the
    /// hypothesis first came to rest, not when the endpointer got around to
    /// finalizing it.
    fn pending_deadline(
        &self,
        resting: Option<&EagerResting>,
        accept: &Accept,
        position: usize,
    ) -> Instant {
        let fresh = Instant::now() + self.options.completion_timeout;
        match resting {
            Some(rest)
                if rest.ambiguous
                    && rest.position == position
                    && same_accept(&rest.accept, accept) =>
            {
                rest.deadline.min(fresh)
            }
            _ => fresh,
        }
    }

    /// When the utterance's n-best list contains a close competitor which
    /// would run *different* commands, returns
    /// `(top text, competitor text, margin)` — the utterance should then be
    /// suppressed. `None` whenever fewer than two alternatives are present
    /// (gating off, or nothing to compare).
    ///
    /// Alternatives resolving to the same key sequences (homophones of the
    /// same phrase, or of phrases with the same output) or to nothing at all
    /// are ignored: they would not have changed what was pressed.
    fn close_ambiguity(
        &self,
        utterance: &Utterance,
        origin: &Walk<'a>,
        origin_fresh: bool,
        passed: Option<&Accept>,
    ) -> Option<(String, String, f32)> {
        let (top, rest) = utterance.alternatives.split_first()?;
        if rest.is_empty() {
            return None;
        }

        let chosen = self.resolve(&top.0, origin, origin_fresh, passed);
        for (text, confidence) in rest {
            let gap = top.1 - confidence;
            if gap > self.options.confidence_margin {
                continue;
            }
            let competitor = self.resolve(text, origin, origin_fresh, passed);
            if !competitor.is_empty() && competitor != chosen {
                return Some((top.0.clone(), text.clone(), gap.abs()));
            }
        }
        None
    }

    /// What an alternative's text would press, resolved from the same walk
    /// origin the real utterance will use: the full walk's fires plus a
    /// resting pending command, each assembled under the profile's pacing,
    /// positions ignored — two texts which would press the same keys in the
    /// same order are the same interpretation. (Where v1 compared
    /// `CommandId`s, the grammar has no per-command identity to compare, so
    /// the assembled plans themselves are the identity — which is what the
    /// n-best gating design specifies.)
    fn resolve(
        &self,
        text: &str,
        origin: &Walk<'a>,
        origin_fresh: bool,
        passed: Option<&Accept>,
    ) -> Vec<Vec<KeyEvent>> {
        let words = words_of(text);
        // Resolving a hypothetical reading must not warn at the user: only
        // the walk of the utterance the engine acts on gets to do that.
        let mut warn = |_: String| {};
        let (fired, end) = self.walk_final(&words, origin, origin_fresh, passed, &mut warn);
        let mut sequence: Vec<Vec<KeyEvent>> = fired
            .into_iter()
            .map(|matched| assemble(&matched.accept.actions, &self.pacing))
            .collect();
        if let WalkEnd::Pending { accept, .. } = end {
            sequence.push(assemble(&accept.actions, &self.pacing));
        }
        sequence
    }

    /// Handles a partial hypothesis with eager matching on. Returns whether
    /// the command queue is still open.
    async fn on_eager_partial(
        &self,
        state: &mut MatchState<'a>,
        eager: &mut Option<EagerContext<'a>>,
        text: &str,
    ) -> bool {
        let words = words_of(text);

        if eager.is_none() {
            // A new utterance is opening. A command still pending from the
            // previous one hands its resting walk over: this hypothesis walks
            // on from there, and the continuation logic itself decides the
            // pending command's fate — which subsumes the eager-off rule
            // where an extending partial merely pushed the pending deadline
            // out.
            let (origin, passed) = match std::mem::replace(state, MatchState::Idle) {
                MatchState::Pending { accept, walk, .. } => (walk, Some(accept)),
                MatchState::Idle => (self.automaton.walk(), None),
            };
            *eager = Some(EagerContext {
                origin,
                passed,
                fired: Vec::new(),
                resting: None,
                warned: Vec::new(),
            });
        }
        let ctx = eager.as_mut().expect("the context was just ensured");

        // Walk warnings are deduplicated per utterance: every changed partial
        // re-walks the same words, and the user needs to hear about an
        // overflow once, not once per revision.
        let mut new_warnings: Vec<String> = Vec::new();
        let (certain, resting) = {
            let warned = &ctx.warned;
            let mut warn = |message: String| {
                if !warned.contains(&message) && !new_warnings.contains(&message) {
                    new_warnings.push(message);
                }
            };
            let mut certain = Vec::new();
            let rest = self.greedy_pass(
                &words,
                0,
                &ctx.origin,
                ctx.passed.is_none(),
                ctx.passed.as_ref(),
                &mut certain,
                &mut warn,
            );
            // Unlike a Final's walk, only matches the pass *resynced past*
            // are certain — they ended strictly before a later word, so no
            // revision of the words still being spoken can take them back. An
            // accept the walk is resting on stays uncommitted until its
            // deadline: the caller arms one below.
            let resting = if rest.walk.has_accept() {
                let trailing = rest
                    .trailing
                    .expect("a resting accept is always tracked as the trailing match");
                Some((trailing.accept, rest.walk))
            } else {
                None
            };
            (certain, resting)
        };
        for message in new_warnings {
            self.warn_user(message.clone());
            ctx.warned.push(message);
        }

        // Fire whatever the greedy walk has passed and resynced beyond.
        // `fired` keeps a revision which repeats the walk from firing the
        // same position twice.
        for matched in certain {
            if !ctx.fired.iter().any(|fired| same_match(fired, &matched)) {
                let accept = matched.accept.clone();
                ctx.fired.push(matched);
                if !self.fire(&accept).await {
                    return false;
                }
            }
        }

        // Where the hypothesis rests decides what is armed:
        // - an unambiguous accept arms `eager_delay`, re-armed by every
        //   changed partial (the hypothesis must hold perfectly still to be
        //   trusted);
        // - an ambiguous accept arms the completion timeout when the walk
        //   FIRST rests there — a later text change which does not move the
        //   resting point (a trailing "[unk]", say) keeps the armed deadline,
        //   because the wait starting early is the whole latency win;
        // - mid-phrase or the root arms nothing.
        ctx.resting =
            match resting {
                Some((accept, walk)) => {
                    let position = words.len();
                    if ctx.fired.iter().any(|fired| {
                        fired.position == position && same_accept(&fired.accept, &accept)
                    }) {
                        // This exact match already fired from an earlier deadline;
                        // a hypothesis merely holding still must not fire it
                        // again.
                        None
                    } else {
                        let ambiguous = walk.is_ambiguous();
                        let unchanged = ctx.resting.as_ref().is_some_and(|prev| {
                            same_accept(&prev.accept, &accept)
                                && prev.position == position
                                && prev.ambiguous == ambiguous
                        });
                        let deadline = match &ctx.resting {
                            Some(prev) if ambiguous && unchanged => prev.deadline,
                            _ if ambiguous => Instant::now() + self.options.completion_timeout,
                            _ => Instant::now() + self.options.eager_delay,
                        };
                        Some(EagerResting {
                            accept,
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
}

/// Consumes [`RecognitionEvent`]s and produces [`CommandAction`]s onto the
/// command queue, resolving ambiguous prefixes with the completion timeout.
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
    // The eager path's open utterance, when there is one. See
    // [`EagerContext`] for the invariant tying it to `state`.
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
                if let MatchState::Pending { accept, .. } = &state {
                    debug!(
                        command = %accept.display,
                        "Dropping the pending command '{}': shutdown was requested.",
                        accept.display
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
                        ctx.fired.push(Match {
                            position: rest.position,
                            accept: rest.accept.clone(),
                        });
                        if !engine.fire(&rest.accept).await {
                            return Ok(());
                        }
                    }
                } else if let MatchState::Pending { accept, .. } =
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
                    if !engine.on_final(&mut state, &mut eager, &utterance).await {
                        return Ok(());
                    }
                }
                Some(RecognitionEvent::Partial(text)) => {
                    if engine.options.eager {
                        if !engine.on_eager_partial(&mut state, &mut eager, &text).await {
                            return Ok(());
                        }
                    } else if let MatchState::Pending { walk, deadline, .. } = &mut state {
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
                    // resumes — and neither may a partial hypothesis.
                    if let MatchState::Pending { accept, .. } = &state {
                        debug!(
                            command = %accept.display,
                            "Dropping the pending command '{}': listening was turned off.",
                            accept.display
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
                        if let Some(accept) = &ctx.passed
                            && ctx.fired.is_empty()
                        {
                            engine.fire(accept).await;
                        }
                        if ctx.resting.is_some() {
                            debug!("Dropping an unconfirmed partial hypothesis: the recognition channel closed.");
                        }
                    } else if let MatchState::Pending { accept, .. } = state {
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
    const EAGER_DELAY: Duration = Duration::from_millis(100);

    /// The eager configuration the eager suite runs under.
    fn eager_options() -> MatcherOptions {
        MatcherOptions {
            eager: true,
            eager_delay: EAGER_DELAY,
            ..MatcherOptions::with_timeout(TIMEOUT)
        }
    }

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

        async fn hear_final_with_alternatives(&self, alternatives: &[(&str, f32)]) {
            self.events
                .send(RecognitionEvent::Final(utterance_of(alternatives)))
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
        assert_eq!(h.fired(), vec!["DeploySentry"]);

        // When the Final does arrive, reconciliation skips what already
        // fired — nothing fires twice, and the engine is left clean.
        h.hear_final("deploy sentry").await;
        h.nothing_fired();
        h.no_warnings();
        h.hear_final("reload").await;
        assert_eq!(h.fired(), vec!["Reload"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_certain_prefix_fires_immediately_on_the_partial() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // "deploy sentry" was passed and resynced beyond by "reload": it is
        // certain the moment the partial arrives, with no delay at all.
        h.hear_partial("deploy sentry reload").await;
        assert_eq!(h.fired(), vec!["DeploySentry"]);

        // The resting "reload" needs its stability delay.
        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["Reload"]);

        h.hear_final("deploy sentry reload").await;
        h.nothing_fired();
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_ambiguous_partial_arms_the_completion_timeout_from_now() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // "autocannon" rests on an ambiguous accept: the completion wait
        // starts at the partial, not at finalization.
        h.hear_partial("autocannon").await;
        h.nothing_fired();

        h.advance(TIMEOUT - Duration::from_millis(1)).await;
        h.nothing_fired();
        h.advance(Duration::from_millis(1)).await;
        assert_eq!(h.fired(), vec!["Autocannon"]);

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
        assert_eq!(h.fired(), vec!["Autocannon"]);
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
        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_revised_partial_disarms_the_deadline() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        h.hear_partial("deploy sentry").await;
        // The recognizer changed its mind before the delay elapsed: the
        // revised hypothesis rests mid-walk and nothing may fire.
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

        // The partial rests mid-walk: nothing is armed...
        h.hear_partial("deploy").await;
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();

        // ...but the Final completes the phrase and fires it at once.
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_mismatch_on_a_divergent_final_warns_and_drops() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // "reload" is resynced past ("deploy" does not extend it): certain,
        // fired immediately.
        h.hear_partial("reload deploy").await;
        assert_eq!(h.fired(), vec!["Reload"]);

        // The finalized utterance says something the fired prefix does not
        // begin with: the keys are already down, so the engine warns and
        // drops the rest rather than compounding the mistake.
        h.hear_final("deploy sentry").await;
        h.nothing_fired();

        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "one mismatch, one warning: {warnings:?}");
        assert!(
            warnings[0].contains("\"Reload\"") && warnings[0].contains("deploy sentry"),
            "the warning should name what fired and what was heard: {}",
            warnings[0]
        );

        // The mismatch leaves no state behind.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.hear_final("deploy sentry").await;
        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eager_completion_fire_followed_by_a_continuation_is_reported() {
        let mut h = Harness::start_with(arsenal(), eager_options());

        // The speaker pauses longer than the completion timeout mid-utterance:
        // the ambiguous choice is made and "autocannon" fires...
        h.hear_partial("autocannon").await;
        h.advance(TIMEOUT).await;
        assert_eq!(h.fired(), vec!["Autocannon"]);

        // ...and then they continue after all. The longer command fires too —
        // its keys are what they now asked for — and the Final reports the
        // overrun, because "autocannon" was pressed and cannot be taken back.
        h.hear_partial("autocannon sentry").await;
        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["AutocannonSentry"]);

        h.hear_final("autocannon sentry").await;
        h.nothing_fired();
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "the overrun warns once: {warnings:?}");
        assert!(
            warnings[0].contains("\"Autocannon\""),
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
        assert_eq!(h.fired(), vec!["DeploySentry"]);
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
        assert_eq!(h.fired(), vec!["DeploySentry"]); // resynced past: certain

        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["Reload"]);

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

        // The next utterance's partial extends the pending walk: the
        // continuation logic takes over, the pending deadline is disarmed,
        // and the longer command fires after its own stability delay.
        h.hear_partial("sentry").await;
        h.advance(Duration::from_millis(99)).await; // t0 + 349ms: past the old deadline
        h.nothing_fired();

        h.advance(Duration::from_millis(1)).await; // partial + EAGER_DELAY
        assert_eq!(h.fired(), vec!["AutocannonSentry"]);

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

        // "reload" cannot extend the pending walk: the pending command was
        // fully spoken, so it flushes immediately — the eager equivalent of
        // the non-extending Final rule, minus the wait.
        h.hear_partial("reload").await;
        assert_eq!(h.fired(), vec!["Autocannon"]);

        h.advance(EAGER_DELAY).await;
        assert_eq!(h.fired(), vec!["Reload"]);

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
            .expect("the engine task should not panic")
            .expect("the engine task should end cleanly");

        assert_eq!(
            actions
                .try_recv()
                .expect("the absorbed pending command fires")
                .command,
            "Autocannon"
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
        assert_eq!(h.fired(), vec!["AutocannonSentry"]);

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
        assert_eq!(h.fired(), vec!["DeploySentry"]);
        h.shutdown().await;
    }

    // --- Confidence gating (alternatives) ---------------------------------

    /// A grammar for the gating suite: two acoustically-confusable commands
    /// with different keys, and one command with homophone phrases.
    fn gating_arsenal() -> Automaton {
        compile(
            r#"
            MortarSentry = "mortar sentry" { 1 }
            RocketSentry = "rocket sentry" { 2 }
            OneUp = "one up" | "won up" { 3 }
            "#,
        )
    }

    /// An utterance carrying an n-best list, best first.
    fn utterance_of(alternatives: &[(&str, f32)]) -> Utterance {
        Utterance {
            text: alternatives
                .first()
                .map(|(text, _)| (*text).to_string())
                .unwrap_or_default(),
            alternatives: alternatives
                .iter()
                .map(|&(text, confidence)| (text.to_string(), confidence))
                .collect(),
        }
    }

    #[rstest::rstest]
    // A close competitor resolving to different keys: suppress.
    #[case(&[("mortar sentry", 240.0), ("rocket sentry", 238.8)], Some(1.2))]
    // The same competitor, outside the margin: the winner is clear.
    #[case(&[("mortar sentry", 240.0), ("rocket sentry", 235.0)], None)]
    // Homophone phrases of one command at identical confidence: they press
    // the same keys, never suppressed.
    #[case(&[("one up", 240.0), ("won up", 240.0)], None)]
    // A competitor resolving to nothing at all is ignored.
    #[case(&[("mortar sentry", 240.0), ("more tar sen tree", 239.9)], None)]
    // A single alternative has nothing to compete with.
    #[case(&[("mortar sentry", 240.0)], None)]
    // No alternatives at all (gating disabled): nothing to do.
    #[case(&[], None)]
    fn gating_margin_table(#[case] alternatives: &[(&str, f32)], #[case] expected: Option<f32>) {
        let automaton = gating_arsenal();
        let (queue, _actions) = mpsc::channel(1);
        let engine = Engine::new(
            &automaton,
            pacing(),
            MatcherOptions::with_timeout(TIMEOUT),
            queue,
        );
        let utterance = utterance_of(alternatives);

        let ambiguity = engine.close_ambiguity(&utterance, &engine.automaton.walk(), true, None);

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
        let mut h = Harness::start(gating_arsenal());

        h.hear_final_with_alternatives(&[("mortar sentry", 240.0), ("rocket sentry", 238.8)])
            .await;

        h.nothing_fired();
        let warnings = h.warnings();
        assert_eq!(
            warnings,
            vec!["ambiguous: \"mortar sentry\" vs \"rocket sentry\" (margin 1.2)"]
        );

        // Suppression leaves the engine idle: the next utterance matches
        // exactly as it would from scratch.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.hear_final_with_alternatives(&[("mortar sentry", 240.0), ("rocket sentry", 230.0)])
            .await;
        assert_eq!(h.fired(), vec!["MortarSentry"]);
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn gating_same_command_homophones_do_not_suppress() {
        // "one"/"won" return byte-identical confidences from Vosk; both
        // phrases belong to the same rule, so there is nothing ambiguous
        // about what to press.
        let mut h = Harness::start(gating_arsenal());

        h.hear_final_with_alternatives(&[("one up", 240.0), ("won up", 240.0)])
            .await;

        assert_eq!(h.fired(), vec!["OneUp"]);
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn gating_suppression_flushes_a_prior_pending_command() {
        // A pending command from a previous, trusted utterance follows the
        // existing non-extending rule when the next utterance is suppressed:
        // it was fully spoken and confirmed, so it fires; only the suppressed
        // utterance's own commands are withheld.
        let mut h = Harness::start(compile(
            r#"
            MortarSentry = "mortar sentry" { 1 }
            RocketSentry = "rocket sentry" { 2 }
            Autocannon = "autocannon" { 4 }
            AutocannonSentry = "autocannon sentry" { 5 }
            "#,
        ));

        h.hear_final("autocannon").await;
        h.nothing_fired();

        h.hear_final_with_alternatives(&[("mortar sentry", 240.0), ("rocket sentry", 238.8)])
            .await;
        assert_eq!(h.fired(), vec!["Autocannon"]);
        assert_eq!(h.warnings().len(), 1);

        // And the pending state is gone for good.
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    // --- Grammar v2 specifics ---------------------------------------------
    //
    // Everything above re-proves the v1 contract; these tests cover what the
    // trie never could — captures, splices, shared subject rules, and the
    // hypothesis-set failure modes.

    use crate::output::assembly::ActionItem;
    use crate::output::keys;

    /// A chord press: `press("leftshift+f1")`.
    fn press(chord: &str) -> ActionItem {
        ActionItem::Press(
            chord
                .split('+')
                .map(|name| keys::from_name(name).expect("a known key"))
                .collect(),
        )
    }

    /// The plan `items` assemble to under the test pacing.
    fn keyboard(items: &[ActionItem]) -> CompiledOutput {
        CompiledOutput::Keyboard(assemble(items, &pacing()))
    }

    /// The canonical Arma automaton, compiled once — the fixture the design
    /// says must load.
    fn arma() -> Automaton {
        use std::sync::OnceLock;
        static ARMA: OnceLock<Automaton> = OnceLock::new();
        ARMA.get_or_init(|| compile(&crate::grammar::v2::fixtures::arma_source()))
            .clone()
    }

    #[tokio::test(start_paused = true)]
    async fn captures_assemble_different_outputs_for_different_spoken_words() {
        // The assign_colour pattern: one command whose keys depend on which
        // word was spoken, assembled at fire time — there is no per-command
        // pre-compiled output a trie could have carried.
        let mut h = Harness::start(compile(
            r#"
            colour = ( "red" { 1 } | "blue" { 3 } )
            Assign = "assign" ("team"? colour):c { 9, c... }
            "#,
        ));

        h.hear_final("assign red").await;
        let actions = h.fired_actions();
        assert_eq!(actions.len(), 1, "one command fires: {actions:?}");
        assert_eq!(actions[0].command, "Assign(red)");
        assert_eq!(actions[0].output, keyboard(&[press("9"), press("1")]));

        // The same command, a different object, a different plan — and the
        // optional "team" contributes words to the display but no presses.
        h.hear_final("assign team blue").await;
        let actions = h.fired_actions();
        assert_eq!(actions.len(), 1, "one command fires: {actions:?}");
        assert_eq!(actions[0].command, "Assign(team blue)");
        assert_eq!(actions[0].output, keyboard(&[press("9"), press("3")]));
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn arma_subject_led_command_splices_the_subject_first() {
        // "two three advance": the shared subject rule's presses (F2, F3)
        // land before the menu presses the action block adds.
        let mut h = Harness::start(arma());

        h.hear_final("two three advance").await;

        let actions = h.fired_actions();
        assert_eq!(actions.len(), 1, "one command fires: {actions:?}");
        assert_eq!(actions[0].command, "Advance");
        assert_eq!(
            actions[0].output,
            keyboard(&[press("f2"), press("f3"), press("1"), press("2")])
        );
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn arma_watch_splices_captures_around_the_menu_presses() {
        // Watch's block reorders its captures — subject, menu, a beat for the
        // menu to open, then the direction — which spoken order alone could
        // never produce.
        let mut h = Harness::start(arma());

        h.hear_final("two watch east").await;

        let actions = h.fired_actions();
        assert_eq!(actions.len(), 1, "one command fires: {actions:?}");
        assert_eq!(actions[0].command, "Watch(two, east)");
        assert_eq!(
            actions[0].output,
            keyboard(&[
                press("f2"),
                press("3"),
                press("8"),
                ActionItem::Wait(Duration::from_millis(20)),
                press("3"),
            ])
        );
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn arma_select_bare_subject_fires_only_via_the_completion_timeout() {
        // Every subject is an ambiguous prefix of every subject-led command,
        // so a bare "two" only ever fires Select by waiting out the timeout.
        let mut h = Harness::start(arma());

        h.hear_final("two").await;
        h.nothing_fired();

        h.advance(TIMEOUT - Duration::from_millis(1)).await;
        h.nothing_fired();
        h.advance(Duration::from_millis(1)).await;

        let actions = h.fired_actions();
        assert_eq!(actions.len(), 1, "one command fires: {actions:?}");
        assert_eq!(actions[0].command, "Select");
        assert_eq!(actions[0].output, keyboard(&[press("f2")]));

        // Continued in time, the subject-led command supersedes Select
        // entirely.
        h.hear_final("two").await;
        h.hear_final("advance").await;
        assert_eq!(h.fired(), vec!["Advance"]);
        h.advance(TIMEOUT * 2).await;
        h.nothing_fired();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn synonym_rules_with_identical_keys_fire_the_first_defined_silently() {
        // Two published rules accepting the same words with the same keys are
        // deliberate synonyms — load-time duplicate detection lets them
        // collapse, and the engine fires the first-defined one without
        // warning anybody.
        let mut h = Harness::start(compile(
            r#"
            Alpha = "go" { 1 }
            Beta = "go" { 1 }
            "#,
        ));

        h.hear_final("go").await;

        assert_eq!(h.fired(), vec!["Alpha"]);
        h.no_warnings();
        h.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn hypothesis_overflow_warns_once_and_drops_the_utterance() {
        // Ten "x"s multiply into over 2^10 readings — past MAX_HYPOTHESES —
        // without any pair of them colliding on an accepted phrase, so the
        // grammar loads and the overflow only exists at runtime. The walk
        // goes dead, the utterance drops, and the user hears about it once.
        let mut h = Harness::start(compile(
            r#"
            first = "x" { 1 }
            second = "x" { 2 }
            seg = ( first | second )
            TenOne = seg[10] "one" { f1 }
            TenTwo = seg[10] "two" { f2 }
            "#,
        ));

        let utterance = format!("{} one", ["x"; 10].join(" "));
        h.hear_final(&utterance).await;

        h.nothing_fired();
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "one overflow, one warning: {warnings:?}");
        assert!(
            warnings[0].contains("possible readings"),
            "the warning should explain the overflow: {}",
            warnings[0]
        );

        // The engine is still healthy: a small utterance matches normally.
        h.hear_final("x one").await;
        h.nothing_fired(); // too few x's — an incomplete phrase, dropped
        h.shutdown().await;
    }
}
