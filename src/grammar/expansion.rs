//! Expansion of a parsed phrase AST into the concrete word sequences Vosk will
//! hear, plus the linear word-set walk used by vocabulary validation.
//!
//! Words are lowercased *here*, not in the lexer, so a phrase's source always
//! round-trips through `Display` unchanged.

use std::collections::{BTreeSet, HashSet};

use crate::Error;

use super::expr::{Node, PhraseExpr};

/// The maximum number of concrete phrases a single command may expand into.
/// Exceeding it is a hard error — silently truncating a grammar would silently
/// break commands.
#[allow(dead_code)] // consumed as the wave-2 validate/run modules land
pub const MAX_EXPANSIONS_PER_COMMAND: usize = 512;

/// The concrete phrases a command expands into: deduplicated, in insertion
/// order, with every word lowercased.
#[derive(Debug, PartialEq)]
#[allow(dead_code)] // consumed as the wave-2 validate/run modules land
pub struct Expansion {
    pub phrases: Vec<Vec<String>>,
}

/// Counts the phrases `expr` would expand into — multiplicatively, without
/// materializing anything, saturating at `usize::MAX` — so an explosive phrase
/// can be rejected before it allocates. The count is taken before
/// deduplication, so it is an upper bound on [`Expansion::phrases`].
#[allow(dead_code)] // consumed as the wave-2 validate/run modules land
pub fn count(expr: &PhraseExpr<'_>) -> usize {
    sequence_count(&expr.0)
}

fn sequence_count(nodes: &[Node<'_>]) -> usize {
    nodes
        .iter()
        .fold(1usize, |acc, node| acc.saturating_mul(node_count(node)))
}

fn node_count(node: &Node<'_>) -> usize {
    match node {
        Node::Word(..) => 1,
        // An optional group contributes its omission plus every inner variant.
        Node::Optional(_, nodes) => sequence_count(nodes).saturating_add(1),
        Node::Alternates(_, branches) => branches.iter().fold(0usize, |acc, branch| {
            acc.saturating_add(sequence_count(branch))
        }),
    }
}

/// Expands `expr` into its concrete phrases, erroring *before* materializing
/// anything when the [`count`] exceeds [`MAX_EXPANSIONS_PER_COMMAND`].
#[allow(dead_code)] // consumed as the wave-2 validate/run modules land
pub fn expand(expr: &PhraseExpr<'_>) -> Result<Expansion, Error> {
    let count = count(expr);
    if count > MAX_EXPANSIONS_PER_COMMAND {
        return Err(human_errors::user(
            format!(
                "Your phrase expands into {count} concrete phrases, which is more than the {MAX_EXPANSIONS_PER_COMMAND} a single command may use."
            ),
            &[
                "Split the command into several smaller commands, or remove some of the '[optional]' and '{alternate, choices}' groups.",
            ],
        ));
    }

    // Deduplicate while preserving insertion order, so e.g. '[a] [a]'
    // collapses its duplicate 'a'.
    let mut seen = HashSet::new();
    let mut phrases = Vec::new();
    for phrase in sequence_expansions(&expr.0) {
        if seen.insert(phrase.clone()) {
            phrases.push(phrase);
        }
    }

    Ok(Expansion { phrases })
}

fn sequence_expansions(nodes: &[Node<'_>]) -> Vec<Vec<String>> {
    let mut results = vec![Vec::new()];

    for node in nodes {
        let options = node_expansions(node);
        let mut extended = Vec::with_capacity(results.len() * options.len());
        for prefix in &results {
            for option in &options {
                let mut phrase = prefix.clone();
                phrase.extend_from_slice(option);
                extended.push(phrase);
            }
        }
        results = extended;
    }

    results
}

fn node_expansions(node: &Node<'_>) -> Vec<Vec<String>> {
    match node {
        Node::Word(_, word) => vec![vec![word.to_lowercase()]],
        Node::Optional(_, nodes) => {
            let mut options = vec![Vec::new()];
            options.extend(sequence_expansions(nodes));
            options
        }
        Node::Alternates(_, branches) => branches
            .iter()
            .flat_map(|branch| sequence_expansions(branch))
            .collect(),
    }
}

/// Collects every distinct (lowercased) word in `expr` with a linear walk of
/// the AST, so vocabulary checking never needs the full expansion.
#[allow(dead_code)] // consumed as the wave-2 validate/run modules land
pub fn word_set(expr: &PhraseExpr<'_>) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    collect_words(&expr.0, &mut words);
    words
}

fn collect_words(nodes: &[Node<'_>], words: &mut BTreeSet<String>) {
    for node in nodes {
        match node {
            Node::Word(_, word) => {
                words.insert(word.to_lowercase());
            }
            Node::Optional(_, nodes) => collect_words(nodes, words),
            Node::Alternates(_, branches) => {
                for branch in branches {
                    collect_words(branch, words);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::{lexer::Scanner, parser::Parser};
    use super::*;

    fn parse(input: &str) -> PhraseExpr<'_> {
        Parser::parse(Scanner::new(input)).expect("the phrase should parse")
    }

    /// Builds `groups` chained copies of "{a, b, c, d} " — each multiplies the
    /// expansion count by four.
    fn chained_alternates(groups: usize) -> String {
        vec!["{a, b, c, d}"; groups].join(" ")
    }

    #[rstest]
    #[case("deploy", 1)]
    #[case("deploy the sentry", 1)]
    #[case("[a]", 2)]
    #[case("{a, b}", 2)]
    #[case("[a] {a, b}", 4)]
    #[case("deploy [the] {autocannon, auto cannon} [sentry]", 8)]
    #[case("[{a, b} c]", 3)]
    #[case("{a, b, c, d} {a, b, c, d}", 16)]
    fn test_count(#[case] input: &str, #[case] expected: usize) {
        assert_eq!(count(&parse(input)), expected);
    }

    #[test]
    fn test_count_is_multiplicative_and_saturates() {
        assert_eq!(count(&parse(&chained_alternates(10))), 1_048_576);
        // 4^40 overflows a u64, so the count saturates instead of panicking.
        assert_eq!(count(&parse(&chained_alternates(40))), usize::MAX);
    }

    fn joined(expansion: &Expansion) -> Vec<String> {
        expansion
            .phrases
            .iter()
            .map(|phrase| phrase.join(" "))
            .collect()
    }

    #[rstest]
    #[case("deploy", &["deploy"])]
    #[case("deploy the sentry", &["deploy the sentry"])]
    // An all-optional phrase includes the empty phrase among its expansions.
    #[case("[a]", &["", "a"])]
    #[case("{autocannon, auto cannon}", &["auto cannon", "autocannon"])]
    #[case("[a] {a, b}", &["a", "a a", "a b", "b"])]
    // Expansion lowercases; the source (and Display) keep the original case.
    #[case("Deploy [The] SENTRY", &["deploy sentry", "deploy the sentry"])]
    #[case("deploy [the] {autocannon, auto cannon} [sentry]", &[
        "deploy auto cannon",
        "deploy auto cannon sentry",
        "deploy autocannon",
        "deploy autocannon sentry",
        "deploy the auto cannon",
        "deploy the auto cannon sentry",
        "deploy the autocannon",
        "deploy the autocannon sentry",
    ])]
    fn test_expand(#[case] input: &str, #[case] expected: &[&str]) {
        let expansion = expand(&parse(input)).expect("the phrase should expand");
        let mut phrases = joined(&expansion);
        phrases.sort();
        assert_eq!(phrases, expected);
    }

    #[test]
    fn test_deploy_phrase_expands_to_exactly_eight_phrases() {
        let expansion = expand(&parse("deploy [the] {autocannon, auto cannon} [sentry]"))
            .expect("the phrase should expand");
        assert_eq!(expansion.phrases.len(), 8);
    }

    #[rstest]
    #[case("[a] [a]", &["", "a", "a a"])]
    #[case("{a, a}", &["a"])]
    #[case("{a, [a]}", &["a", ""])]
    #[case("[Alpha] {alpha, beta}", &["alpha", "beta", "alpha alpha", "alpha beta"])]
    fn test_expand_dedupes_preserving_insertion_order(
        #[case] input: &str,
        #[case] expected: &[&str],
    ) {
        let expansion = expand(&parse(input)).expect("the phrase should expand");
        assert_eq!(joined(&expansion), expected);
    }

    #[test]
    fn test_expand_preserves_insertion_order() {
        let expansion =
            expand(&parse("deploy [the] {autocannon, auto cannon}")).expect("should expand");
        assert_eq!(
            joined(&expansion),
            &[
                "deploy autocannon",
                "deploy auto cannon",
                "deploy the autocannon",
                "deploy the auto cannon",
            ]
        );
    }

    #[test]
    fn test_expand_at_the_cap_succeeds() {
        // Nine chained {a, b} groups expand to exactly 2^9 = 512 phrases,
        // which is within the cap.
        let source = ["{a, b}"; 9].join(" ");
        let expansion = expand(&parse(&source)).expect("512 phrases should be allowed");
        assert_eq!(expansion.phrases.len(), 512);
    }

    #[test]
    fn test_expand_past_the_cap_errors_before_materializing() {
        // Ten chained {a, b, c, d} groups would expand into 4^10 = 1,048,576
        // phrases; the multiplicative count rejects that without allocating.
        let source = chained_alternates(10);
        let started = std::time::Instant::now();
        let error = expand(&parse(&source)).expect_err("the expansion should be rejected");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the cap check should short-circuit long before materializing"
        );

        let message = error.to_string();
        assert!(
            message.contains("Your phrase expands into 1048576 concrete phrases"),
            "unexpected error: {message}"
        );
        assert!(message.contains("512"), "unexpected error: {message}");
    }

    #[rstest]
    #[case("deploy [the] {autocannon, auto cannon} [sentry]", &[
        "auto", "autocannon", "cannon", "deploy", "sentry", "the",
    ])]
    #[case("Deploy DEPLOY deploy", &["deploy"])]
    #[case("[{a, b} c] d", &["a", "b", "c", "d"])]
    fn test_word_set(#[case] input: &str, #[case] expected: &[&str]) {
        let words: Vec<String> = word_set(&parse(input)).into_iter().collect();
        assert_eq!(words, expected, "BTreeSet iteration should be sorted");
    }

    #[test]
    fn test_word_set_never_expands() {
        // 4^40 phrases could never be materialized, but the word set is a
        // linear walk of the AST.
        let words = word_set(&parse(&chained_alternates(40)));
        assert_eq!(
            words.into_iter().collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
    }
}
