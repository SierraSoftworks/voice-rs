use std::iter::Peekable;

use human_errors::Error;

use super::{
    expr::{Node, PhraseExpr},
    location::Loc,
    token::Token,
};

/// The maximum depth to which `[...]` and `{...}` groups may nest. Deeper
/// phrases are almost certainly a mistake, and the cap bounds parser recursion.
pub const MAX_NESTING_DEPTH: usize = 8;

/// A recursive-descent parser over a phrase token stream.
///
/// The parser is generic in its token iterator so tests can drive it directly;
/// in production the iterator is the [`Scanner`](super::lexer::Scanner), whose
/// lexing errors flow through as stream items and surface at exactly the point
/// the parser demands the bad token.
pub struct Parser<'a, I: Iterator<Item = Result<Token<'a>, Error>>> {
    tokens: Peekable<I>,
    nesting_depth: usize,
}

impl<'a, I: Iterator<Item = Result<Token<'a>, Error>>> Parser<'a, I> {
    /// Parses a complete phrase from `tokens`, consuming the whole stream.
    pub fn parse(tokens: I) -> Result<PhraseExpr<'a>, Error> {
        let mut parser = Parser {
            tokens: tokens.peekable(),
            nesting_depth: 0,
        };

        let nodes = parser.sequence()?;
        parser.ensure_end()?;

        if nodes.is_empty() {
            return Err(human_errors::user(
                "Your phrase is empty — a command needs at least one word.",
                &["Write at least one word, e.g. 'deploy [the] sentry'."],
            ));
        }

        Ok(PhraseExpr(nodes))
    }

    /// Guards the two recursion points (`[` and `{`) against depth abuse.
    fn nested<T>(
        &mut self,
        location: Loc,
        parse: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if self.nesting_depth >= MAX_NESTING_DEPTH {
            return Err(human_errors::user(
                format!(
                    "You've nested groups more than {MAX_NESTING_DEPTH} levels deep at {location}."
                ),
                &[
                    "Simplify the phrase — deeply nested '[{...}]' groups usually read better as several separate commands.",
                ],
            ));
        }

        self.nesting_depth += 1;
        let result = parse(self);
        self.nesting_depth -= 1;
        result
    }

    /// Verifies that the token stream is exhausted. Any leftover token can only
    /// be a group closer (or comma) without an opener, since [`Self::sequence`]
    /// consumes every other kind of token.
    fn ensure_end(&mut self) -> Result<(), Error> {
        match self.tokens.next() {
            None => Ok(()),
            Some(Ok(Token::RightBracket(location))) => Err(stray_right_bracket(location)),
            Some(Ok(Token::RightBrace(location))) => Err(stray_right_brace(location)),
            Some(Ok(Token::Comma(location))) => Err(comma_outside_alternates(location)),
            Some(Err(err)) => Err(err),
            Some(Ok(token)) => unreachable!("the sequence parser consumes '{token}' tokens"),
        }
    }

    /// Parses `term , { term }` — a run of words and groups — stopping (without
    /// consuming) at the first token which cannot start a term. Iterative, so
    /// phrase length costs no recursion depth.
    fn sequence(&mut self) -> Result<Vec<Node<'a>>, Error> {
        let mut nodes = Vec::new();

        loop {
            match self.tokens.peek() {
                Some(Ok(Token::Word(..))) => {
                    if let Some(Ok(Token::Word(location, word))) = self.tokens.next() {
                        nodes.push(Node::Word(location, word));
                    } else {
                        unreachable!()
                    }
                }
                Some(Ok(Token::LeftBracket(..))) => nodes.push(self.optional()?),
                Some(Ok(Token::LeftBrace(..))) => nodes.push(self.alternates()?),
                Some(Err(..)) => return Err(self.tokens.next().unwrap().unwrap_err()),
                _ => return Ok(nodes),
            }
        }
    }

    /// Parses an `[optional]` group. The caller has peeked the `[`.
    fn optional(&mut self) -> Result<Node<'a>, Error> {
        let location = self.tokens.next().unwrap()?.location();
        let nodes = self.nested(location, |parser| parser.sequence())?;

        match self.tokens.next() {
            Some(Ok(Token::RightBracket(..))) => {
                if nodes.is_empty() {
                    Err(empty_optional(location))
                } else {
                    Ok(Node::Optional(location, nodes))
                }
            }
            Some(Ok(Token::Comma(comma))) => Err(comma_outside_alternates(comma)),
            // A `}` here means an enclosing `{` was never closed — but the
            // nearest unfinished group is this `[`, so report it.
            Some(Ok(Token::RightBrace(..))) | None => Err(unclosed_optional(location)),
            Some(Err(err)) => Err(err),
            Some(Ok(token)) => unreachable!("the sequence parser consumes '{token}' tokens"),
        }
    }

    /// Parses an `{alternate, choices}` group. The caller has peeked the `{`.
    fn alternates(&mut self) -> Result<Node<'a>, Error> {
        let location = self.tokens.next().unwrap()?.location();
        let branches = self.nested(location, |parser| {
            let mut branches = Vec::new();
            loop {
                let branch = parser.sequence()?;
                match parser.tokens.next() {
                    Some(Ok(Token::Comma(..))) => {
                        if branch.is_empty() {
                            return Err(empty_branch(location));
                        }
                        branches.push(branch);
                    }
                    Some(Ok(Token::RightBrace(..))) => {
                        if branch.is_empty() {
                            return Err(empty_branch(location));
                        }
                        branches.push(branch);
                        return Ok(branches);
                    }
                    // A `]` here means an enclosing `[` was never closed — but
                    // the nearest unfinished group is this `{`, so report it.
                    Some(Ok(Token::RightBracket(..))) | None => {
                        return Err(unclosed_alternates(location));
                    }
                    Some(Err(err)) => return Err(err),
                    Some(Ok(token)) => {
                        unreachable!("the sequence parser consumes '{token}' tokens")
                    }
                }
            }
        })?;

        Ok(Node::Alternates(location, branches))
    }
}

// The advice arrays must be `&'static`, so all dynamic detail (most notably the
// location) lives in the message.

fn unclosed_optional(location: Loc) -> Error {
    human_errors::user(
        format!(
            "You have an unclosed '[' at {location} — every optional group needs a matching ']'."
        ),
        &["Close the optional group, e.g. 'deploy [the] sentry'."],
    )
}

fn unclosed_alternates(location: Loc) -> Error {
    human_errors::user(
        format!(
            "You have an unclosed '{{' at {location} — every alternates group needs a matching '}}'."
        ),
        &["Close the alternates group, e.g. 'deploy {autocannon, auto cannon}'."],
    )
}

fn stray_right_bracket(location: Loc) -> Error {
    human_errors::user(
        format!("We found a ']' at {location} without a matching '[' before it."),
        &["Remove the stray ']' or add a '[' where the optional words begin."],
    )
}

fn stray_right_brace(location: Loc) -> Error {
    human_errors::user(
        format!("We found a '}}' at {location} without a matching '{{' before it."),
        &["Remove the stray '}' or add a '{' where the alternate choices begin."],
    )
}

fn comma_outside_alternates(location: Loc) -> Error {
    human_errors::user(
        format!("We found a ',' at {location} outside of an '{{alternate, choices}}' group."),
        &[
            "Commas only separate branches inside '{...}' groups — remove it, or wrap the alternatives in braces.",
        ],
    )
}

fn empty_optional(location: Loc) -> Error {
    human_errors::user(
        format!("The optional group at {location} is empty."),
        &["Put at least one word inside '[...]', or remove the brackets entirely."],
    )
}

fn empty_branch(location: Loc) -> Error {
    human_errors::user(
        format!(
            "The alternates group starting at {location} has an empty branch (a ',' with nothing before or after it)."
        ),
        &[
            "Every branch in an '{a, b}' group needs at least one word, e.g. '{autocannon, auto cannon}'.",
        ],
    )
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::lexer::Scanner;
    use super::*;

    fn parse(input: &str) -> Result<PhraseExpr<'_>, Error> {
        Parser::parse(Scanner::new(input))
    }

    fn word(line: usize, column: usize, word: &str) -> Node<'_> {
        Node::Word(Loc::new(line, column), word)
    }

    #[rstest]
    #[case("deploy", PhraseExpr(vec![word(1, 1, "deploy")]))]
    #[case("deploy sentry", PhraseExpr(vec![word(1, 1, "deploy"), word(1, 8, "sentry")]))]
    #[case("[the]", PhraseExpr(vec![Node::Optional(Loc::new(1, 1), vec![word(1, 2, "the")])]))]
    #[case("{a, b}", PhraseExpr(vec![Node::Alternates(
        Loc::new(1, 1),
        vec![vec![word(1, 2, "a")], vec![word(1, 5, "b")]],
    )]))]
    #[case("deploy [the] {autocannon, auto cannon} [sentry]", PhraseExpr(vec![
        word(1, 1, "deploy"),
        Node::Optional(Loc::new(1, 8), vec![word(1, 9, "the")]),
        Node::Alternates(Loc::new(1, 14), vec![
            vec![word(1, 15, "autocannon")],
            vec![word(1, 27, "auto"), word(1, 32, "cannon")],
        ]),
        Node::Optional(Loc::new(1, 40), vec![word(1, 41, "sentry")]),
    ]))]
    #[case("[{optional, elective}] combinations", PhraseExpr(vec![
        Node::Optional(Loc::new(1, 1), vec![Node::Alternates(Loc::new(1, 2), vec![
            vec![word(1, 3, "optional")],
            vec![word(1, 13, "elective")],
        ])]),
        word(1, 24, "combinations"),
    ]))]
    #[case("{[the] big, small} gun", PhraseExpr(vec![
        Node::Alternates(Loc::new(1, 1), vec![
            vec![
                Node::Optional(Loc::new(1, 2), vec![word(1, 3, "the")]),
                word(1, 8, "big"),
            ],
            vec![word(1, 13, "small")],
        ]),
        word(1, 20, "gun"),
    ]))]
    fn test_parsing(#[case] input: &str, #[case] ast: PhraseExpr<'_>) {
        match parse(input) {
            Ok(expr) => assert_eq!(expr, ast, "Expected '{input}' to parse to {ast:?}"),
            Err(e) => panic!("Error: {}", e),
        }
    }

    #[test]
    fn test_multi_line_locations() {
        let expr = parse("deploy\n  [the]\nsentry").expect("the phrase should parse");
        assert_eq!(
            expr,
            PhraseExpr(vec![
                word(1, 1, "deploy"),
                Node::Optional(Loc::new(2, 3), vec![word(2, 4, "the")]),
                word(3, 1, "sentry"),
            ])
        );
    }

    #[rstest]
    #[case(
        "deploy [the sentry",
        "You have an unclosed '[' at line 1, column 8 — every optional group needs a matching ']'."
    )]
    #[case(
        "a ]",
        "We found a ']' at line 1, column 3 without a matching '[' before it."
    )]
    #[case(
        "deploy {a, , b}",
        "The alternates group starting at line 1, column 8 has an empty branch (a ',' with nothing before or after it)."
    )]
    #[case(
        "{a, }",
        "The alternates group starting at line 1, column 1 has an empty branch (a ',' with nothing before or after it)."
    )]
    #[case(
        "{, a}",
        "The alternates group starting at line 1, column 1 has an empty branch (a ',' with nothing before or after it)."
    )]
    #[case(
        "{}",
        "The alternates group starting at line 1, column 1 has an empty branch (a ',' with nothing before or after it)."
    )]
    #[case("deploy []", "The optional group at line 1, column 8 is empty.")]
    #[case(
        "[[[[[[[[[a]]]]]]]]]",
        "You've nested groups more than 8 levels deep at line 1, column 9."
    )]
    #[case(
        "deploy (now)",
        "We found an unexpected character '(' at line 1, column 8."
    )]
    #[case(
        "deploy {autocannon, auto cannon",
        "You have an unclosed '{' at line 1, column 8 — every alternates group needs a matching '}'."
    )]
    #[case(
        "a }",
        "We found a '}' at line 1, column 3 without a matching '{' before it."
    )]
    #[case(
        "a, b",
        "We found a ',' at line 1, column 2 outside of an '{alternate, choices}' group."
    )]
    #[case(
        "[a, b]",
        "We found a ',' at line 1, column 3 outside of an '{alternate, choices}' group."
    )]
    #[case("", "Your phrase is empty — a command needs at least one word.")]
    #[case(
        "  \t\n  ",
        "Your phrase is empty — a command needs at least one word."
    )]
    #[case(
        "[{a]",
        "You have an unclosed '{' at line 1, column 2 — every alternates group needs a matching '}'."
    )]
    #[case(
        "{[a}",
        "You have an unclosed '[' at line 1, column 2 — every optional group needs a matching ']'."
    )]
    #[case(
        "deploy\n  (x)",
        "We found an unexpected character '(' at line 2, column 3."
    )]
    fn test_invalid_phrases(#[case] input: &str, #[case] message: &str) {
        match parse(input) {
            Ok(expr) => panic!("Expected an error, got {:?}", expr),
            Err(e) => assert!(
                e.to_string().contains(message),
                "Expected error message to contain '{}', got '{}'",
                message,
                e
            ),
        }
    }

    #[test]
    fn test_eight_levels_of_nesting_are_accepted() {
        let expr = parse("[[[[[[[[a]]]]]]]]").expect("8 levels of nesting should parse");

        // Unwrap the eight optional layers back down to the word.
        let mut nodes = &expr.0;
        for _ in 0..8 {
            match nodes.as_slice() {
                [Node::Optional(_, inner)] => nodes = inner,
                other => panic!("Expected an optional group, got {other:?}"),
            }
        }
        assert_eq!(nodes.as_slice(), &[word(1, 9, "a")]);

        // Mixed bracket/brace nesting counts the same way.
        parse("{[{[{[{[a]}]}]}]}").expect("mixed 8-level nesting should parse");
    }

    #[test]
    fn test_long_phrases_cost_no_recursion() {
        // Sequences are parsed iteratively, so a 500-word phrase parses without
        // any per-word recursion.
        let source = vec!["word"; 500].join(" ");
        let expr = parse(&source).expect("the phrase should parse");
        assert_eq!(expr.0.len(), 500);
    }
}
