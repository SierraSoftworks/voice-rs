//! The chumsky parser: spanned token stream → rule list.
//!
//! Two properties shape the grammar here:
//!
//! - **Rules have no terminator.** A rule ends where the next `name =` begins,
//!   so a rule reference is "a word *not* followed by `=`" — the lookahead is
//!   what lets `Select = subject` end before `ReturnToFormation = ...` starts.
//! - **Where an action block binds depends on where it sits.** Inside a
//!   parenthesized group a `{ ... }` terminates the branch it follows; at rule
//!   level a trailing block belongs to the whole rule. Group branches and
//!   top-level branches are therefore parsed by different (but term-sharing)
//!   parsers.
//!
//! Errors accumulate: a broken rule is skipped up to the next `name =` and
//! parsing continues, so one load reports every problem.

use chumsky::{
    input::{Input, Stream, ValueInput},
    prelude::*,
};

use super::{
    ast::{
        Action, ActionBlock, ActionKind, Alternation, Atom, Branch, Capture, Chord, ChordSegment,
        MAX_REPETITION, Repeat, Rule, Span, Term,
    },
    diagnostic::{Diagnostic, from_rich},
    lexer,
    token::Token,
};

type ParseError<'tokens> = Rich<'tokens, Token, SimpleSpan<usize>>;

fn span(value: SimpleSpan<usize>) -> Span {
    Span::new(value.start, value.end)
}

/// Lexes and parses a grammar source, accumulating diagnostics across both
/// stages. The rule list is present whenever the parser could produce one —
/// even alongside errors, thanks to recovery — but callers must treat any
/// error diagnostic as a failed parse.
pub(super) fn parse(source: &str) -> (Option<Vec<Rule>>, Vec<Diagnostic>) {
    let (tokens, lexical_errors) = lexer::lexer().parse(source).into_output_errors();
    let mut diagnostics: Vec<Diagnostic> = lexical_errors.iter().map(from_rich).collect();

    let Some(tokens) = tokens else {
        return (None, diagnostics);
    };

    let end: SimpleSpan<usize> = (source.len()..source.len()).into();
    let stream = Stream::from_iter(tokens).map(end, |(token, span)| (token, span));
    let (rules, parse_errors) = grammar_parser().parse(stream).into_output_errors();
    diagnostics.extend(parse_errors.iter().map(from_rich));

    (rules, diagnostics)
}

fn grammar_parser<'tokens, I>()
-> impl Parser<'tokens, I, Vec<Rule>, extra::Err<ParseError<'tokens>>>
where
    I: ValueInput<'tokens, Token = Token, Span = SimpleSpan<usize>>,
{
    let word = select! { Token::Word(word) => word };

    // ----- action blocks -----

    let chord_segment = word.map_with(|name, extra| ChordSegment {
        name,
        span: span(extra.span()),
    });

    // `wait`, `hold` and `release` are reserved in action position: they can
    // never start a chord, so `wait` missing its `(...)` is a syntax error
    // rather than a surprising unknown-key error later.
    let chord_start = select! {
        Token::Word(word) if !matches!(word.as_str(), "wait" | "hold" | "release") => word,
    }
    .map_with(|name, extra| ChordSegment {
        name,
        span: span(extra.span()),
    });

    let chord = chord_start
        .then(
            just(Token::Plus)
                .ignore_then(chord_segment)
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map_with(|(first, rest), extra| {
            let mut segments = vec![first];
            segments.extend(rest);
            Chord {
                segments,
                span: span(extra.span()),
            }
        });

    let keyword = |name: &'static str| {
        select! { Token::Word(word) if word == name => () }
    };

    // A duration is one or more words joined back together (`20ms`, or the
    // spaced `1m 30s` humantime also accepts), parsed by humantime itself.
    let duration = word
        .repeated()
        .at_least(1)
        .collect::<Vec<String>>()
        .try_map(|parts, value| {
            let text = parts.concat();
            humantime::parse_duration(&text).map_err(|_| {
                Rich::custom(
                    value,
                    format!(
                        "We can't read '{text}' as a duration — write durations like '20ms', '1s' or '1m30s'."
                    ),
                )
            })
        });

    let wait = keyword("wait")
        .ignore_then(duration.delimited_by(just(Token::LParen), just(Token::RParen)))
        .map(ActionKind::Wait);

    let hold = keyword("hold")
        .ignore_then(
            chord
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(ActionKind::Hold);

    let release = keyword("release")
        .ignore_then(
            choice((just(Token::Star).to(None), chord.clone().map(Some)))
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|chord| match chord {
            Some(chord) => ActionKind::Release(chord),
            None => ActionKind::ReleaseAll,
        });

    let splice_all = just(Token::Ellipsis).to(ActionKind::SpliceAll);
    let splice_capture = word
        .then_ignore(just(Token::Ellipsis))
        .map(ActionKind::SpliceCapture);

    let action = choice((
        wait,
        hold,
        release,
        splice_all,
        splice_capture,
        chord.map(ActionKind::Press),
    ))
    .map_with(|kind, extra| Action {
        kind,
        span: span(extra.span()),
    });

    let action_block = action
        .separated_by(just(Token::Comma))
        .at_least(1)
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
        .map_with(|actions, extra| ActionBlock {
            actions,
            span: span(extra.span()),
        });
    let rule_action_block = action_block.clone();

    // ----- expressions -----

    let number = word.try_map(|word, value| {
        word.parse::<usize>().map_err(|_| {
            Rich::custom(
                value,
                format!(
                    "We can't read '{word}' as a repetition count — bounds are written like [2], [1..3], [..4] or [2..]."
                ),
            )
        })
    });

    let bounds = choice((
        just(Token::DotDot).ignore_then(number).map(|max| (0, max)),
        number
            .then(just(Token::DotDot).ignore_then(number.or_not()).or_not())
            .map(|(min, bound)| match bound {
                None => (min, min),
                Some(None) => (min, MAX_REPETITION),
                Some(Some(max)) => (min, max),
            }),
    ));

    let repeat = choice((
        just(Token::Question).to((0, 1)),
        just(Token::Star).to((0, MAX_REPETITION)),
        just(Token::Plus).to((1, MAX_REPETITION)),
        bounds.delimited_by(just(Token::LBracket), just(Token::RBracket)),
    ))
    .map_with(|(min, max), extra| Repeat {
        min,
        max,
        span: span(extra.span()),
    });

    let capture = just(Token::Colon).ignore_then(word.map_with(|name, extra| Capture {
        name,
        span: span(extra.span()),
    }));

    let literal = select! { Token::Literal(words) => Atom::Literal(words) };

    // The lookahead that ends a rule: a word followed by `=` is the next
    // rule's name, never a reference.
    let rule_ref = word.then_ignore(just(Token::Eq).not()).map(Atom::Ref);

    let term = recursive(|term| {
        let branch = term
            .repeated()
            .at_least(1)
            .collect::<Vec<Term>>()
            .then(action_block.or_not())
            .map_with(|(terms, actions), extra| Branch {
                terms,
                actions,
                span: span(extra.span()),
            });

        let group = branch
            .separated_by(just(Token::Pipe))
            .at_least(1)
            .collect::<Vec<_>>()
            .map_with(|branches, extra| Alternation {
                branches,
                span: span(extra.span()),
            })
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map(Atom::Group);

        let atom =
            choice((literal, group, rule_ref)).map_with(|atom, extra| (atom, span(extra.span())));

        atom.then(repeat.or_not()).then(capture.or_not()).map_with(
            |(((atom, atom_span), repeat), capture), extra| Term {
                atom,
                atom_span,
                repeat,
                capture,
                span: span(extra.span()),
            },
        )
    });

    // ----- rules -----

    let top_branch = term
        .repeated()
        .at_least(1)
        .collect::<Vec<Term>>()
        .map_with(|terms, extra| Branch {
            terms,
            actions: None,
            span: span(extra.span()),
        });

    let top_alternation = top_branch
        .separated_by(just(Token::Pipe))
        .at_least(1)
        .collect::<Vec<_>>()
        .map_with(|branches, extra| Alternation {
            branches,
            span: span(extra.span()),
        });

    let rule = word
        .map_with(|name, extra| (name, span(extra.span())))
        .then_ignore(just(Token::Eq))
        .then(top_alternation)
        .then(rule_action_block.or_not())
        .map_with(|(((name, name_span), pattern), actions), extra| Rule {
            name,
            name_span,
            pattern,
            actions,
            span: span(extra.span()),
        });

    // Recovery: a broken rule consumes one token (its own start, however
    // mangled) and then skips to the next `name =`, so every rule after it
    // still parses and reports its own problems.
    let rule_start = select! { Token::Word(_) => () }
        .then(just(Token::Eq))
        .ignored();
    let skip_to_next_rule = any()
        .then(any().and_is(rule_start.not()).repeated())
        .to(None);

    rule.map(Some)
        .recover_with(via_parser(skip_to_next_rule))
        .repeated()
        .collect::<Vec<Option<Rule>>>()
        .map(|rules| rules.into_iter().flatten().collect())
        .then_ignore(end())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use rstest::rstest;

    /// Parses a source that must be syntactically clean.
    fn parse_ok(source: &str) -> Vec<Rule> {
        let (rules, diagnostics) = parse(source);
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics for {source:?}:\n{}",
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.render(source))
                .collect::<Vec<_>>()
                .join("\n")
        );
        rules.expect("the grammar should parse")
    }

    /// Parses a single rule and returns it.
    fn parse_rule(source: &str) -> Rule {
        let mut rules = parse_ok(source);
        assert_eq!(rules.len(), 1, "expected exactly one rule in {source:?}");
        rules.remove(0)
    }

    fn parse_errors(source: &str) -> Vec<Diagnostic> {
        let (_, diagnostics) = parse(source);
        assert!(
            !diagnostics.is_empty(),
            "expected diagnostics for {source:?}"
        );
        diagnostics
    }

    /// The single term of a single-branch rule.
    fn only_term(rule: &Rule) -> &Term {
        assert_eq!(rule.pattern.branches.len(), 1);
        assert_eq!(rule.pattern.branches[0].terms.len(), 1);
        &rule.pattern.branches[0].terms[0]
    }

    #[test]
    fn test_parses_a_minimal_rule() {
        let rule = parse_rule("Map = \"map\" { m }");
        assert_eq!(rule.name, "Map");
        assert!(rule.published());
        assert_eq!(rule.name_span, Span::new(0, 3));
        assert_eq!(only_term(&rule).atom, Atom::Literal(vec!["map".to_owned()]));

        let actions = rule.actions.expect("the rule should have actions");
        assert_eq!(actions.actions.len(), 1);
        match &actions.actions[0].kind {
            ActionKind::Press(chord) => assert_eq!(chord.to_string(), "m"),
            other => panic!("expected a press, got {other:?}"),
        }
    }

    #[test]
    fn test_private_rules_are_not_published() {
        let rule = parse_rule("subject = \"all\"");
        assert!(!rule.published());
        assert!(
            rule.actions.is_none(),
            "a rule without a block implicitly propagates"
        );
    }

    #[test]
    fn test_top_level_alternation_gives_the_block_to_the_rule() {
        // `{ m }` binds to the rule, not to the "toggle map" branch.
        let rule = parse_rule("Map = \"map\" | \"toggle map\" { m }");
        assert_eq!(rule.pattern.branches.len(), 2);
        assert!(rule.pattern.branches[0].actions.is_none());
        assert!(rule.pattern.branches[1].actions.is_none());
        assert!(rule.actions.is_some());
        assert_eq!(
            rule.pattern.branches[1].terms[0].atom,
            Atom::Literal(vec!["toggle".to_owned(), "map".to_owned()])
        );
    }

    #[test]
    fn test_inline_actions_bind_to_group_branches() {
        let rule = parse_rule("squad = ( \"one\" { f1 } | \"two\" { f2 } )");
        let Atom::Group(group) = &only_term(&rule).atom else {
            panic!("expected a group");
        };
        assert_eq!(group.branches.len(), 2);
        for (branch, key) in group.branches.iter().zip(["f1", "f2"]) {
            let actions = branch.actions.as_ref().expect("branch actions");
            match &actions.actions[0].kind {
                ActionKind::Press(chord) => assert_eq!(chord.to_string(), key),
                other => panic!("expected a press, got {other:?}"),
            }
        }
        assert!(rule.actions.is_none());
    }

    #[test]
    fn test_rules_end_at_the_next_definition() {
        let rules = parse_ok("Select = subject\nAdvance = subject \"advance\" { ..., 1, 2 }");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "Select");
        assert_eq!(only_term(&rules[0]).atom, Atom::Ref("subject".to_owned()));
        assert_eq!(rules[1].name, "Advance");
        assert_eq!(rules[1].pattern.branches[0].terms.len(), 2);
    }

    #[rstest]
    #[case("r = a?", 0, 1)]
    #[case("r = a*", 0, MAX_REPETITION)]
    #[case("r = a+", 1, MAX_REPETITION)]
    #[case("r = a[3]", 3, 3)]
    #[case("r = a[1..4]", 1, 4)]
    #[case("r = a[2..]", 2, MAX_REPETITION)]
    #[case("r = a[..9]", 0, 9)]
    #[case("r = (\"and\"? a)[0..9]", 0, 9)]
    fn test_repetition_forms(#[case] source: &str, #[case] min: usize, #[case] max: usize) {
        let rule = parse_rule(source);
        let repeat = only_term(&rule).repeat.expect("a repetition");
        assert_eq!((repeat.min, repeat.max), (min, max), "for {source:?}");
    }

    #[test]
    fn test_captures_name_terms_and_groups() {
        let rule =
            parse_rule("Assign = subject:sub (\"team\"? colour):colour { sub..., 9, colour... }");
        let terms = &rule.pattern.branches[0].terms;
        assert_eq!(
            terms[0].capture.as_ref().map(|c| c.name.as_str()),
            Some("sub")
        );
        assert_eq!(
            terms[1].capture.as_ref().map(|c| c.name.as_str()),
            Some("colour")
        );
        assert!(matches!(terms[1].atom, Atom::Group(_)));

        let actions = rule.actions.expect("actions");
        assert_eq!(
            actions.actions[0].kind,
            ActionKind::SpliceCapture("sub".to_owned())
        );
        assert_eq!(
            actions.actions[2].kind,
            ActionKind::SpliceCapture("colour".to_owned())
        );
    }

    #[test]
    fn test_repetition_and_capture_order() {
        let rule = parse_rule("r = a[1..2]:name");
        let term = only_term(&rule);
        assert!(term.repeat.is_some());
        assert_eq!(term.capture.as_ref().map(|c| c.name.as_str()), Some("name"));
    }

    #[test]
    fn test_action_items() {
        let rule = parse_rule(
            "R = \"x\" { shift+f1, wait(20ms), hold(leftctrl), release(leftctrl), release(*), ..., sub... }",
        );
        let actions = rule.actions.expect("actions").actions;
        let kinds: Vec<&ActionKind> = actions.iter().map(|action| &action.kind).collect();

        match kinds[0] {
            ActionKind::Press(chord) => {
                assert_eq!(chord.to_string(), "shift+f1");
                assert_eq!(chord.segments.len(), 2);
            }
            other => panic!("expected a chord press, got {other:?}"),
        }
        assert_eq!(*kinds[1], ActionKind::Wait(Duration::from_millis(20)));
        assert!(matches!(kinds[2], ActionKind::Hold(chord) if chord.to_string() == "leftctrl"));
        assert!(matches!(kinds[3], ActionKind::Release(chord) if chord.to_string() == "leftctrl"));
        assert_eq!(*kinds[4], ActionKind::ReleaseAll);
        assert_eq!(*kinds[5], ActionKind::SpliceAll);
        assert_eq!(*kinds[6], ActionKind::SpliceCapture("sub".to_owned()));
    }

    #[test]
    fn test_numbers_are_valid_chords() {
        let rule = parse_rule("R = \"x\" { 1, 2 }");
        let actions = rule.actions.expect("actions").actions;
        assert!(matches!(&actions[0].kind, ActionKind::Press(chord) if chord.to_string() == "1"));
    }

    #[test]
    fn test_comments_are_ignored() {
        let rules = parse_ok("// a comment\nMap = \"map\" { m } // trailing\n// done");
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn test_spans_are_byte_offsets_into_the_source() {
        let source = "Map = \"map\" { m }";
        let rule = parse_rule(source);
        assert_eq!(&source[rule.name_span.start..rule.name_span.end], "Map");
        let term = only_term(&rule);
        assert_eq!(&source[term.atom_span.start..term.atom_span.end], "\"map\"");
        let actions = rule.actions.expect("actions");
        assert_eq!(&source[actions.span.start..actions.span.end], "{ m }");
    }

    #[rstest]
    #[case("Map = ", "end of the grammar")]
    #[case("Map \"map\" { m }", "'=' was expected")]
    #[case("Map = (\"a\" | \"b\" { m }", "')'")]
    #[case("Map = \"map\" { m", "'}'")]
    #[case("Map = \"map\" {}", "expected")]
    #[case("Map = \"map\" { m, }", "expected")]
    #[case("R = \"x\" { wait }", "'('")]
    #[case("R = \"x\" { wait(nonsense) }", "as a duration")]
    #[case("R = \"x\" { hold(*) }", "expected")]
    #[case("r = a[x]", "as a repetition count")]
    #[case("r = a[]", "We found ']' here")]
    #[case("= \"map\"", "'='")]
    fn test_syntax_errors(#[case] source: &str, #[case] expected: &str) {
        let diagnostics = parse_errors(source);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected a message containing {expected:?}, got: {:?}",
            diagnostics
                .iter()
                .map(|diagnostic| &diagnostic.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_recovery_reports_every_broken_rule() {
        // Three rules, the first and third broken: both problems are reported
        // and the middle rule still parses.
        let source = "One = \"a\" { m\nTwo = \"b\" { i }\nThree = ";
        let (rules, diagnostics) = parse(source);
        assert!(
            diagnostics.len() >= 2,
            "both broken rules should report, got: {diagnostics:?}"
        );
        let rules = rules.expect("recovery should keep the good rules");
        assert!(
            rules.iter().any(|rule| rule.name == "Two"),
            "the healthy rule should survive: {rules:?}"
        );
    }

    #[test]
    fn test_error_spans_point_into_the_source() {
        let source = "R = \"x\" { wait(nonsense) }";
        let diagnostics = parse_errors(source);
        let diagnostic = &diagnostics[0];
        assert_eq!(
            &source[diagnostic.span.start..diagnostic.span.end],
            "nonsense"
        );
        let rendered = diagnostic.render(source);
        assert!(rendered.contains("nonsense"), "got: {rendered}");
    }
}
