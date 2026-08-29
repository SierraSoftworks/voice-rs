//! The word-level transducer a grammar compiles into, and the hypothesis walk
//! the matcher drives over it. See DESIGN.md §"Compilation: a word-level
//! transducer".
//!
//! The compiler inlines every rule reference (analysis guarantees there are no
//! cycles) and unrolls every bounded repetition, so the automaton is a finite
//! DAG: transitions consume exactly one recognized word, and the epsilon
//! structure that Thompson-style construction produces is flattened away
//! before the walk ever sees it. Output is *not* carried as raw key presses on
//! the edges — a rule's action block may reorder its children's contributions
//! (`{ sub..., 3, 8, dir... }`), so edges instead carry scope operations
//! (enter/exit a rule or actioned branch, open/close a capture) and every
//! block is evaluated the moment its scope closes, exactly as DESIGN.md
//! §"Command semantics" describes the accumulation.
//!
//! Determinization is deliberately not attempted: the same word can carry
//! different outputs by context ("red" is leftshift+f1 as a subject, plain 1
//! as an assign object). The walk instead keeps a set of alive [`Hypothesis`]
//! values, each carrying its state and accumulated vectors, which stays small
//! because utterances are short and word-level branching is low.

use std::collections::HashMap;

use crate::output::assembly::ActionItem;

use super::ast::Span;
use super::diagnostic::Diagnostic;
use super::{ActionBlock, ActionKind, Alternation, Atom, Branch, Grammar, Rule, Term};

/// The most NFA states — and, independently, the most flattened transitions —
/// a grammar may compile into. Exceeding it is a load-time error naming the
/// largest rules.
///
/// The bound is generous on purpose: the canonical Arma profile (forty
/// published commands, each inlining a subject rule that itself unrolls a
/// nine-iteration repetition) lands in the tens of thousands, so 200k leaves
/// several times that headroom while still catching a runaway repetition
/// (`[1000]` of a large group) long before it exhausts memory.
pub const MAX_AUTOMATON_STATES: usize = 200_000;

/// The most simultaneous hypotheses a walk will follow.
///
/// A compile-time bound is not statically derivable here — hypotheses are
/// distinguished by their accumulated output, not just their state, so subset
/// analysis over states undercounts — and so this is a runtime guard instead:
/// a step that would exceed it drops the walk dead and records a warning
/// ([`Walk::warning`]) rather than panicking or stalling. The value is far
/// above anything a sane grammar produces (Arma peaks around one hypothesis
/// per published rule, ~50); a grammar that trips it is nondeterministic in a
/// way that multiplies readings per word, which duplicate detection usually
/// reports at load time first.
pub const MAX_HYPOTHESES: usize = 512;

/// One step of an action program: a concrete action, or a splice resolved
/// against the accumulation when the program runs.
#[derive(Clone, Debug, PartialEq)]
enum ProgramItem {
    /// A concrete action, resolved from the block at compile time.
    Item(ActionItem),
    /// `...`: the whole accumulated vector of the evaluating scope.
    SpliceAll,
    /// `name...`: everything accumulated under one capture.
    SpliceCapture(u32),
}

type Program = Vec<ProgramItem>;

/// An output operation carried on a transition, applied in order before the
/// word itself is recorded.
#[derive(Clone, Debug, PartialEq)]
enum Op {
    /// Start collecting a `term:name` capture in the nearest rule scope.
    OpenCapture(u32),
    /// The capture's term is done matching.
    CloseCapture,
    /// A rule reference starts matching: its accumulation is its own.
    EnterRule,
    /// A group branch with an inline action block starts matching.
    EnterBranch,
    /// The scope's pattern is fully matched: evaluate its action program and
    /// append the result to the enclosing scope's accumulation.
    Exit(u32),
}

/// A flattened transition: consume `word`, applying `ops` first.
#[derive(Clone, Debug)]
struct Transition {
    word: u32,
    ops: Vec<Op>,
    target: usize,
}

/// A published rule accepted at a state, with the ops that close the scopes
/// still open between the last word and the rule's end.
#[derive(Clone, Debug)]
struct AcceptEntry {
    rule: usize,
    ops: Vec<Op>,
}

/// One state of the flattened automaton: only word-consuming transitions,
/// epsilon paths already folded into `ops`.
#[derive(Clone, Debug, Default)]
struct FlatState {
    transitions: Vec<Transition>,
    accepts: Vec<AcceptEntry>,
}

/// A published rule as the automaton knows it.
#[derive(Clone, Debug)]
struct CompiledRule {
    name: String,
    name_span: Span,
    /// The rule's action program — the parsed block, or the implicit `{ ... }`.
    program: u32,
}

/// The compiled word-level transducer for one grammar.
#[derive(Clone, Debug)]
pub struct Automaton {
    /// Flattened states; index 0 is the root every walk starts from.
    states: Vec<FlatState>,
    rules: Vec<CompiledRule>,
    programs: Vec<Program>,
    /// Every word the grammar can consume, interned; a word outside this map
    /// kills every hypothesis, which is the out-of-grammar case.
    words: HashMap<String, u32>,
    /// NFA states contributed per published rule (inlined references and
    /// unrolled repetitions included), for `validate`'s size report.
    rule_sizes: Vec<(String, usize)>,
}

impl Automaton {
    /// Compiles an analyzed grammar into its transducer.
    ///
    /// Fails with load-time diagnostics when the automaton exceeds
    /// [`MAX_AUTOMATON_STATES`] (naming the largest rules) or when two
    /// published rules accept the same word sequence with different outputs.
    pub fn compile(grammar: &Grammar) -> Result<Self, Vec<Diagnostic>> {
        let mut builder = Builder::new(grammar);
        if builder.compile().is_err() {
            return Err(vec![builder.overflow_diagnostic()]);
        }

        let Ok(states) = builder.flatten() else {
            return Err(vec![builder.overflow_diagnostic()]);
        };

        let automaton = Self {
            states,
            rules: builder.rules,
            programs: builder.programs,
            words: builder.words,
            rule_sizes: builder.rule_sizes,
        };

        Ok(automaton)
    }

    /// Starts a hypothesis walk from the root.
    pub fn walk(&self) -> Walk<'_> {
        Walk::new(self)
    }

    /// NFA states contributed per published rule, in definition order — the
    /// per-rule size report `validate` shows so a `*` at the repetition cap is
    /// visible.
    pub fn rule_sizes(&self) -> &[(String, usize)] {
        &self.rule_sizes
    }
}

// ---------------------------------------------------------------------------
// Compilation: AST → NFA with output ops on epsilon edges.
// ---------------------------------------------------------------------------

/// The state cap was hit; the caller turns this into a diagnostic. Carrying no
/// detail keeps every compile function's signature simple — the builder itself
/// knows the per-rule attribution.
struct Overflow;

#[derive(Debug, Default)]
struct NfaState {
    edges: Vec<Edge>,
    /// The published rule whose pattern ends here, if any.
    accept: Option<usize>,
}

#[derive(Debug)]
enum Edge {
    /// A free move, applying at most one output op.
    Eps(Option<Op>, usize),
    /// Consume one word.
    Word(u32, usize),
}

struct Builder<'g> {
    grammar: &'g Grammar,
    states: Vec<NfaState>,
    words: HashMap<String, u32>,
    capture_ids: HashMap<String, u32>,
    programs: Vec<Program>,
    /// Rule blocks interned once per rule; inline branch blocks once per
    /// source block (keyed by span), so forty inlinings of `subject` share
    /// one copy of each program.
    rule_programs: HashMap<String, u32>,
    block_programs: HashMap<(usize, usize), u32>,
    rules: Vec<CompiledRule>,
    rule_sizes: Vec<(String, usize)>,
}

impl<'g> Builder<'g> {
    fn new(grammar: &'g Grammar) -> Self {
        Self {
            grammar,
            states: Vec::new(),
            words: HashMap::new(),
            capture_ids: HashMap::new(),
            programs: Vec::new(),
            rule_programs: HashMap::new(),
            block_programs: HashMap::new(),
            rules: Vec::new(),
            rule_sizes: Vec::new(),
        }
    }

    fn compile(&mut self) -> Result<(), Overflow> {
        let root = self.state()?;
        debug_assert_eq!(root, 0);

        let grammar = self.grammar;
        for rule in grammar.published() {
            let before = self.states.len();
            let program = self.rule_program(rule);
            let index = self.rules.len();
            self.rules.push(CompiledRule {
                name: rule.name.clone(),
                name_span: rule.name_span,
                program,
            });

            let compiled = self.compile_alternation(&rule.pattern);
            // Attribute the states even when the cap was hit mid-rule, so the
            // overflow diagnostic can name this rule as a contributor.
            self.rule_sizes
                .push((rule.name.clone(), self.states.len() - before));
            let (start, end) = compiled?;
            self.eps(root, start, None);
            self.states[end].accept = Some(index);
        }

        Ok(())
    }

    fn state(&mut self) -> Result<usize, Overflow> {
        if self.states.len() >= MAX_AUTOMATON_STATES {
            return Err(Overflow);
        }
        self.states.push(NfaState::default());
        Ok(self.states.len() - 1)
    }

    fn eps(&mut self, from: usize, to: usize, op: Option<Op>) {
        self.states[from].edges.push(Edge::Eps(op, to));
    }

    fn word_edge(&mut self, from: usize, to: usize, word: &str) {
        let next = self.words.len() as u32;
        let id = *self.words.entry(word.to_owned()).or_insert(next);
        self.states[from].edges.push(Edge::Word(id, to));
    }

    fn capture_id(&mut self, name: &str) -> u32 {
        let next = self.capture_ids.len() as u32;
        *self.capture_ids.entry(name.to_owned()).or_insert(next)
    }

    fn intern_program(&mut self, program: Program) -> u32 {
        self.programs.push(program);
        (self.programs.len() - 1) as u32
    }

    /// The action program of a rule: its trailing block, or the implicit
    /// propagation `{ ... }` when it has none.
    fn rule_program(&mut self, rule: &Rule) -> u32 {
        if let Some(&id) = self.rule_programs.get(&rule.name) {
            return id;
        }
        let program = match &rule.actions {
            Some(block) => self.lower_block(block),
            None => vec![ProgramItem::SpliceAll],
        };
        let id = self.intern_program(program);
        self.rule_programs.insert(rule.name.clone(), id);
        id
    }

    fn branch_program(&mut self, block: &ActionBlock) -> u32 {
        let key = (block.span.start, block.span.end);
        if let Some(&id) = self.block_programs.get(&key) {
            return id;
        }
        let program = self.lower_block(block);
        let id = self.intern_program(program);
        self.block_programs.insert(key, id);
        id
    }

    fn lower_block(&mut self, block: &ActionBlock) -> Program {
        block
            .actions
            .iter()
            .map(|action| match &action.kind {
                ActionKind::Press(chord) => ProgramItem::Item(ActionItem::Press(
                    chord.keys().expect("analysis resolves every key name"),
                )),
                ActionKind::Hold(chord) => ProgramItem::Item(ActionItem::Hold(
                    chord.keys().expect("analysis resolves every key name"),
                )),
                ActionKind::Release(chord) => ProgramItem::Item(ActionItem::Release(
                    chord.keys().expect("analysis resolves every key name"),
                )),
                ActionKind::ReleaseAll => ProgramItem::Item(ActionItem::ReleaseAll),
                ActionKind::Wait(duration) => ProgramItem::Item(ActionItem::Wait(*duration)),
                ActionKind::SpliceAll => ProgramItem::SpliceAll,
                ActionKind::SpliceCapture(name) => {
                    ProgramItem::SpliceCapture(self.capture_id(name))
                }
            })
            .collect()
    }

    fn compile_alternation(
        &mut self,
        alternation: &Alternation,
    ) -> Result<(usize, usize), Overflow> {
        let start = self.state()?;
        let end = self.state()?;
        for branch in &alternation.branches {
            let (branch_start, branch_end) = self.compile_branch(branch)?;
            self.eps(start, branch_start, None);
            self.eps(branch_end, end, None);
        }
        Ok((start, end))
    }

    fn compile_branch(&mut self, branch: &Branch) -> Result<(usize, usize), Overflow> {
        let start = self.state()?;
        let mut current = start;

        // A branch with an inline block is its own scope: `...` in the block
        // splices what the branch itself accumulated, and the block's result
        // — not the raw accumulation — is what the branch contributes.
        let program = branch
            .actions
            .as_ref()
            .map(|block| self.branch_program(block));
        if program.is_some() {
            let entered = self.state()?;
            self.eps(current, entered, Some(Op::EnterBranch));
            current = entered;
        }

        for term in &branch.terms {
            let (term_start, term_end) = self.compile_term(term)?;
            self.eps(current, term_start, None);
            current = term_end;
        }

        if let Some(program) = program {
            let exited = self.state()?;
            self.eps(current, exited, Some(Op::Exit(program)));
            current = exited;
        }

        Ok((start, current))
    }

    fn compile_term(&mut self, term: &Term) -> Result<(usize, usize), Overflow> {
        let (min, max) = term
            .repeat
            .map(|repeat| (repeat.min, repeat.max))
            .unwrap_or((1, 1));

        // Repetition unrolls: the required copies chain, then the optional
        // tail nests (`X (X (X)?)?`) so each match count has exactly one
        // path — a flat `X? X? X?` would let one occurrence match in any
        // slot, manufacturing spurious ambiguity.
        let start = self.state()?;
        let mut current = start;
        for _ in 0..min {
            let (atom_start, atom_end) = self.compile_atom(&term.atom)?;
            self.eps(current, atom_start, None);
            current = atom_end;
        }
        let end = self.state()?;
        for _ in min..max {
            self.eps(current, end, None);
            let (atom_start, atom_end) = self.compile_atom(&term.atom)?;
            self.eps(current, atom_start, None);
            current = atom_end;
        }
        self.eps(current, end, None);

        // The capture wraps the whole repetition, so every iteration appends
        // into the same capture, in spoken order.
        if let Some(capture) = &term.capture {
            let id = self.capture_id(&capture.name);
            let opened = self.state()?;
            let closed = self.state()?;
            self.eps(opened, start, Some(Op::OpenCapture(id)));
            self.eps(end, closed, Some(Op::CloseCapture));
            Ok((opened, closed))
        } else {
            Ok((start, end))
        }
    }

    fn compile_atom(&mut self, atom: &Atom) -> Result<(usize, usize), Overflow> {
        match atom {
            Atom::Literal(words) => {
                let start = self.state()?;
                let mut current = start;
                for word in words {
                    let next = self.state()?;
                    self.word_edge(current, next, word);
                    current = next;
                }
                Ok((start, current))
            }
            Atom::Ref(name) => {
                let grammar = self.grammar;
                let rule = grammar
                    .rule(name)
                    .expect("analysis rejects references to undefined rules");
                let program = self.rule_program(rule);
                let start = self.state()?;
                let end = self.state()?;
                let (body_start, body_end) = self.compile_alternation(&rule.pattern)?;
                self.eps(start, body_start, Some(Op::EnterRule));
                self.eps(body_end, end, Some(Op::Exit(program)));
                Ok((start, end))
            }
            Atom::Group(alternation) => self.compile_alternation(alternation),
        }
    }

    /// Folds the epsilon structure away: every state a word transition can
    /// land on (plus the root) becomes a flat state whose transitions carry
    /// the ops of the epsilon path leading to each word edge, and whose
    /// accepts carry the ops of the path to each accepting state.
    ///
    /// The NFA is a DAG, so epsilon-path enumeration terminates; the total
    /// transition count is capped by [`MAX_AUTOMATON_STATES`] too, because a
    /// diamond of nullable actioned branches multiplies paths rather than
    /// states.
    fn flatten(&self) -> Result<Vec<FlatState>, Overflow> {
        let mut flat_index: HashMap<usize, usize> = HashMap::new();
        let mut significant: Vec<usize> = vec![0];
        flat_index.insert(0, 0);
        for state in &self.states {
            for edge in &state.edges {
                if let Edge::Word(_, target) = edge
                    && !flat_index.contains_key(target)
                {
                    flat_index.insert(*target, significant.len());
                    significant.push(*target);
                }
            }
        }

        let mut flat: Vec<FlatState> = vec![FlatState::default(); significant.len()];
        let mut total = 0usize;
        for (index, &origin) in significant.iter().enumerate() {
            let mut stack: Vec<(usize, Vec<Op>)> = vec![(origin, Vec::new())];
            while let Some((node, ops)) = stack.pop() {
                if let Some(rule) = self.states[node].accept {
                    flat[index].accepts.push(AcceptEntry {
                        rule,
                        ops: ops.clone(),
                    });
                    total += 1;
                }
                for edge in &self.states[node].edges {
                    match edge {
                        Edge::Word(word, target) => {
                            flat[index].transitions.push(Transition {
                                word: *word,
                                ops: ops.clone(),
                                target: flat_index[target],
                            });
                            total += 1;
                        }
                        Edge::Eps(op, target) => {
                            let mut next = ops.clone();
                            if let Some(op) = op {
                                next.push(op.clone());
                            }
                            stack.push((*target, next));
                        }
                    }
                }
                if total > MAX_AUTOMATON_STATES {
                    return Err(Overflow);
                }
            }
        }

        Ok(flat)
    }

    fn overflow_diagnostic(&self) -> Diagnostic {
        let mut sizes = self.rule_sizes.clone();
        sizes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let largest: Vec<String> = sizes
            .iter()
            .take(3)
            .map(|(name, count)| format!("'{name}' ({count} states)"))
            .collect();
        let span = sizes
            .first()
            .and_then(|(name, _)| self.grammar.rule(name))
            .map(|rule| rule.name_span)
            .unwrap_or(Span::new(0, 0));

        Diagnostic::analysis(
            format!(
                "Your grammar compiles into more than {MAX_AUTOMATON_STATES} automaton states, which is more than we can load. The largest commands are {}.",
                largest.join(", ")
            ),
            span,
        )
        .with_help(
            "Every repetition bound multiplies the size of everything it repeats, and every rule reference copies the whole referenced rule. Lower the largest bounds, or split the biggest rules into smaller commands.",
        )
    }
}

// ---------------------------------------------------------------------------
// The hypothesis walk.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum ScopeKind {
    Rule,
    Branch,
}

/// One capture's accumulation: the evaluated commands of its term, and the
/// words the term matched (for the display name).
#[derive(Clone, Debug, PartialEq)]
struct CaptureSlot {
    id: u32,
    items: Vec<ActionItem>,
    words: Vec<String>,
}

/// One accumulation scope: a rule instantiation, or a group branch with an
/// inline action block.
///
/// Captures are rule-scoped, so slots always live on a `Rule` scope; a
/// capture opened while a `Branch` scope is on top registers its slot in the
/// nearest rule scope but keeps the *open* marker on the branch, which is
/// what routes contributions correctly: only what the capture's own term
/// contributes reaches the slot, never the branch's block-evaluated result on
/// top of it.
#[derive(Clone, Debug, PartialEq)]
struct Scope {
    kind: ScopeKind,
    /// The scope's accumulated command vector, in matched order.
    emitted: Vec<ActionItem>,
    /// Capture slots (rule scopes only), in the order they opened.
    captures: Vec<CaptureSlot>,
    /// Captures currently open in *this* scope, as indices into the nearest
    /// rule scope's `captures`.
    open: Vec<usize>,
}

impl Scope {
    fn new(kind: ScopeKind) -> Self {
        Self {
            kind,
            emitted: Vec::new(),
            captures: Vec::new(),
            open: Vec::new(),
        }
    }
}

/// One alive reading of the utterance so far.
#[derive(Clone, Debug, PartialEq)]
struct Hypothesis {
    state: usize,
    /// The scope stack; `scopes[0]` is the published rule's own scope, and it
    /// never pops — accept entries close everything above it.
    scopes: Vec<Scope>,
}

impl Hypothesis {
    fn root() -> Self {
        Self {
            state: 0,
            scopes: vec![Scope::new(ScopeKind::Rule)],
        }
    }

    fn nearest_rule(&self) -> usize {
        self.scopes
            .iter()
            .rposition(|scope| scope.kind == ScopeKind::Rule)
            .expect("the root scope is a rule scope and never pops")
    }

    fn apply(&mut self, op: &Op, automaton: &Automaton) {
        match op {
            Op::OpenCapture(id) => {
                let rule = self.nearest_rule();
                self.scopes[rule].captures.push(CaptureSlot {
                    id: *id,
                    items: Vec::new(),
                    words: Vec::new(),
                });
                let slot = self.scopes[rule].captures.len() - 1;
                self.scopes
                    .last_mut()
                    .expect("the scope stack is never empty")
                    .open
                    .push(slot);
            }
            Op::CloseCapture => {
                self.scopes
                    .last_mut()
                    .expect("the scope stack is never empty")
                    .open
                    .pop();
            }
            Op::EnterRule => self.scopes.push(Scope::new(ScopeKind::Rule)),
            Op::EnterBranch => self.scopes.push(Scope::new(ScopeKind::Branch)),
            Op::Exit(program) => {
                let scope = self.scopes.pop().expect("Exit ops balance Enter ops");
                let program = &automaton.programs[*program as usize];
                let rule = self.nearest_rule();
                // A rule's block resolves captures against its own scope; a
                // branch block shares the enclosing rule's captures, because
                // captures are rule-scoped.
                let result = match scope.kind {
                    ScopeKind::Rule => evaluate(program, &scope.emitted, &scope.captures),
                    ScopeKind::Branch => {
                        evaluate(program, &scope.emitted, &self.scopes[rule].captures)
                    }
                };
                self.append(&result);
            }
        }
    }

    /// Appends a matched child's evaluated contribution to the current scope
    /// and to every capture open *in* it.
    fn append(&mut self, items: &[ActionItem]) {
        let rule = self.nearest_rule();
        let top = self.scopes.len() - 1;
        self.scopes[top].emitted.extend_from_slice(items);
        let open = self.scopes[top].open.clone();
        for slot in open {
            self.scopes[rule].captures[slot]
                .items
                .extend_from_slice(items);
        }
    }

    /// Records the consumed word's text into every capture currently open
    /// anywhere on the stack — an outer capture's display text spans the
    /// words of everything its term matched, referenced rules included.
    fn record_word(&mut self, word: &str) {
        let mut rule = 0;
        for index in 0..self.scopes.len() {
            if self.scopes[index].kind == ScopeKind::Rule {
                rule = index;
            }
            let open = self.scopes[index].open.clone();
            for slot in open {
                self.scopes[rule].captures[slot].words.push(word.to_owned());
            }
        }
    }
}

/// Runs an action program against a scope's accumulation.
fn evaluate(
    program: &[ProgramItem],
    emitted: &[ActionItem],
    captures: &[CaptureSlot],
) -> Vec<ActionItem> {
    let mut output = Vec::new();
    for item in program {
        match item {
            ProgramItem::Item(action) => output.push(action.clone()),
            ProgramItem::SpliceAll => output.extend_from_slice(emitted),
            ProgramItem::SpliceCapture(id) => {
                // A capture inside a repetition opens one slot per iteration;
                // the splice concatenates them in spoken order.
                for slot in captures.iter().filter(|slot| slot.id == *id) {
                    output.extend_from_slice(&slot.items);
                }
            }
        }
    }
    output
}

/// One accepting reading of the walked words.
#[derive(Clone, Debug, PartialEq)]
pub struct Accept {
    /// The published rule that matched.
    pub rule: String,
    /// The rule's evaluated action program — splices expanded, ready for
    /// [`crate::output::assembly::assemble`].
    pub actions: Vec<ActionItem>,
    /// The log-facing name: the rule plus its captures' matched words, e.g.
    /// `Watch(two three, north)`.
    pub display: String,
}

/// A hypothesis walk: the matcher's view of the automaton.
///
/// Feed it one recognized word at a time with [`Walk::step`]; between steps,
/// query what the alive set means:
///
/// - [`Walk::accepts`] — the readings that form a complete published command
///   right now, each with its evaluated action program and display name;
/// - [`Walk::can_extend`] — whether any reading could consume another word;
/// - [`Walk::is_ambiguous`] — some reading accepts *and* some can extend,
///   which is the completion-timeout condition;
/// - [`Walk::is_dead`] — no reading survived, i.e. the words match nothing.
#[derive(Clone, Debug)]
pub struct Walk<'a> {
    automaton: &'a Automaton,
    hypotheses: Vec<Hypothesis>,
    warning: Option<String>,
}

impl<'a> Walk<'a> {
    fn new(automaton: &'a Automaton) -> Self {
        Self {
            automaton,
            hypotheses: vec![Hypothesis::root()],
            warning: None,
        }
    }

    /// Consumes one word, replacing the alive set with its successors. A word
    /// no hypothesis can consume empties the set ([`Walk::is_dead`]).
    pub fn step(&mut self, word: &str) {
        let word = word.to_lowercase();
        let Some(&id) = self.automaton.words.get(&word) else {
            self.hypotheses.clear();
            return;
        };

        let mut next: Vec<Hypothesis> = Vec::new();
        for hypothesis in &self.hypotheses {
            for transition in &self.automaton.states[hypothesis.state].transitions {
                if transition.word != id {
                    continue;
                }
                let mut successor = hypothesis.clone();
                successor.state = transition.target;
                for op in &transition.ops {
                    successor.apply(op, self.automaton);
                }
                successor.record_word(&word);
                // Identical readings (same state, same accumulation) collapse
                // — they would stay identical forever.
                if !next.contains(&successor) {
                    next.push(successor);
                }
            }
        }

        if next.len() > MAX_HYPOTHESES {
            self.warning = Some(format!(
                "We were tracking more than {MAX_HYPOTHESES} possible readings of what you said after '{word}', so we stopped following this utterance."
            ));
            next.clear();
        }
        self.hypotheses = next;
    }

    /// Whether no hypothesis survived the walk so far.
    pub fn is_dead(&self) -> bool {
        self.hypotheses.is_empty()
    }

    /// Whether any hypothesis can consume at least one more word.
    pub fn can_extend(&self) -> bool {
        self.hypotheses.iter().any(|hypothesis| {
            !self.automaton.states[hypothesis.state]
                .transitions
                .is_empty()
        })
    }

    /// Whether any hypothesis is accepting, without evaluating anything.
    pub fn has_accept(&self) -> bool {
        self.hypotheses
            .iter()
            .any(|hypothesis| !self.automaton.states[hypothesis.state].accepts.is_empty())
    }

    /// The completion-timeout condition: some hypothesis accepts while any
    /// hypothesis (the same one or another) can still extend.
    pub fn is_ambiguous(&self) -> bool {
        self.has_accept() && self.can_extend()
    }

    /// Every accepting reading of the words walked so far, evaluated.
    /// Readings with the same rule and the same actions collapse to one.
    pub fn accepts(&self) -> Vec<Accept> {
        let mut accepts: Vec<Accept> = Vec::new();
        for hypothesis in &self.hypotheses {
            for entry in &self.automaton.states[hypothesis.state].accepts {
                let mut closed = hypothesis.clone();
                for op in &entry.ops {
                    closed.apply(op, self.automaton);
                }
                debug_assert_eq!(
                    closed.scopes.len(),
                    1,
                    "accept ops close every scope above the root"
                );
                let root = &closed.scopes[0];
                let rule = &self.automaton.rules[entry.rule];
                let actions = evaluate(
                    &self.automaton.programs[rule.program as usize],
                    &root.emitted,
                    &root.captures,
                );
                let accept = Accept {
                    display: display_name(&rule.name, &root.captures),
                    rule: rule.name.clone(),
                    actions,
                };
                if !accepts.contains(&accept) {
                    accepts.push(accept);
                }
            }
        }
        accepts
    }

    /// The warning recorded when [`MAX_HYPOTHESES`] was exceeded and the walk
    /// dropped, if that happened.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// Returns to the root with a single fresh hypothesis — the matcher's
    /// re-sync.
    pub fn reset(&mut self) {
        *self = Self::new(self.automaton);
    }
}

/// `Watch(two three, north)`: the rule's name, plus each capture's matched
/// words in capture order. Slots of one capture (from repetition iterations)
/// merge into a single argument.
fn display_name(name: &str, captures: &[CaptureSlot]) -> String {
    if captures.is_empty() {
        return name.to_owned();
    }
    let mut parts: Vec<(u32, Vec<String>)> = Vec::new();
    for slot in captures {
        match parts.iter_mut().find(|(id, _)| *id == slot.id) {
            Some((_, words)) => words.extend(slot.words.iter().cloned()),
            None => parts.push((slot.id, slot.words.clone())),
        }
    }
    let arguments: Vec<String> = parts
        .into_iter()
        .map(|(_, words)| words.join(" "))
        .collect();
    format!("{}({})", name, arguments.join(", "))
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::time::Duration;

    use rstest::rstest;

    use crate::output::keys;

    use super::super::fixtures;
    use super::*;

    fn compile(source: &str) -> Automaton {
        let grammar = Grammar::parse(source).expect("the grammar should load");
        Automaton::compile(&grammar).unwrap_or_else(|diagnostics| {
            panic!(
                "the grammar should compile:\n{}",
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.render(source))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
    }

    fn walked<'a>(automaton: &'a Automaton, phrase: &str) -> Walk<'a> {
        let mut walk = automaton.walk();
        for word in phrase.split_whitespace() {
            walk.step(word);
        }
        walk
    }

    /// A chord press: `press("leftshift+f1")`.
    fn press(chord: &str) -> ActionItem {
        ActionItem::Press(
            chord
                .split('+')
                .map(|name| keys::from_name(name).expect("a known key"))
                .collect(),
        )
    }

    fn wait_ms(millis: u64) -> ActionItem {
        ActionItem::Wait(Duration::from_millis(millis))
    }

    /// The single accepting reading's evaluated actions.
    fn actions_of(automaton: &Automaton, phrase: &str) -> Vec<ActionItem> {
        let walk = walked(automaton, phrase);
        let accepts = walk.accepts();
        assert_eq!(
            accepts.len(),
            1,
            "{phrase:?} should have exactly one accepting reading, got: {accepts:?}"
        );
        accepts.into_iter().next().unwrap().actions
    }

    #[rstest]
    // A plain sequence.
    #[case::sequence("Go = \"go\" \"now\" { m }", "go now", vec![press("m")])]
    // Both alternates of one rule reach the same block.
    #[case::alternation_first("Map = \"map\" | \"toggle map\" { m }", "map", vec![press("m")])]
    #[case::alternation_second("Map = \"map\" | \"toggle map\" { m }", "toggle map", vec![press("m")])]
    // Inline branch actions bind to the branch they terminate.
    #[case::branch_actions(
        "num = ( \"one\" { f1 } | \"two\" { f2 } )\nSel = num { ..., 9 }",
        "two",
        vec![press("f2"), press("9")]
    )]
    // A blockless rule implicitly propagates its accumulation.
    #[case::implicit_propagation(
        "num = ( \"one\" { f1 } | \"two\" { f2 } )\nSel = num",
        "one",
        vec![press("f1")]
    )]
    // Splices reorder: the block places each capture, not spoken order.
    #[case::splice_order(
        "x = \"x\" { 1 }\ny = \"y\" { 2 }\nR = x:a y:b { b..., a... }",
        "x y",
        vec![press("2"), press("1")]
    )]
    // Multiple children accumulate in spoken order under a bare splice.
    #[case::splice_all_order(
        "num = ( \"one\" { f1 } | \"two\" { f2 } )\nR = num num { ..., 9 }",
        "one two",
        vec![press("f1"), press("f2"), press("9")]
    )]
    // `...` may splice any number of times.
    #[case::multi_splice(
        "sub = \"one\" { f1 }\nR = sub \"go\" { ..., 5, ... }",
        "one go",
        vec![press("f1"), press("5"), press("f1")]
    )]
    // An explicit wait sits exactly where the block puts it.
    #[case::wait_placement(
        "R = \"go\" { 1, wait(20ms), 2 }",
        "go",
        vec![press("1"), wait_ms(20), press("2")]
    )]
    // A capture on an unmatched optional splices as empty.
    #[case::optional_capture_empty(
        "x = \"x\" { 1 }\nR = \"a\" x?:c { c..., m }",
        "a",
        vec![press("m")]
    )]
    #[case::optional_capture_taken(
        "x = \"x\" { 1 }\nR = \"a\" x?:c { c..., m }",
        "a x",
        vec![press("1"), press("m")]
    )]
    // Bounded repetition matches every count in range, and a captured
    // repetition appends per iteration.
    #[case::repeat_zero(
        "R = ( \"a\" { 1 } )[0..2]:c \"end\" { c..., m }",
        "end",
        vec![press("m")]
    )]
    #[case::repeat_one(
        "R = ( \"a\" { 1 } )[0..2]:c \"end\" { c..., m }",
        "a end",
        vec![press("1"), press("m")]
    )]
    #[case::repeat_two(
        "R = ( \"a\" { 1 } )[0..2]:c \"end\" { c..., m }",
        "a a end",
        vec![press("1"), press("1"), press("m")]
    )]
    // A group capture aliases the whole group; words inside contributing
    // nothing stay out of the capture's commands.
    #[case::group_capture(
        "R = (\"team\"? (\"red\" { 1 } | \"blue\" { 2 })):c \"go\" { 9, c... }",
        "team red go",
        vec![press("9"), press("1")]
    )]
    #[case::group_capture_short(
        "R = (\"team\"? (\"red\" { 1 } | \"blue\" { 2 })):c \"go\" { 9, c... }",
        "blue go",
        vec![press("9"), press("2")]
    )]
    // hold/release/release(*) pass through evaluation untouched.
    #[case::hold_release(
        "R = \"grab\" { hold(leftctrl), t, release(leftctrl), release(*) }",
        "grab",
        vec![
            ActionItem::Hold(vec![keys::from_name("leftctrl").unwrap()]),
            press("t"),
            ActionItem::Release(vec![keys::from_name("leftctrl").unwrap()]),
            ActionItem::ReleaseAll,
        ]
    )]
    fn test_walks_evaluate(
        #[case] source: &str,
        #[case] phrase: &str,
        #[case] expected: Vec<ActionItem>,
    ) {
        let automaton = compile(source);
        assert_eq!(actions_of(&automaton, phrase), expected);
    }

    #[rstest]
    // Not enough words yet: extendable, not accepting, not dead.
    #[case::incomplete("go", false, true, false)]
    // A full short command that is also a prefix: the ambiguity condition.
    #[case::ambiguous_prefix("go now", true, true, false)]
    // The longer command: accepting, nothing further.
    #[case::complete("go now please", true, false, false)]
    // An out-of-grammar word kills the set.
    #[case::dead("go banana", false, false, true)]
    fn test_walk_conditions(
        #[case] phrase: &str,
        #[case] accepting: bool,
        #[case] extendable: bool,
        #[case] dead: bool,
    ) {
        let automaton =
            compile("Short = \"go\" \"now\" { 1 }\nLong = \"go\" \"now\" \"please\" { 2 }");
        let walk = walked(&automaton, phrase);
        assert_eq!(walk.has_accept(), accepting, "has_accept for {phrase:?}");
        assert_eq!(walk.can_extend(), extendable, "can_extend for {phrase:?}");
        assert_eq!(walk.is_dead(), dead, "is_dead for {phrase:?}");
        assert_eq!(
            walk.is_ambiguous(),
            accepting && extendable,
            "is_ambiguous for {phrase:?}"
        );
    }

    #[test]
    fn test_reset_returns_to_the_root() {
        let automaton = compile("Go = \"go\" { m }");
        let mut walk = automaton.walk();
        walk.step("banana");
        assert!(walk.is_dead());
        walk.reset();
        walk.step("go");
        assert_eq!(walk.accepts().len(), 1);
    }

    #[test]
    fn test_display_names_carry_captured_words() {
        let automaton = compile(
            "sub = ( \"one\" { f1 } | \"two\" { f2 } )\ndir = ( \"north\" { 1 } | \"north east\" { 2 } )\nWatch = sub:s \"watch\" dir:d { s..., 3, d... }",
        );
        let walk = walked(&automaton, "two watch north east");
        let accepts = walk.accepts();
        assert_eq!(accepts.len(), 1);
        assert_eq!(accepts[0].display, "Watch(two, north east)");
        assert_eq!(accepts[0].rule, "Watch");
    }

    #[test]
    fn test_display_name_without_captures_is_the_rule_name() {
        let automaton = compile("Map = \"map\" { m }");
        assert_eq!(walked(&automaton, "map").accepts()[0].display, "Map");
    }

    #[test]
    fn test_a_captured_repetition_displays_every_iteration() {
        let automaton =
            compile("num = ( \"one\" { f1 } | \"two\" { f2 } )\nSel = num[1..3]:n { n... }");
        let walk = walked(&automaton, "one two");
        let accepts = walk.accepts();
        assert_eq!(accepts[0].display, "Sel(one two)");
        assert_eq!(accepts[0].actions, vec![press("f1"), press("f2")]);
    }

    #[test]
    fn test_the_state_cap_is_a_diagnostic_naming_the_largest_rule() {
        // Ten alternates repeated 30,000 times can't fit under the cap; the
        // load must fail with a diagnostic, not hang or exhaust memory.
        let source = "Big = (\"a\"|\"b\"|\"c\"|\"d\"|\"e\"|\"f\"|\"g\"|\"h\"|\"i\"|\"j\")[30000] { m }\nMap = \"map\" { m }";
        let grammar = Grammar::parse(source).expect("the grammar itself is legal");
        let diagnostics =
            Automaton::compile(&grammar).expect_err("the automaton should overflow the cap");
        assert_eq!(diagnostics.len(), 1);
        let message = &diagnostics[0].message;
        assert!(message.contains("'Big'"), "got: {message}");
        assert!(
            message.contains(&MAX_AUTOMATON_STATES.to_string()),
            "got: {message}"
        );
    }

    #[test]
    fn test_the_hypothesis_cap_drops_the_walk_with_a_warning() {
        // Each word doubles the alive set (two branches, same word, different
        // accumulations, constant output so the load-time duplicate check has
        // nothing to reject); ten words would need 1024 hypotheses.
        let group = "(\"a\" { 1 } | \"a\" { 2 })";
        let source = format!("R = {} {{ m }}", [group; 10].join(" "));
        let automaton = compile(&source);

        let mut walk = automaton.walk();
        for _ in 0..10 {
            walk.step("a");
        }
        assert!(walk.is_dead(), "the walk should have been dropped");
        let warning = walk.warning().expect("the drop should carry a warning");
        assert!(
            warning.contains(&MAX_HYPOTHESES.to_string()),
            "got: {warning}"
        );
    }

    // -----------------------------------------------------------------------
    // The canonical Arma profile, walked end to end.
    // -----------------------------------------------------------------------

    fn arma() -> &'static Automaton {
        static ARMA: OnceLock<Automaton> = OnceLock::new();
        ARMA.get_or_init(|| {
            let source = fixtures::arma_source();
            let grammar = Grammar::parse(&source).expect("the canonical grammar should load");
            Automaton::compile(&grammar).unwrap_or_else(|diagnostics| {
                panic!(
                    "profiles/arma.yaml should compile with zero diagnostics:\n{}",
                    diagnostics
                        .iter()
                        .map(|diagnostic| diagnostic.render(&source))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
        })
    }

    #[test]
    fn test_the_canonical_arma_grammar_compiles_cleanly() {
        arma();
    }

    #[rstest]
    #[case::fall_back(
        "two and three fall back",
        "ReturnToFormation",
        vec![press("f2"), press("f3"), press("1"), press("1")]
    )]
    #[case::assign(
        "all assign to team red",
        "Assign",
        vec![press("grave"), press("9"), press("1")]
    )]
    #[case::formation(
        "team red form wedge",
        "Formation",
        vec![press("leftshift+f1"), press("8"), press("3")]
    )]
    #[case::watch(
        "two watch north east",
        "Watch",
        vec![press("f2"), press("3"), press("8"), wait_ms(20), press("2")]
    )]
    #[case::select("two", "Select", vec![press("f2")])]
    #[case::chained_selection(
        "one two and three advance",
        "Advance",
        vec![press("f1"), press("f2"), press("f3"), press("1"), press("2")]
    )]
    fn test_arma_walks(
        #[case] phrase: &str,
        #[case] rule: &str,
        #[case] expected: Vec<ActionItem>,
    ) {
        let walk = walked(arma(), phrase);
        let accepts = walk.accepts();
        assert_eq!(
            accepts.len(),
            1,
            "{phrase:?} should have exactly one accepting reading, got: {accepts:?}"
        );
        assert_eq!(accepts[0].rule, rule, "for {phrase:?}");
        assert_eq!(accepts[0].actions, expected, "for {phrase:?}");
    }

    #[test]
    fn test_a_bare_subject_is_accepting_and_extendable() {
        // "two" is a complete Select and the prefix of every subject-led
        // command: the ambiguity condition must hold so the matcher engages
        // the completion timeout.
        let walk = walked(arma(), "two");
        assert!(walk.has_accept(), "Select should accept");
        assert!(walk.can_extend(), "every subject-led command should extend");
        assert!(walk.is_ambiguous());
    }

    #[test]
    fn test_an_out_of_grammar_word_kills_the_set() {
        let walk = walked(arma(), "two banana");
        assert!(walk.is_dead());
        assert!(walk.warning().is_none(), "dying is not an overflow");
    }

    #[test]
    fn test_arma_watch_displays_its_captures() {
        let walk = walked(arma(), "two and three watch north");
        let accepts = walk.accepts();
        assert_eq!(accepts.len(), 1);
        assert_eq!(accepts[0].display, "Watch(two and three, north)");
    }

    #[test]
    fn test_arma_rule_sizes_are_reported_per_published_rule() {
        let sizes = arma().rule_sizes();
        let published = Grammar::parse(&fixtures::arma_source())
            .expect("the canonical grammar should load")
            .published()
            .count();
        assert_eq!(sizes.len(), published);
        assert!(
            sizes.iter().all(|(_, count)| *count > 0),
            "every published rule contributes states"
        );
    }
}
