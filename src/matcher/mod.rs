//! The matcher: walks recognized utterances through the phrase trie and
//! resolves ambiguous prefixes with the completion-timeout state machine.
//! See DESIGN.md §"Matcher: trie + completion timeout".
//!
//! Commands fire on `Final` results only; partials are used solely to hold a
//! pending timer open, never to fire.

// Consumed by `commands/run.rs` when the pipeline assembly lands.
#![allow(dead_code)]

pub mod trie;

pub use trie::{CommandId, CompiledCommand, PhraseTrie};

use crate::output::CompiledOutput;
use crate::recognition::RecognitionEvent;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing_batteries::prelude::*;

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
    completion_timeout: std::time::Duration,
    mut events: mpsc::Receiver<RecognitionEvent>,
    queue: mpsc::Sender<CommandAction>,
    cancel: CancellationToken,
) -> Result<(), crate::Error> {
    let mut state = MatchState::Idle;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Cancellation is a shutdown demanded from outside the
                // pipeline (signal, child exit): like a mute, it must not
                // press keys under the user, so a pending command is dropped.
                if let MatchState::Pending { command, .. } = state {
                    debug!(
                        command = %commands[command.0].name,
                        "Dropping the pending command '{}': shutdown was requested.",
                        commands[command.0].name
                    );
                }
                debug!("Shutdown requested, stopping the matcher.");
                return Ok(());
            }
            _ = deadline_elapsed(state.deadline()) => {
                // The speaker paused long enough: the pending command is the
                // one they meant.
                if let MatchState::Pending { command, .. } =
                    std::mem::replace(&mut state, MatchState::Idle)
                    && !fire(&commands, command, &queue).await
                {
                    return Ok(());
                }
            }
            event = events.recv() => match event {
                Some(RecognitionEvent::Final(text)) => {
                    let words = words_of(&text);
                    let (start, passed) = match state {
                        MatchState::Pending { command, node, .. } => (node, Some(command)),
                        MatchState::Idle => (PhraseTrie::ROOT, None),
                    };

                    let (fired, end) = walk(&trie, &words, start, passed);
                    for command in fired {
                        if !fire(&commands, command, &queue).await {
                            return Ok(());
                        }
                    }

                    state = match end {
                        WalkEnd::Pending { command, node } => MatchState::Pending {
                            command,
                            node,
                            deadline: Instant::now() + completion_timeout,
                        },
                        WalkEnd::Complete => MatchState::Idle,
                    };
                }
                Some(RecognitionEvent::Partial(text)) => {
                    // A partial only ever *extends* a pending deadline. The
                    // pending state came from a previously *finalized*
                    // utterance, so any new partial is the start of a fresh
                    // hypothesis — its first word extending from the pending
                    // node means the speaker is mid-way through the longer
                    // phrase, and the short command must not fire under them.
                    if let MatchState::Pending { node, deadline, .. } = &mut state {
                        let words = words_of(&text);
                        if let Some(first) = words.first()
                            && trie.step(*node, first).is_some()
                        {
                            *deadline = Instant::now() + completion_timeout;
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
                    if let MatchState::Pending { command, .. } = state {
                        debug!(
                            command = %commands[command.0].name,
                            "Dropping the pending command '{}': listening was turned off.",
                            commands[command.0].name
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
                    if let MatchState::Pending { command, .. } = state {
                        fire(&commands, command, &queue).await;
                    }
                    debug!("The recognition channel was closed, stopping the matcher.");
                    return Ok(());
                }
            }
        }
    }
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

/// Walks a finalized utterance through the trie with greedy longest-match
/// segmentation, returning the commands to fire (in order) and where the walk
/// came to rest.
///
/// `start`/`passed` continue a pending walk: `passed` is a terminal already
/// crossed at `start` (the pending command), so a continuation which fails to
/// extend it flushes the pending command before re-syncing from the root.
fn walk(
    trie: &PhraseTrie,
    words: &[String],
    start: usize,
    passed: Option<CommandId>,
) -> (Vec<CommandId>, WalkEnd) {
    let mut fired = Vec::new();
    let mut node = start;
    // The most recent terminal crossed on the current path, and the index of
    // the word right after it — where to re-sync once the greedy walk fails.
    let mut last_terminal: Option<(CommandId, usize)> = passed.map(|command| (command, 0));
    let mut i = 0;

    loop {
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
                fired.push(command);
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

        // The utterance is exhausted: decide from where the walk rests.
        if let Some(command) = trie.terminal(node) {
            if trie.is_ambiguous(node) {
                // Also a strict prefix of a longer phrase — hold it open for
                // the completion timeout.
                return (fired, WalkEnd::Pending { command, node });
            }
            fired.push(command);
            return (fired, WalkEnd::Complete);
        }

        let Some((command, resume)) = last_terminal.take() else {
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
        fired.push(command);
        node = PhraseTrie::ROOT;
        i = resume;
        if i >= words.len() {
            return (fired, WalkEnd::Complete);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_millis(300);

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
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<Result<(), crate::Error>>,
    }

    impl Harness {
        fn start(commands: Vec<CompiledCommand>) -> Self {
            let trie = PhraseTrie::build(&commands).expect("the test command set should build");
            // Roomy channels so long utterances can fire many commands
            // without the matcher blocking on a full queue mid-test.
            let (events, events_rx) = mpsc::channel(16);
            let (actions_tx, actions) = mpsc::channel(256);
            let cancel = CancellationToken::new();
            let handle = tokio::spawn(matcher_task(
                trie,
                commands,
                TIMEOUT,
                events_rx,
                actions_tx,
                cancel.clone(),
            ));

            Harness {
                events,
                actions,
                cancel,
                handle,
            }
        }

        async fn hear_final(&self, text: &str) {
            self.events
                .send(RecognitionEvent::Final(text.to_string()))
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
}
