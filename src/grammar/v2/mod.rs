//! Grammar v2: the composable, rule-based command grammar language.
//! See DESIGN.md §"Grammar v2: composable command grammars".
//!
//! This module owns the surface syntax: lexing and parsing (chumsky), the
//! spanned AST, static analysis, and diagnostics (ariadne). The automaton
//! compiler and matcher build on the [`Grammar`] produced here; they are not
//! part of this module.
//!
//! The parse is a two-stage pipeline — lexer → spanned token stream → parser —
//! with errors accumulated across lexing, parsing *and* analysis, so loading a
//! grammar once reports every problem it has.

// Consumed as the automaton compiler, matcher and profile wiring (G4–G6) land.
#![allow(dead_code)]

mod analysis;
mod ast;
mod diagnostic;
mod lexer;
mod parser;
mod token;

use std::collections::BTreeSet;

#[allow(unused_imports)] // consumed as the automaton compiler (G4) lands
pub use ast::{
    Action, ActionBlock, ActionKind, Alternation, Atom, Branch, Capture, Chord, ChordSegment,
    MAX_REPETITION, Repeat, Rule, Span, Term,
};
#[allow(unused_imports)] // consumed as the profile wiring (G6) lands
pub use diagnostic::{Diagnostic, DiagnosticKind, user_error};

/// A parsed and analyzed grammar: the spanned rule list plus everything the
/// automaton compiler and `validate` need to know about it.
#[derive(Clone, Debug)]
pub struct Grammar {
    source: String,
    rules: Vec<Rule>,
    lints: Vec<Diagnostic>,
}

impl Grammar {
    /// Lexes, parses and statically analyzes a grammar source.
    ///
    /// Diagnostics accumulate across all three stages, so a grammar with a
    /// syntax error in one rule and an unknown key in another reports both in
    /// this single call. Any error diagnostic fails the load; lints alone do
    /// not — they ride along on the returned grammar ([`Grammar::lints`]).
    /// Fold a failure into `crate::Error` with [`user_error`].
    pub fn parse(source: &str) -> Result<Self, Vec<Diagnostic>> {
        let (rules, mut diagnostics) = parser::parse(source);

        let Some(rules) = rules else {
            return Err(diagnostics);
        };

        let mut lints = Vec::new();
        for finding in analysis::analyze(&rules) {
            if finding.kind.is_error() {
                diagnostics.push(finding);
            } else {
                lints.push(finding);
            }
        }

        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind.is_error())
        {
            diagnostics.extend(lints);
            diagnostics.sort_by_key(|diagnostic| (diagnostic.span.start, diagnostic.span.end));
            return Err(diagnostics);
        }

        Ok(Self {
            source: source.to_owned(),
            rules,
            lints,
        })
    }

    /// The grammar exactly as it was written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Every rule, in definition order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Looks up a rule by name.
    pub fn rule(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.name == name)
    }

    /// The published rules — the speakable commands — in definition order.
    pub fn published(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter(|rule| rule.published())
    }

    /// The warnings static analysis produced. These never fail a load, but
    /// `validate` surfaces them.
    pub fn lints(&self) -> &[Diagnostic] {
        &self.lints
    }

    /// Every distinct lowercased literal word in the grammar, for vocabulary
    /// validation against the model. Linear in the size of the rule list — no
    /// expansion happens.
    pub fn word_set(&self) -> BTreeSet<String> {
        fn collect(alternation: &Alternation, words: &mut BTreeSet<String>) {
            for branch in &alternation.branches {
                for term in &branch.terms {
                    match &term.atom {
                        Atom::Literal(literal) => {
                            words.extend(literal.iter().cloned());
                        }
                        Atom::Group(inner) => collect(inner, words),
                        Atom::Ref(_) => {}
                    }
                }
            }
        }

        let mut words = BTreeSet::new();
        for rule in &self.rules {
            collect(&rule.pattern, &mut words);
        }
        words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `grammar:` block of the canonical Arma profile.
    fn arma_grammar() -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/profiles/arma.yaml");
        let raw = std::fs::read_to_string(path).expect("profiles/arma.yaml should be readable");
        let profile: serde_yaml::Value =
            serde_yaml::from_str(&raw).expect("profiles/arma.yaml should be valid YAML");
        profile["grammar"]
            .as_str()
            .expect("profiles/arma.yaml should carry an inline grammar block")
            .to_owned()
    }

    /// The canonical profile must stay honest: it parses and analyzes with no
    /// errors and no lint warnings.
    #[test]
    fn test_the_canonical_arma_grammar_is_clean() {
        let source = arma_grammar();
        let grammar = Grammar::parse(&source).unwrap_or_else(|diagnostics| {
            panic!(
                "profiles/arma.yaml should parse cleanly:\n{}",
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.render(&source))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });

        assert!(
            grammar.lints().is_empty(),
            "profiles/arma.yaml should be lint-free:\n{}",
            grammar
                .lints()
                .iter()
                .map(|lint| lint.render(&source))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_the_arma_grammar_shape_is_what_the_profile_promises() {
        let source = arma_grammar();
        let grammar = Grammar::parse(&source).expect("the canonical grammar should load");

        // A few landmarks, so a refactor that quietly drops rules is loud.
        for name in ["Map", "Select", "Watch", "Formation", "Assign"] {
            let rule = grammar
                .rule(name)
                .unwrap_or_else(|| panic!("missing rule '{name}'"));
            assert!(rule.published(), "'{name}' should be published");
        }
        for name in ["subject", "squad_selection", "direction", "assign_colour"] {
            let rule = grammar
                .rule(name)
                .unwrap_or_else(|| panic!("missing rule '{name}'"));
            assert!(!rule.published(), "'{name}' should be private");
        }

        let words = grammar.word_set();
        for word in ["map", "advance", "north", "wedge", "i'm"] {
            assert!(words.contains(word), "the word set should contain '{word}'");
        }
        assert!(
            words.iter().all(|word| word
                .chars()
                .all(|c| c.is_alphanumeric() || c == '\'' || c == '-')),
            "every vocabulary word should be speakable"
        );
    }

    #[test]
    fn test_word_set_is_lowercased_and_deduplicated() {
        let grammar =
            Grammar::parse("Map = \"MAP\" | \"map\" | \"toggle map\" { m }").expect("should load");
        let words: Vec<String> = grammar.word_set().into_iter().collect();
        assert_eq!(words, vec!["map".to_owned(), "toggle".to_owned()]);
    }

    #[test]
    fn test_published_iterates_speakable_commands_only() {
        let grammar =
            Grammar::parse("Map = \"map\" { m }\nsubject = \"all\" { grave }\nSelect = subject")
                .expect("should load");
        let published: Vec<&str> = grammar.published().map(|rule| rule.name.as_str()).collect();
        assert_eq!(published, vec!["Map", "Select"]);
        assert_eq!(grammar.rules().len(), 3);
    }

    #[test]
    fn test_failures_fold_into_a_single_user_error() {
        let source = "Map = \"map\" { notakey }";
        let diagnostics = Grammar::parse(source).expect_err("the key should be rejected");
        let error = user_error(&diagnostics, source);
        let message = error.to_string();
        assert!(message.contains("notakey"), "got: {message}");
        assert!(
            message.contains("Your grammar has a problem"),
            "got: {message}"
        );
    }

    #[test]
    fn test_syntax_and_analysis_problems_accumulate_in_one_load() {
        // A syntax error in the first rule, an unknown key in the second: one
        // load reports both.
        let source = "One = \"a\" { m\nTwo = \"b\" { notakey }";
        let diagnostics = Grammar::parse(source).expect_err("the grammar should be rejected");
        let messages: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("notakey")),
            "the analysis error should be present: {messages:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::Syntax),
            "the syntax error should be present: {messages:?}"
        );
    }
}
