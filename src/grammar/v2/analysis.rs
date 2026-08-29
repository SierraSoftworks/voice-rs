//! Load-time static analysis over the parsed rule list.
//!
//! Everything here reports [`Diagnostic`]s rather than aborting, so one load
//! surfaces every problem a grammar has. Errors are things the grammar cannot
//! run with (undefined rules, cycles, unknown keys); lints are legal-but-
//! probably-wrong constructions the grammar can still load with. Automaton-
//! level checks (duplicate spoken phrases, state caps) belong to the compiler,
//! not here — this pass only needs the rule graph.

use std::collections::{HashMap, HashSet};

use crate::output::keys;

use super::{
    ast::{ActionBlock, ActionKind, Alternation, Atom, Capture, Chord, Rule, Span, Term},
    diagnostic::Diagnostic,
};

/// Runs every check, returning errors and lints together (sorted by source
/// position; [`DiagnosticKind::is_error`] tells them apart).
///
/// [`DiagnosticKind::is_error`]: super::diagnostic::DiagnosticKind::is_error
pub(super) fn analyze(rules: &[Rule]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    check_duplicate_rules(rules, &mut diagnostics);
    check_references(rules, &mut diagnostics);
    check_cycles(rules, &mut diagnostics);
    check_published_rules_need_words(rules, &mut diagnostics);
    check_repetition_bounds(rules, &mut diagnostics);
    for rule in rules {
        check_captures(rule, &mut diagnostics);
        check_action_blocks(rule, &mut diagnostics);
    }
    check_unreferenced_rules(rules, &mut diagnostics);

    diagnostics.sort_by_key(|diagnostic| (diagnostic.span.start, diagnostic.span.end));
    diagnostics
}

/// Calls `visit` on every term of the alternation, including terms nested in
/// groups, in source order.
fn walk_terms<'a>(alternation: &'a Alternation, visit: &mut impl FnMut(&'a Term)) {
    for branch in &alternation.branches {
        for term in &branch.terms {
            visit(term);
            if let Atom::Group(inner) = &term.atom {
                walk_terms(inner, visit);
            }
        }
    }
}

/// Calls `visit` on every action block of the rule: the rule's own trailing
/// block and every inline block nested inside its groups.
fn walk_blocks<'a>(rule: &'a Rule, visit: &mut impl FnMut(&'a ActionBlock)) {
    fn walk<'a>(alternation: &'a Alternation, visit: &mut impl FnMut(&'a ActionBlock)) {
        for branch in &alternation.branches {
            if let Some(block) = &branch.actions {
                visit(block);
            }
            for term in &branch.terms {
                if let Atom::Group(inner) = &term.atom {
                    walk(inner, visit);
                }
            }
        }
    }

    walk(&rule.pattern, visit);
    if let Some(block) = &rule.actions {
        visit(block);
    }
}

/// A `strsim`-ranked "did you mean" over an arbitrary candidate list, using
/// the same plausibility rules as [`keys::suggest`] so rule-name and key-name
/// hints feel the same.
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    const MAX_SUGGESTION_DISTANCE: usize = 2;

    let first = name.chars().next();
    candidates
        .filter_map(|candidate| {
            let distance = strsim::levenshtein(name, candidate);
            let plausible =
                distance <= MAX_SUGGESTION_DISTANCE && distance < name.len().min(candidate.len());
            plausible.then_some((distance, candidate))
        })
        .min_by_key(|(distance, candidate)| {
            (
                *distance,
                candidate.chars().next() != first,
                candidate.len().abs_diff(name.len()),
                *candidate,
            )
        })
        .map(|(_, candidate)| candidate)
}

fn check_duplicate_rules(rules: &[Rule], diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for rule in rules {
        if seen.insert(rule.name.as_str(), ()).is_some() {
            diagnostics.push(
                Diagnostic::analysis(
                    format!(
                        "You've defined the rule '{}' twice — this definition conflicts with an earlier one.",
                        rule.name
                    ),
                    rule.name_span,
                )
                .with_help(
                    "Every rule name must be unique. Merge the two definitions into one (branches join with '|'), or rename one of them.",
                ),
            );
        }
    }
}

fn check_references(rules: &[Rule], diagnostics: &mut Vec<Diagnostic>) {
    let defined: HashSet<&str> = rules.iter().map(|rule| rule.name.as_str()).collect();

    for rule in rules {
        walk_terms(&rule.pattern, &mut |term| {
            let Atom::Ref(name) = &term.atom else { return };
            if defined.contains(name.as_str()) {
                return;
            }

            let hint = suggest(name, rules.iter().map(|rule| rule.name.as_str()))
                .map(|suggestion| format!(" Did you mean '{suggestion}'?"))
                .unwrap_or_default();
            diagnostics.push(
                Diagnostic::analysis(
                    format!(
                        "You're referring to a rule called '{name}', but no rule with that name is defined.{hint}"
                    ),
                    term.atom_span,
                )
                .with_help(
                    "Define it as its own rule ('name = ...'), or correct the reference — rule names are case-sensitive.",
                ),
            );
        });
    }
}

fn check_cycles(rules: &[Rule], diagnostics: &mut Vec<Diagnostic>) {
    let index: HashMap<&str, usize> = rules
        .iter()
        .enumerate()
        .map(|(position, rule)| (rule.name.as_str(), position))
        .collect();

    // One edge per referenced rule, keeping the first reference's span so the
    // report can point somewhere concrete.
    let edges: Vec<Vec<(usize, Span)>> = rules
        .iter()
        .map(|rule| {
            let mut targets: Vec<(usize, Span)> = Vec::new();
            walk_terms(&rule.pattern, &mut |term| {
                if let Atom::Ref(name) = &term.atom
                    && let Some(&target) = index.get(name.as_str())
                    && !targets.iter().any(|(existing, _)| *existing == target)
                {
                    targets.push((target, term.atom_span));
                }
            });
            targets
        })
        .collect();

    // Iterative DFS with an explicit path so a back edge can name the whole
    // cycle. Each node is visited once, so each cycle reports once.
    const UNVISITED: u8 = 0;
    const ON_PATH: u8 = 1;
    const DONE: u8 = 2;
    let mut state = vec![UNVISITED; rules.len()];

    fn visit(
        node: usize,
        rules: &[Rule],
        edges: &[Vec<(usize, Span)>],
        state: &mut [u8],
        path: &mut Vec<usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        state[node] = ON_PATH;
        path.push(node);

        for &(target, span) in &edges[node] {
            match state[target] {
                ON_PATH => {
                    let start = path
                        .iter()
                        .position(|&member| member == target)
                        .expect("a node on the path is in the path");
                    let cycle = path[start..]
                        .iter()
                        .chain(std::iter::once(&target))
                        .map(|&member| rules[member].name.as_str())
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    diagnostics.push(
                        Diagnostic::analysis(
                            format!(
                                "The rule '{}' eventually refers back to itself ({cycle}) — grammars can't recurse.",
                                rules[target].name
                            ),
                            span,
                        )
                        .with_help(
                            "Repetition is how a grammar repeats: give the reference a bound instead, e.g. squad_number (\"and\"? squad_number)[0..9].",
                        ),
                    );
                }
                UNVISITED => visit(target, rules, edges, state, path, diagnostics),
                _ => {}
            }
        }

        path.pop();
        state[node] = DONE;
    }

    let mut path = Vec::new();
    for node in 0..rules.len() {
        if state[node] == UNVISITED {
            visit(node, rules, &edges, &mut state, &mut path, diagnostics);
        }
    }
}

/// Computes which rules can match the empty word sequence, by least fixpoint
/// so that reference cycles (reported separately) cannot loop this check.
fn nullable_rules(rules: &[Rule]) -> HashMap<&str, bool> {
    fn term_nullable(term: &Term, nullable: &HashMap<&str, bool>) -> bool {
        if term.repeat.is_some_and(|repeat| repeat.min == 0) {
            return true;
        }
        match &term.atom {
            Atom::Literal(words) => words.is_empty(),
            Atom::Ref(name) => nullable.get(name.as_str()).copied().unwrap_or(false),
            Atom::Group(inner) => alternation_nullable(inner, nullable),
        }
    }

    fn alternation_nullable(alternation: &Alternation, nullable: &HashMap<&str, bool>) -> bool {
        alternation.branches.iter().any(|branch| {
            branch
                .terms
                .iter()
                .all(|term| term_nullable(term, nullable))
        })
    }

    let mut nullable: HashMap<&str, bool> = rules
        .iter()
        .map(|rule| (rule.name.as_str(), false))
        .collect();
    loop {
        let mut changed = false;
        for rule in rules {
            if !nullable[rule.name.as_str()] && alternation_nullable(&rule.pattern, &nullable) {
                nullable.insert(rule.name.as_str(), true);
                changed = true;
            }
        }
        if !changed {
            return nullable;
        }
    }
}

fn check_published_rules_need_words(rules: &[Rule], diagnostics: &mut Vec<Diagnostic>) {
    let nullable = nullable_rules(rules);
    for rule in rules {
        if rule.published() && nullable[rule.name.as_str()] {
            diagnostics.push(
                Diagnostic::analysis(
                    format!(
                        "'{}' can match without a single word being spoken — a published command must require at least one word.",
                        rule.name
                    ),
                    rule.name_span,
                )
                .with_help(
                    "Make sure every branch needs at least one word: drop a '?', raise a '[0..n]' minimum to 1, or add a literal.",
                ),
            );
        }
    }
}

fn check_repetition_bounds(rules: &[Rule], diagnostics: &mut Vec<Diagnostic>) {
    for rule in rules {
        walk_terms(&rule.pattern, &mut |term| {
            let Some(repeat) = term.repeat else { return };
            if repeat.max == 0 {
                diagnostics.push(
                    Diagnostic::analysis(
                        "This repetition allows at most zero occurrences, so it can never match anything.",
                        repeat.span,
                    )
                    .with_help(
                        "Give the repetition a positive upper bound, e.g. [0..1] or [1..3] — or remove the term entirely.",
                    ),
                );
            } else if repeat.min > repeat.max {
                diagnostics.push(
                    Diagnostic::analysis(
                        format!(
                            "This repetition's minimum ({}) is larger than its maximum ({}), so it can never match.",
                            repeat.min, repeat.max
                        ),
                        repeat.span,
                    )
                    .with_help("Write the smaller bound first, e.g. [1..3]."),
                );
            }
        });
    }
}

fn check_captures(rule: &Rule, diagnostics: &mut Vec<Diagnostic>) {
    let mut captures: Vec<&Capture> = Vec::new();
    walk_terms(&rule.pattern, &mut |term| {
        if let Some(capture) = &term.capture {
            captures.push(capture);
        }
    });

    let mut seen: HashSet<&str> = HashSet::new();
    for capture in &captures {
        if !seen.insert(capture.name.as_str()) {
            diagnostics.push(
                Diagnostic::analysis(
                    format!(
                        "You've already captured ':{}' earlier in '{}' — each capture in a rule needs its own name.",
                        capture.name, rule.name
                    ),
                    capture.span,
                )
                .with_help(
                    "Rename one of the captures; each 'name...' splice follows whichever name you pick.",
                ),
            );
        }
    }

    for block in blocks_of(rule) {
        for action in &block.actions {
            let ActionKind::SpliceCapture(name) = &action.kind else {
                continue;
            };
            if seen.contains(name.as_str()) {
                continue;
            }

            let hint = suggest(name, seen.iter().copied())
                .map(|suggestion| format!(" Did you mean '{suggestion}...'?"))
                .unwrap_or_default();
            diagnostics.push(
                Diagnostic::analysis(
                    format!(
                        "You're splicing '{name}...', but nothing in '{}' is captured as ':{name}'.{hint}",
                        rule.name
                    ),
                    action.span,
                )
                .with_help(
                    "Attach the capture to a term first (e.g. direction:dir), then splice it with 'dir...'.",
                ),
            );
        }
    }
}

/// Collects a rule's action blocks into a vector, for checks that need to
/// iterate them more than once.
fn blocks_of(rule: &Rule) -> Vec<&ActionBlock> {
    let mut blocks = Vec::new();
    walk_blocks(rule, &mut |block| blocks.push(block));
    blocks
}

fn check_action_blocks(rule: &Rule, diagnostics: &mut Vec<Diagnostic>) {
    for block in blocks_of(rule) {
        let mut splice_all: Option<Span> = None;
        let mut splices_captures = false;
        let mut held: Vec<(&Chord, Span)> = Vec::new();

        for action in &block.actions {
            match &action.kind {
                ActionKind::Press(chord) => check_chord(chord, diagnostics),
                ActionKind::Hold(chord) => {
                    check_chord(chord, diagnostics);
                    held.push((chord, action.span));
                }
                ActionKind::Release(chord) => {
                    check_chord(chord, diagnostics);
                    let released = chord.to_string();
                    if let Some(position) = held
                        .iter()
                        .position(|(held, _)| held.to_string() == released)
                    {
                        held.remove(position);
                    }
                }
                ActionKind::ReleaseAll => held.clear(),
                ActionKind::SpliceAll => splice_all = splice_all.or(Some(action.span)),
                ActionKind::SpliceCapture(_) => splices_captures = true,
                ActionKind::Wait(_) => {}
            }
        }

        if let Some(span) = splice_all
            && splices_captures
        {
            diagnostics.push(
                Diagnostic::lint(
                    "This block splices everything with '...' and also splices captures by name — the captured presses will play twice.",
                    span,
                )
                .with_help(
                    "Naming a capture doesn't remove it from '...'. Keep either the bare '...' or the named splices, not both.",
                ),
            );
        }

        for (chord, span) in held {
            diagnostics.push(
                Diagnostic::lint(
                    format!(
                        "You hold '{chord}' here but never release it — the keys stay down after the command finishes."
                    ),
                    span,
                )
                .with_help(
                    "Add a matching 'release(...)' (or 'release(*)') later in the block, unless keeping the keys held past the command is deliberate.",
                ),
            );
        }
    }
}

fn check_chord(chord: &Chord, diagnostics: &mut Vec<Diagnostic>) {
    for segment in &chord.segments {
        if segment.key().is_some() {
            continue;
        }

        let hint = keys::suggest(&segment.name)
            .map(|suggestion| format!(" Did you mean '{suggestion}'?"))
            .unwrap_or_default();
        diagnostics.push(
            Diagnostic::analysis(
                format!("We don't recognize '{}' as a key name.{hint}", segment.name),
                segment.span,
            )
            .with_help(
                "Key names are the lowercase evdev key names with their 'KEY_' prefix removed, e.g. 'a', '4', 'f5', 'space', 'enter', 'leftctrl' or 'kp1'. The key reference page in the documentation lists every name we accept.",
            ),
        );
    }
}

fn check_unreferenced_rules(rules: &[Rule], diagnostics: &mut Vec<Diagnostic>) {
    let mut referenced: HashSet<&str> = HashSet::new();
    for rule in rules {
        walk_terms(&rule.pattern, &mut |term| {
            if let Atom::Ref(name) = &term.atom {
                referenced.insert(name.as_str());
            }
        });
    }

    for rule in rules {
        if !rule.published() && !referenced.contains(rule.name.as_str()) {
            diagnostics.push(
                Diagnostic::lint(
                    format!(
                        "Nothing refers to the private rule '{}', so it can never be spoken.",
                        rule.name
                    ),
                    rule.name_span,
                )
                .with_help(
                    "Reference it from another rule, publish it by giving it a TitleCase name, or remove it.",
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::{Grammar, diagnostic::DiagnosticKind};

    /// The message and help of every error, joined so assertions can target
    /// either the finding or its advice.
    fn errors(source: &str) -> Vec<String> {
        let diagnostics = Grammar::parse(source).expect_err("the grammar should be rejected");
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.kind.is_error())
            .map(|diagnostic| {
                format!(
                    "{} {}",
                    diagnostic.message,
                    diagnostic.help.as_deref().unwrap_or_default()
                )
            })
            .collect()
    }

    fn lints(source: &str) -> Vec<String> {
        let grammar = Grammar::parse(source)
            .unwrap_or_else(|diagnostics| panic!("the grammar should load: {diagnostics:?}"));
        grammar
            .lints()
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect()
    }

    #[rstest]
    // Undefined references, with and without a plausible suggestion.
    #[case::undefined_ref(
        "Advance = ghost \"advance\" { 1 }",
        "referring to a rule called 'ghost'"
    )]
    #[case::undefined_ref_hint(
        "Advance = subjcet \"advance\" { 1 }\nsubject = \"all\" { grave }",
        "Did you mean 'subject'?"
    )]
    // Duplicate definitions.
    #[case::duplicate_rule(
        "Map = \"map\" { m }\nMap = \"chart\" { m }",
        "defined the rule 'Map' twice"
    )]
    // Cycles, direct and indirect.
    #[case::self_cycle("A = \"go\" A { m }", "refers back to itself (A -> A)")]
    #[case::indirect_cycle(
        "a = \"x\" b\nb = \"y\" a\nUse = a { m }",
        "refers back to itself (a -> b -> a)"
    )]
    #[case::cycle_advice("A = \"go\" A { m }", "give the reference a bound")]
    // Published rules that can match silence.
    #[case::nullable_published("Silent = \"word\"? { m }", "without a single word")]
    #[case::nullable_via_ref(
        "opt = \"maybe\"?\nSilent = opt { m }",
        "'Silent' can match without a single word"
    )]
    // Repetition bounds.
    #[case::zero_repeat("R = \"a\" \"b\"[0..0] { m }", "at most zero occurrences")]
    #[case::zero_exact("R = \"a\" \"b\"[0] { m }", "at most zero occurrences")]
    #[case::inverted_bounds("R = \"a\"[3..1] { m }", "minimum (3) is larger than its maximum (1)")]
    // Keys.
    #[case::unknown_key("R = \"x\" { notakey }", "don't recognize 'notakey' as a key name")]
    #[case::key_hint("R = \"x\" { leftctlr }", "Did you mean 'leftctrl'?")]
    #[case::unknown_key_in_chord("R = \"x\" { shift+f1 }", "don't recognize 'shift'")]
    #[case::unknown_key_in_hold("R = \"x\" { hold(notakey), release(*) }", "'notakey'")]
    // Captures.
    #[case::unknown_splice("R = \"x\" { sub... }", "nothing in 'R' is captured as ':sub'")]
    #[case::splice_hint(
        "R = direction:dir \"x\" { dri... }\ndirection = \"north\" { 1 }",
        "Did you mean 'dir...'?"
    )]
    #[case::capture_collision(
        "R = a:x b:x { x... }\na = \"a\" { 1 }\nb = \"b\" { 2 }",
        "already captured ':x'"
    )]
    fn test_errors(#[case] source: &str, #[case] expected: &str) {
        let messages = errors(source);
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "expected an error containing {expected:?}, got: {messages:?}"
        );
    }

    #[rstest]
    #[case::unreferenced_private(
        "Map = \"map\" { m }\ndead = \"end\" { m }",
        "Nothing refers to the private rule 'dead'"
    )]
    #[case::double_splice(
        "R = sub:s \"go\" { ..., s... }\nsub = \"one\" { f1 }",
        "captured presses will play twice"
    )]
    #[case::unreleased_hold(
        "R = \"x\" { hold(leftctrl), t }",
        "hold 'leftctrl' here but never release it"
    )]
    fn test_lints(#[case] source: &str, #[case] expected: &str) {
        let messages = lints(source);
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "expected a lint containing {expected:?}, got: {messages:?}"
        );
    }

    #[rstest]
    // A hold discharged by its release, by release(*), and one deliberately
    // spanning rules is not flagged beyond its own block.
    #[case::hold_released("R = \"x\" { hold(leftctrl), t, release(leftctrl) }")]
    #[case::hold_released_all("R = \"x\" { hold(leftctrl), hold(a), release(*) }")]
    // Multiple bare splices are fine, and named splices without a bare one too.
    #[case::multiple_splice_all("R = a \"x\" { ..., 1, ... }\na = \"a\" { 2 }")]
    #[case::named_splices_only("R = a:s \"x\" { s..., 1, s... }\na = \"a\" { 2 }")]
    // Explicit bounds may exceed MAX_REPETITION; published rules are roots.
    #[case::wide_bounds("R = \"a\" \"b\"[0..20] { m }")]
    #[case::published_roots("Map = \"map\" { m }\nOther = \"chart\" { i }")]
    // A private nullable rule is fine when its published user isn't nullable.
    #[case::nullable_private("opt = \"the\"?\nDeploy = opt \"sentry\" { 4 }")]
    fn test_clean_grammars(#[case] source: &str) {
        let grammar = Grammar::parse(source)
            .unwrap_or_else(|diagnostics| panic!("the grammar should load: {diagnostics:?}"));
        assert!(
            grammar.lints().is_empty(),
            "unexpected lints: {:?}",
            grammar.lints()
        );
    }

    #[test]
    fn test_lints_do_not_fail_the_load_but_are_exposed() {
        let grammar = Grammar::parse("Map = \"map\" { m }\ndead = \"end\" { m }")
            .expect("lints alone must not fail the load");
        assert_eq!(grammar.lints().len(), 1);
        assert_eq!(grammar.lints()[0].kind, DiagnosticKind::Lint);
    }

    #[test]
    fn test_all_problems_report_in_one_pass() {
        // Three distinct problems: an unknown key, an undefined rule, and a
        // duplicate definition — one load reports all three.
        let source = "\
Map = \"map\" { notakey }
Advance = ghost \"advance\" { 1 }
Map = \"chart\" { m }";
        let diagnostics = Grammar::parse(source).expect_err("the grammar should be rejected");
        let messages: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();

        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.kind.is_error())
                .count(),
            3,
            "all three problems should be reported: {messages:?}"
        );
        assert!(messages.iter().any(|m| m.contains("notakey")));
        assert!(messages.iter().any(|m| m.contains("ghost")));
        assert!(messages.iter().any(|m| m.contains("twice")));
    }
}
