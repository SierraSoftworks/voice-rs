//! The recognition grammar fed to Vosk. See DESIGN.md §"Feeding Vosk".
//!
//! Full per-command expansion is impossible under composition — a subject
//! rule alone admits thousands of forms — so the feed is built per published
//! rule: a rule whose concrete expansion count fits under
//! [`MAX_EXPANSIONS_PER_RULE`] contributes its whole phrases (best
//! recognition — Vosk sees complete utterances), and a larger rule is
//! decomposed at referenced-rule boundaries into fragment phrase lists,
//! relying on Vosk chaining grammar entries within one utterance. The
//! automaton, not Vosk, enforces which fragment sequences form a real
//! command; invalid orderings decode as clean words and die in the matcher's
//! re-sync path.
//!
//! Counting is multiplicative and saturating — nothing is materialized until
//! the count says it fits — the same trick the original expansion machinery
//! used to reject explosive phrases before allocating.

use std::collections::{HashMap, HashSet};

use super::{Alternation, Atom, Grammar, Term};

/// The most concrete phrases one rule may contribute whole. Kept at the old
/// per-command expansion cap so the recognizer sees grammars of the same
/// scale it always has.
pub const MAX_EXPANSIONS_PER_RULE: usize = 512;

/// The recognition grammar: every phrase to hand the recognizer (the caller
/// appends `"[unk]"`), plus the record of which rules had to be decomposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    /// Lowercased, space-joined phrases — whole commands where they fit,
    /// fragments where a rule decomposed — globally deduplicated in
    /// insertion order.
    pub phrases: Vec<String>,
    /// Every rule that was decomposed instead of expanded, in the order the
    /// walk met them. Deterministic, and reported by `validate`, since
    /// decomposition trades recognition accuracy for feasibility.
    pub decompositions: Vec<Decomposition>,
}

/// One rule the feed decomposed at its referenced-rule boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decomposition {
    /// The rule's name — published, or a private rule met while decomposing
    /// a published one.
    pub rule: String,
    /// The concrete expansion count that forced the decomposition (saturates
    /// at `usize::MAX`).
    pub expansions: usize,
}

/// Builds the recognition grammar for every published rule.
pub fn feed(grammar: &Grammar) -> Feed {
    let mut builder = FeedBuilder {
        grammar,
        counts: HashMap::new(),
        seen: HashSet::new(),
        phrases: Vec::new(),
        decompositions: Vec::new(),
        processed: HashSet::new(),
    };
    for rule in grammar.published() {
        builder.process_rule(&rule.name);
    }
    Feed {
        phrases: builder.phrases,
        decompositions: builder.decompositions,
    }
}

struct FeedBuilder<'g> {
    grammar: &'g Grammar,
    /// Expansion counts per rule, memoized — the graph is acyclic, so this
    /// terminates, and saturating arithmetic keeps explosive rules finite.
    counts: HashMap<String, usize>,
    seen: HashSet<String>,
    phrases: Vec<String>,
    decompositions: Vec<Decomposition>,
    /// Rules already contributed, whole or decomposed; a rule referenced from
    /// forty commands contributes once.
    processed: HashSet<String>,
}

impl<'g> FeedBuilder<'g> {
    /// Contributes one rule: whole phrases when its count fits under the cap,
    /// otherwise a decomposition record plus its fragments.
    fn process_rule(&mut self, name: &str) {
        if !self.processed.insert(name.to_owned()) {
            return;
        }
        let count = self.rule_count(name);
        let grammar = self.grammar;
        let rule = grammar.rule(name).expect("references are analyzed");
        if count <= MAX_EXPANSIONS_PER_RULE {
            let phrases = self.expand_alternation(&rule.pattern);
            self.contribute(phrases);
        } else {
            self.decompositions.push(Decomposition {
                rule: name.to_owned(),
                expansions: count,
            });
            self.decompose_alternation(&rule.pattern);
        }
    }

    fn contribute(&mut self, phrases: Vec<Vec<String>>) {
        for words in phrases {
            // A fully-optional fragment expands to the empty phrase, which
            // cannot be spoken and must not reach the recognizer.
            if words.is_empty() {
                continue;
            }
            let joined = words.join(" ");
            if self.seen.insert(joined.clone()) {
                self.phrases.push(joined);
            }
        }
    }

    // -- Counting (multiplicative, saturating, memoized) --------------------

    fn rule_count(&mut self, name: &str) -> usize {
        if let Some(&count) = self.counts.get(name) {
            return count;
        }
        let grammar = self.grammar;
        let rule = grammar.rule(name).expect("references are analyzed");
        let count = self.alternation_count(&rule.pattern);
        self.counts.insert(name.to_owned(), count);
        count
    }

    fn alternation_count(&mut self, alternation: &Alternation) -> usize {
        alternation.branches.iter().fold(0usize, |sum, branch| {
            let product = branch.terms.iter().fold(1usize, |product, term| {
                product.saturating_mul(self.term_count(term))
            });
            sum.saturating_add(product)
        })
    }

    fn term_count(&mut self, term: &Term) -> usize {
        let base = self.atom_count(&term.atom);
        let Some(repeat) = term.repeat else {
            return base;
        };
        // Each match count k contributes base^k distinct sequences.
        let mut total = 0usize;
        for k in repeat.min..=repeat.max {
            let mut power = 1usize;
            for _ in 0..k {
                power = power.saturating_mul(base);
            }
            total = total.saturating_add(power);
        }
        total
    }

    fn atom_count(&mut self, atom: &Atom) -> usize {
        match atom {
            Atom::Literal(_) => 1,
            Atom::Ref(name) => self.rule_count(name),
            Atom::Group(alternation) => self.alternation_count(alternation),
        }
    }

    // -- Expansion (only ever called under the cap) -------------------------

    fn expand_alternation(&mut self, alternation: &Alternation) -> Vec<Vec<String>> {
        let mut phrases = Vec::new();
        for branch in &alternation.branches {
            phrases.extend(self.expand_terms(&branch.terms));
        }
        phrases
    }

    fn expand_terms(&mut self, terms: &[Term]) -> Vec<Vec<String>> {
        let mut results: Vec<Vec<String>> = vec![Vec::new()];
        for term in terms {
            let options = self.expand_term(term);
            let mut extended = Vec::with_capacity(results.len().saturating_mul(options.len()));
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

    fn expand_term(&mut self, term: &Term) -> Vec<Vec<String>> {
        let base = self.expand_atom(&term.atom);
        let Some(repeat) = term.repeat else {
            return base;
        };
        let mut results = Vec::new();
        for k in repeat.min..=repeat.max {
            let mut sequences: Vec<Vec<String>> = vec![Vec::new()];
            for _ in 0..k {
                let mut extended = Vec::new();
                for prefix in &sequences {
                    for option in &base {
                        let mut sequence = prefix.clone();
                        sequence.extend_from_slice(option);
                        extended.push(sequence);
                    }
                }
                sequences = extended;
            }
            results.extend(sequences);
        }
        results
    }

    fn expand_atom(&mut self, atom: &Atom) -> Vec<Vec<String>> {
        match atom {
            Atom::Literal(words) => vec![words.clone()],
            Atom::Ref(name) => {
                let grammar = self.grammar;
                let rule = grammar.rule(name).expect("references are analyzed");
                self.expand_alternation(&rule.pattern)
            }
            Atom::Group(alternation) => self.expand_alternation(alternation),
        }
    }

    // -- Decomposition at referenced-rule boundaries ------------------------

    fn decompose_alternation(&mut self, alternation: &Alternation) {
        for branch in &alternation.branches {
            self.decompose_terms(&branch.terms);
        }
    }

    /// The connecting runs of reference-free terms become fragment phrases of
    /// their own; each referenced rule recurses through the same
    /// expand-or-decompose test.
    fn decompose_terms(&mut self, terms: &[Term]) {
        let mut run: Vec<&Term> = Vec::new();
        for term in terms {
            if is_reference_free(term) {
                run.push(term);
            } else {
                self.flush_run(&run);
                run.clear();
                self.decompose_term(term);
            }
        }
        self.flush_run(&run);
    }

    fn flush_run(&mut self, run: &[&Term]) {
        if run.is_empty() {
            return;
        }
        let count = run.iter().fold(1usize, |product, term| {
            product.saturating_mul(self.term_count(term))
        });
        if count <= MAX_EXPANSIONS_PER_RULE {
            let mut results: Vec<Vec<String>> = vec![Vec::new()];
            for term in run {
                let options = self.expand_term(term);
                let mut extended = Vec::new();
                for prefix in &results {
                    for option in &options {
                        let mut phrase = prefix.clone();
                        phrase.extend_from_slice(option);
                        extended.push(phrase);
                    }
                }
                results = extended;
            }
            self.contribute(results);
        } else {
            // Even the connecting run is too big whole: fall back to
            // per-term fragments, splitting again where a term alone is
            // still too big.
            for term in run {
                if self.term_count(term) <= MAX_EXPANSIONS_PER_RULE {
                    let phrases = self.expand_term(term);
                    self.contribute(phrases);
                } else {
                    self.decompose_term(term);
                }
            }
        }
    }

    /// Decomposing a term drops its repetition — the fragments chain in the
    /// recognizer, and the automaton is what enforces the counts.
    fn decompose_term(&mut self, term: &Term) {
        match &term.atom {
            Atom::Ref(name) => self.process_rule(name),
            Atom::Group(alternation) => {
                let count = self.alternation_count(alternation);
                if count <= MAX_EXPANSIONS_PER_RULE {
                    let phrases = self.expand_alternation(alternation);
                    self.contribute(phrases);
                } else {
                    self.decompose_alternation(alternation);
                }
            }
            Atom::Literal(words) => self.contribute(vec![words.clone()]),
        }
    }
}

fn is_reference_free(term: &Term) -> bool {
    match &term.atom {
        Atom::Literal(_) => true,
        Atom::Ref(_) => false,
        Atom::Group(alternation) => alternation
            .branches
            .iter()
            .all(|branch| branch.terms.iter().all(is_reference_free)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures;
    use super::*;

    fn feed_of(source: &str) -> Feed {
        feed(&Grammar::parse(source).expect("the grammar should load"))
    }

    #[test]
    fn test_a_rule_under_the_cap_expands_whole() {
        let feed = feed_of("Deploy = \"deploy\" \"the\"? (\"sentry\" | \"gun\") { 4 }");
        assert_eq!(
            feed.phrases,
            vec![
                "deploy sentry",
                "deploy gun",
                "deploy the sentry",
                "deploy the gun",
            ]
        );
        assert!(feed.decompositions.is_empty());
    }

    #[test]
    fn test_references_expand_through_when_the_count_fits() {
        let feed = feed_of("colour = \"red\" { 1 }\nTeam = \"team\" colour { ..., 9 }");
        assert_eq!(feed.phrases, vec!["team red"]);
        assert!(feed.decompositions.is_empty());
    }

    /// A group of four alternates, chained five times: 4^5 = 1024 expansions.
    const BIG: &str = "big = (\"a\"|\"b\"|\"c\"|\"d\") (\"a\"|\"b\"|\"c\"|\"d\") (\"a\"|\"b\"|\"c\"|\"d\") (\"a\"|\"b\"|\"c\"|\"d\") (\"a\"|\"b\"|\"c\"|\"d\")";

    #[test]
    fn test_a_rule_over_the_cap_decomposes_at_rule_boundaries() {
        let feed = feed_of(&format!("{BIG}\nUse = \"go\" big \"now\" {{ m }}"));
        // The connecting literal runs are fragments of their own, and the
        // oversized referenced rule falls back to per-term fragments.
        assert_eq!(feed.phrases, vec!["go", "a", "b", "c", "d", "now"]);
        assert_eq!(
            feed.decompositions,
            vec![
                Decomposition {
                    rule: "Use".to_owned(),
                    expansions: 1024,
                },
                Decomposition {
                    rule: "big".to_owned(),
                    expansions: 1024,
                },
            ]
        );
    }

    #[test]
    fn test_a_decomposed_rule_contributes_once() {
        // Two commands sharing the oversized rule: one decomposition record,
        // no duplicate fragments.
        let feed = feed_of(&format!(
            "{BIG}\nUse = \"go\" big {{ m }}\nAlso = \"run\" big {{ i }}"
        ));
        assert_eq!(feed.phrases, vec!["go", "a", "b", "c", "d", "run"]);
        assert_eq!(
            feed.decompositions
                .iter()
                .filter(|decomposition| decomposition.rule == "big")
                .count(),
            1
        );
    }

    #[test]
    fn test_phrases_are_globally_deduplicated_in_insertion_order() {
        let feed = feed_of("Map = \"map\" | \"toggle map\" { m }\nAlso = \"map\" { m }");
        assert_eq!(feed.phrases, vec!["map", "toggle map"]);
    }

    #[test]
    fn test_the_feed_is_deterministic() {
        let source = fixtures::arma_source();
        let grammar = Grammar::parse(&source).expect("the canonical grammar should load");
        assert_eq!(feed(&grammar), feed(&grammar));
    }

    #[test]
    fn test_the_arma_feed_decomposes_the_subject_rules() {
        let source = fixtures::arma_source();
        let feed = feed(&Grammar::parse(&source).expect("the canonical grammar should load"));

        let decomposed: Vec<&str> = feed
            .decompositions
            .iter()
            .map(|decomposition| decomposition.rule.as_str())
            .collect();
        // Every subject-led command is over the cap (the subject alone admits
        // thousands of forms), and so are the subject rules themselves.
        for rule in [
            "ReturnToFormation",
            "Watch",
            "Assign",
            "subject",
            "squad_selection",
        ] {
            assert!(
                decomposed.contains(&rule),
                "'{rule}' should decompose, got: {decomposed:?}"
            );
        }
        assert!(
            feed.decompositions
                .iter()
                .all(|decomposition| decomposition.expansions > MAX_EXPANSIONS_PER_RULE),
            "every decomposition should record the count that forced it"
        );
    }

    #[test]
    fn test_the_arma_feed_carries_whole_and_fragment_phrases() {
        let source = fixtures::arma_source();
        let feed = feed(&Grammar::parse(&source).expect("the canonical grammar should load"));

        assert!(!feed.phrases.is_empty());
        // Small published rules contribute whole phrases…
        assert!(feed.phrases.iter().any(|phrase| phrase == "toggle map"));
        // …the subject decomposes into its own fragments…
        // The `("and"? squad_number)` group fits under the cap, so it
        // arrives as whole two-word fragments rather than a bare "and".
        for fragment in ["one", "and three", "team red", "all"] {
            assert!(
                feed.phrases.iter().any(|phrase| phrase == fragment),
                "the feed should carry the fragment '{fragment}'"
            );
        }
        // …and connecting runs and direct objects arrive as fragments too.
        for fragment in ["fall back", "north east", "wedge", "assign to"] {
            assert!(
                feed.phrases.iter().any(|phrase| phrase == fragment),
                "the feed should carry the fragment '{fragment}'"
            );
        }

        let mut deduped = feed.phrases.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            feed.phrases.len(),
            "the feed must be deduped"
        );

        assert!(
            feed.phrases
                .iter()
                .all(|phrase| *phrase == phrase.to_lowercase()),
            "every phrase reaches the recognizer lowercased"
        );
    }
}
