//! The word-level phrase table: every expanded phrase of every command loaded
//! into a single trie, arena-allocated with indices instead of boxes. See
//! DESIGN.md §"Phrase table".
//!
//! Ambiguity is a *node* property: a node that is terminal **and** has
//! children marks an utterance that is also a strict prefix of some longer
//! phrase — regardless of which commands are involved.

use std::collections::HashMap;

use crate::Error;
use crate::output::CompiledOutput;

/// Identifies a command by its index into the compiled command slice which
/// accompanies the trie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandId(pub usize);

/// A command compiled for matching: its display name, its pre-compiled output
/// plan, and the concrete word sequences its phrase expands into.
#[derive(Debug, Clone)]
pub struct CompiledCommand {
    /// The command's display name, for logging.
    pub name: String,
    /// The pre-compiled output plan to execute when the command fires.
    pub output: CompiledOutput,
    /// The expanded, lowercased word sequences which trigger the command.
    pub phrases: Vec<Vec<String>>,
}

/// One node in the trie arena.
#[derive(Debug, Default)]
struct TrieNode {
    /// The next node for each word which extends this path.
    children: HashMap<String, usize>,
    /// The command whose full phrase ends at this node, if any. Because one
    /// command expands to many phrases, many nodes may share a `CommandId`.
    terminal: Option<CommandId>,
}

/// The phrase trie: a word-level trie over every expanded phrase of every
/// command, walked by the matcher one recognized word at a time.
#[derive(Debug)]
pub struct PhraseTrie {
    /// The node arena; index [`PhraseTrie::ROOT`] is the root.
    nodes: Vec<TrieNode>,
}

impl PhraseTrie {
    /// The index of the root node.
    pub const ROOT: usize = 0;

    /// Builds the trie from every phrase of every command.
    ///
    /// Fails when two *different* commands expand to the same full phrase
    /// (we couldn't tell which one the speaker meant), when a command
    /// contains an empty phrase (an all-optional phrase which can never be
    /// spoken), or when a command has no phrases at all. The same command
    /// producing a phrase twice is tolerated — expansion dedupes, but the
    /// trie doesn't depend on it.
    pub fn build(commands: &[CompiledCommand]) -> Result<Self, Error> {
        let mut trie = PhraseTrie {
            nodes: vec![TrieNode::default()],
        };

        for (index, command) in commands.iter().enumerate() {
            let id = CommandId(index);

            if command.phrases.is_empty() {
                return Err(no_phrases(&command.name));
            }

            for phrase in &command.phrases {
                if phrase.is_empty() {
                    return Err(empty_phrase(&command.name));
                }

                let mut node = Self::ROOT;
                for word in phrase {
                    // The recognizer's output and the expansion are both
                    // already lowercase, but lowercasing here is cheap and
                    // makes the invariant local.
                    let word = word.to_lowercase();
                    node = match trie.nodes[node].children.get(&word).copied() {
                        Some(next) => next,
                        None => {
                            let next = trie.nodes.len();
                            trie.nodes.push(TrieNode::default());
                            trie.nodes[node].children.insert(word, next);
                            next
                        }
                    };
                }

                match trie.nodes[node].terminal {
                    None => trie.nodes[node].terminal = Some(id),
                    // The same command producing the same phrase twice is
                    // harmless — the duplicate collapses onto the same node.
                    Some(existing) if existing == id => {}
                    Some(existing) => {
                        return Err(duplicate_phrase(
                            &commands[existing.0].name,
                            &command.name,
                            phrase,
                        ));
                    }
                }
            }
        }

        Ok(trie)
    }

    /// The node reached by following `word` from `node`, if that path exists.
    pub fn step(&self, node: usize, word: &str) -> Option<usize> {
        self.nodes.get(node)?.children.get(word).copied()
    }

    /// The command whose full phrase ends at `node`, if any.
    pub fn terminal(&self, node: usize) -> Option<CommandId> {
        self.nodes.get(node)?.terminal
    }

    /// Whether `node` is a terminal which is also a strict prefix of some
    /// longer phrase — the condition which engages the completion timeout.
    pub fn is_ambiguous(&self, node: usize) -> bool {
        self.nodes
            .get(node)
            .is_some_and(|node| node.terminal.is_some() && !node.children.is_empty())
    }
}

// The advice arrays must be `&'static`, so all dynamic detail (most notably
// the offending command names and phrase text) lives in the message.

fn no_phrases(name: &str) -> Error {
    human_errors::user(
        format!("Your command '{name}' has no phrases, so it could never be spoken."),
        &["Give the command at least one phrase, e.g. 'phrase: deploy [the] sentry'."],
    )
}

fn empty_phrase(name: &str) -> Error {
    human_errors::user(
        format!(
            "Your command '{name}' can match an empty phrase because every word in it is optional, and an empty phrase cannot be spoken."
        ),
        &[
            "Make at least one word required by keeping it outside any '[...]' group, e.g. 'deploy [the] [sentry]'.",
        ],
    )
}

fn duplicate_phrase(first: &str, second: &str, phrase: &[String]) -> Error {
    human_errors::user(
        format!(
            "Your commands '{first}' and '{second}' both match the phrase \"{}\", so we couldn't tell which one you meant.",
            phrase.join(" ")
        ),
        &["Reword one of the phrases so that every spoken phrase belongs to exactly one command."],
    )
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

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

    /// autocannon (0) | autocannon sentry (1) | deploy [the] sentry (2)
    fn sample() -> PhraseTrie {
        PhraseTrie::build(&[
            cmd("autocannon", &["autocannon"]),
            cmd("autocannon sentry", &["autocannon sentry"]),
            cmd("deploy sentry", &["deploy sentry", "deploy the sentry"]),
        ])
        .expect("the sample command set should build")
    }

    /// Follows a word path from the root, panicking if it does not exist.
    fn descend(trie: &PhraseTrie, path: &[&str]) -> usize {
        path.iter()
            .try_fold(PhraseTrie::ROOT, |node, word| trie.step(node, word))
            .unwrap_or_else(|| panic!("the path {path:?} should exist in the trie"))
    }

    #[rstest]
    #[case::root(&[], None, false)]
    #[case::terminal_and_prefix(&["autocannon"], Some(0), true)]
    #[case::plain_terminal(&["autocannon", "sentry"], Some(1), false)]
    #[case::mid_trie(&["deploy"], None, false)]
    #[case::mid_trie_branch(&["deploy", "the"], None, false)]
    #[case::shared_terminal(&["deploy", "sentry"], Some(2), false)]
    #[case::shared_terminal_long(&["deploy", "the", "sentry"], Some(2), false)]
    fn node_properties(
        #[case] path: &[&str],
        #[case] terminal: Option<usize>,
        #[case] ambiguous: bool,
    ) {
        let trie = sample();
        let node = descend(&trie, path);
        assert_eq!(trie.terminal(node), terminal.map(CommandId));
        assert_eq!(trie.is_ambiguous(node), ambiguous);
    }

    #[rstest]
    #[case::unknown_at_root(&[], "sentry")]
    #[case::no_such_continuation(&["autocannon"], "deploy")]
    #[case::past_a_leaf(&["autocannon", "sentry"], "sentry")]
    fn missing_steps(#[case] path: &[&str], #[case] word: &str) {
        let trie = sample();
        let node = descend(&trie, path);
        assert_eq!(trie.step(node, word), None);
    }

    #[test]
    fn out_of_range_nodes_are_harmless() {
        let trie = sample();
        assert_eq!(trie.step(9999, "autocannon"), None);
        assert_eq!(trie.terminal(9999), None);
        assert!(!trie.is_ambiguous(9999));
    }

    #[test]
    fn build_lowercases_defensively() {
        let trie = PhraseTrie::build(&[cmd("shout", &["AutoCannon"])])
            .expect("a mixed-case phrase should build");
        let node = descend(&trie, &["autocannon"]);
        assert_eq!(trie.terminal(node), Some(CommandId(0)));
    }

    #[test]
    fn duplicate_phrases_within_one_command_are_tolerated() {
        let trie = PhraseTrie::build(&[cmd("reload", &["reload", "reload"])])
            .expect("a command may harmlessly expand to the same phrase twice");
        assert_eq!(
            trie.terminal(descend(&trie, &["reload"])),
            Some(CommandId(0))
        );
    }

    #[test]
    fn duplicate_phrases_across_commands_are_an_error() {
        let error = PhraseTrie::build(&[
            cmd("throw grenade", &["fire in the hole"]),
            cmd("fire weapon", &["fire in the hole"]),
        ])
        .expect_err("two commands matching the same phrase should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("throw grenade"),
            "the error should name the first command: {message}"
        );
        assert!(
            message.contains("fire weapon"),
            "the error should name the second command: {message}"
        );
        assert!(
            message.contains("fire in the hole"),
            "the error should quote the colliding phrase: {message}"
        );
    }

    #[test]
    fn empty_phrases_are_an_error() {
        let error = PhraseTrie::build(&[cmd("whisper", &[""])])
            .expect_err("an all-optional (empty) phrase should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("whisper"),
            "the error should name the command: {message}"
        );
        assert!(
            message.contains("optional"),
            "the error should explain the all-optional cause: {message}"
        );
        assert!(
            error.advice().join(" ").contains("required"),
            "the advice should suggest making a word required: {:?}",
            error.advice()
        );
    }

    #[test]
    fn commands_without_phrases_are_an_error() {
        let error = PhraseTrie::build(&[cmd("mute", &[])])
            .expect_err("a command with no phrases should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("mute"),
            "the error should name the command: {message}"
        );
        assert!(
            message.contains("no phrases"),
            "the error should explain the cause: {message}"
        );
    }

    #[test]
    fn prefix_relations_across_commands_are_not_an_error() {
        // "autocannon" being a strict prefix of "autocannon sentry" is the
        // whole reason the completion timeout exists — it must build fine.
        sample();
    }
}
