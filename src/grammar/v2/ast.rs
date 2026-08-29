//! The owned, spanned AST for grammar v2.
//!
//! Everything here is `String`-owned and carries byte-offset [`Span`]s into
//! the source it was parsed from — no borrowed lifetimes, no self-referential
//! owner, no `unsafe`. The automaton compiler (G4) consumes this tree as-is,
//! so the shape is public API: a grammar is a list of [`Rule`]s, each rule an
//! [`Alternation`] of [`Branch`]es, each branch a sequence of [`Term`]s.

use std::time::Duration;

use crate::output::keys::{self, KeyCode};

/// The global repetition cap: `*` and `+` desugar to `[0..8]` / `[1..8]`, and
/// `[n..]` fills its missing end with it. Explicit bounds may exceed it — the
/// cap exists to make the sugared forms finite, not to limit what a grammar
/// may say outright.
pub const MAX_REPETITION: usize = 8;

/// A byte-offset range into the grammar source, `start..end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// One rule definition: `name = pattern [ { actions } ]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    /// The rule's name. TitleCase names are published commands, lowercase
    /// names private building blocks — see [`Rule::published`].
    pub name: String,
    /// The span of the name alone, for diagnostics about the rule itself.
    pub name_span: Span,
    /// What the rule matches.
    pub pattern: Alternation,
    /// The rule's trailing action block. `None` means the rule implicitly
    /// propagates its accumulated commands, equivalent to `{ ... }`.
    pub actions: Option<ActionBlock>,
    /// The whole definition, name through final action or term.
    pub span: Span,
}

impl Rule {
    /// Whether this rule is published as a speakable command.
    ///
    /// Publication is spelled with the name itself: a leading uppercase letter
    /// publishes the rule, anything else keeps it private.
    pub fn published(&self) -> bool {
        self.name.chars().next().is_some_and(char::is_uppercase)
    }
}

/// One or more branches separated by `|`. A single-branch alternation is how
/// a plain sequence appears — the parser does not collapse the wrapper, so
/// consumers only ever deal with one shape.
#[derive(Clone, Debug, PartialEq)]
pub struct Alternation {
    pub branches: Vec<Branch>,
    pub span: Span,
}

/// One alternation branch: a sequence of terms, optionally terminated by an
/// inline action block.
///
/// Inline blocks only exist inside parenthesized groups (`("one" { f1 } |
/// "two" { f2 })`), where they bind to the branch they terminate. At rule
/// level a trailing block belongs to the [`Rule`], so top-level branches
/// always carry `actions: None`.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    pub terms: Vec<Term>,
    pub actions: Option<ActionBlock>,
    pub span: Span,
}

/// One term of a sequence: an atom with its optional repetition and capture,
/// `atom[bounds]:name`.
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub atom: Atom,
    /// The atom's own span, without the repetition/capture suffixes — where
    /// diagnostics about the atom itself (an undefined rule, say) point.
    pub atom_span: Span,
    pub repeat: Option<Repeat>,
    pub capture: Option<Capture>,
    pub span: Span,
}

/// The matchable core of a term.
#[derive(Clone, Debug, PartialEq)]
pub enum Atom {
    /// A quoted literal, as its lowercased spoken words (a multi-word literal
    /// is one atom matching each word in sequence).
    Literal(Vec<String>),
    /// A reference to another rule by name.
    Ref(String),
    /// A parenthesized group.
    Group(Alternation),
}

/// An explicit repetition bound. `?`, `*` and `+` desugar here as `[0..1]`,
/// `[0..MAX_REPETITION]` and `[1..MAX_REPETITION]`; `[n]` as `[n..n]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repeat {
    pub min: usize,
    pub max: usize,
    pub span: Span,
}

/// A capture name attached to a term with `:name`. The term's accumulated
/// commands collect under this name for `name...` splices in action blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub name: String,
    pub span: Span,
}

/// A `{ ... }` action block: one or more comma-separated actions.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionBlock {
    pub actions: Vec<Action>,
    pub span: Span,
}

/// One action item with its span.
#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub kind: ActionKind,
    pub span: Span,
}

/// What one action item does when the command fires.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionKind {
    /// A bare chord: press and release, with the default pacing applied.
    Press(Chord),
    /// `hold(chord)`: press without the paired release.
    Hold(Chord),
    /// `release(chord)`: release without the paired press.
    Release(Chord),
    /// `release(*)`: release every key the virtual keyboard currently holds.
    ReleaseAll,
    /// `wait(duration)`: an explicit pause, replacing the implicit interval.
    Wait(Duration),
    /// `...`: splice the entire accumulated child command vector.
    SpliceAll,
    /// `name...`: splice the commands accumulated under one capture.
    SpliceCapture(String),
}

/// A chord: one key name, or several joined with `+` (`shift+f1`).
///
/// Key names stay unresolved in the AST — static analysis checks every
/// segment against the key table, so after a clean analysis [`Chord::keys`]
/// cannot fail.
#[derive(Clone, Debug, PartialEq)]
pub struct Chord {
    pub segments: Vec<ChordSegment>,
    pub span: Span,
}

/// One `+`-separated segment of a chord.
#[derive(Clone, Debug, PartialEq)]
pub struct ChordSegment {
    pub name: String,
    pub span: Span,
}

impl ChordSegment {
    /// The key this segment names, if the key table knows it.
    pub fn key(&self) -> Option<KeyCode> {
        keys::from_name(&self.name)
    }
}

impl Chord {
    /// The resolved keys of this chord, in written order, or `None` if any
    /// segment is not a key name (analysis reports which one).
    pub fn keys(&self) -> Option<Vec<KeyCode>> {
        self.segments.iter().map(ChordSegment::key).collect()
    }
}

impl std::fmt::Display for Chord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, segment) in self.segments.iter().enumerate() {
            if index > 0 {
                formatter.write_str("+")?;
            }
            formatter.write_str(&segment.name)?;
        }
        Ok(())
    }
}
