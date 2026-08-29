//! The chumsky lexer: grammar source → spanned [`Token`] stream.
//!
//! Errors do not abort the scan. A bad character or a malformed literal emits
//! a [`Rich`] error and lexing continues, so one load of a grammar reports
//! every lexical problem it has, not just the first.

use chumsky::prelude::*;

use super::token::Token;

pub(super) type LexError<'src> = Rich<'src, char, SimpleSpan<usize>>;
pub(super) type SpannedToken = (Token, SimpleSpan<usize>);

/// Whether `c` may appear inside a spoken literal word: letters, digits,
/// apostrophes (`don't`) and hyphens (`auto-cannon`), exactly as in the old
/// phrase DSL.
fn is_literal_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '-'
}

/// Builds the lexer for a whole grammar source.
pub(super) fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<SpannedToken>, extra::Err<LexError<'src>>> {
    let word = any()
        .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_')
        .repeated()
        .at_least(1)
        .to_slice()
        .map(|word: &str| Token::Word(word.to_owned()));

    // Literals are validated here rather than in the parser so the error can
    // point at the quoted text itself; a bad word still yields a token, so the
    // rest of the grammar keeps lexing and parsing.
    let literal = none_of("\"\n")
        .repeated()
        .to_slice()
        .delimited_by(just('"'), just('"'))
        .validate(|content: &str, extra, emitter| {
            let words: Vec<String> = content
                .split_whitespace()
                .map(str::to_lowercase)
                .collect();

            if words.is_empty() {
                emitter.emit(Rich::custom(
                    extra.span(),
                    "You've written an empty literal — every literal needs at least one spoken word, e.g. \"fall back\".",
                ));
            }

            for word in &words {
                if let Some(bad) = word.chars().find(|c| !is_literal_word_char(*c)) {
                    emitter.emit(Rich::custom(
                        extra.span(),
                        format!(
                            "The word '{word}' contains '{bad}', which can't be spoken — literal words may only use letters, digits, apostrophes and hyphens."
                        ),
                    ));
                }
            }

            Token::Literal(words)
        });

    let structural = choice((
        just("...").to(Token::Ellipsis),
        just("..").to(Token::DotDot),
        just('=').to(Token::Eq),
        just('|').to(Token::Pipe),
        just('(').to(Token::LParen),
        just(')').to(Token::RParen),
        just('{').to(Token::LBrace),
        just('}').to(Token::RBrace),
        just('[').to(Token::LBracket),
        just(']').to(Token::RBracket),
        just('?').to(Token::Question),
        just('*').to(Token::Star),
        just('+').to(Token::Plus),
        just(':').to(Token::Colon),
        just(',').to(Token::Comma),
    ));

    let line_comment = just("//").then(none_of("\n").repeated()).ignored();
    let padding = any()
        .filter(|c: &char| c.is_whitespace())
        .ignored()
        .or(line_comment)
        .repeated();

    let token = choice((literal, word, structural)).map(Some);

    // Anything no token matched is reported and skipped, so a stray character
    // costs one diagnostic instead of the rest of the grammar.
    let unexpected = any()
        .validate(|c: char, extra, emitter| {
            let message = if c == '"' {
                "You have an unclosed '\"' — every literal needs a closing quote on the same line.".to_owned()
            } else {
                format!(
                    "We found an unexpected character '{c}' — grammars are made of rules like: name = \"spoken words\" {{ f1 }}."
                )
            };
            emitter.emit(Rich::custom(extra.span(), message));
        })
        .to(None);

    choice((token, unexpected))
        .map_with(|token, extra| token.map(|token| (token, extra.span())))
        .padded_by(padding)
        .repeated()
        .collect::<Vec<Option<SpannedToken>>>()
        .map(|tokens| tokens.into_iter().flatten().collect())
        .padded_by(padding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn lex(source: &str) -> Vec<Token> {
        let (tokens, errors) = lexer().parse(source).into_output_errors();
        assert!(errors.is_empty(), "unexpected lex errors: {errors:?}");
        tokens
            .expect("the source should lex")
            .into_iter()
            .map(|(token, _)| token)
            .collect()
    }

    fn lex_errors(source: &str) -> Vec<String> {
        let (_, errors) = lexer().parse(source).into_output_errors();
        errors.into_iter().map(|error| error.to_string()).collect()
    }

    #[rstest]
    #[case("name", vec![Token::Word("name".into())])]
    #[case("f1", vec![Token::Word("f1".into())])]
    #[case("20ms", vec![Token::Word("20ms".into())])]
    #[case("snake_case_9", vec![Token::Word("snake_case_9".into())])]
    #[case("\"map\"", vec![Token::Literal(vec!["map".into()])])]
    #[case(
        "\"Toggle  MAP\"",
        vec![Token::Literal(vec!["toggle".into(), "map".into()])]
    )]
    #[case(
        "\"don't auto-cannon\"",
        vec![Token::Literal(vec!["don't".into(), "auto-cannon".into()])]
    )]
    #[case("= | ( ) { } [ ]", vec![
        Token::Eq, Token::Pipe, Token::LParen, Token::RParen,
        Token::LBrace, Token::RBrace, Token::LBracket, Token::RBracket,
    ])]
    #[case("? * + : ,", vec![
        Token::Question, Token::Star, Token::Plus, Token::Colon, Token::Comma,
    ])]
    #[case("0..9", vec![
        Token::Word("0".into()), Token::DotDot, Token::Word("9".into()),
    ])]
    #[case("...", vec![Token::Ellipsis])]
    #[case("sub...", vec![Token::Word("sub".into()), Token::Ellipsis])]
    #[case("shift+f1", vec![
        Token::Word("shift".into()), Token::Plus, Token::Word("f1".into()),
    ])]
    #[case("// just a comment", vec![])]
    #[case("a // trailing\nb", vec![Token::Word("a".into()), Token::Word("b".into())])]
    #[case("", vec![])]
    #[case("  \n\t ", vec![])]
    fn test_lexes(#[case] source: &str, #[case] expected: Vec<Token>) {
        assert_eq!(lex(source), expected);
    }

    #[test]
    fn test_spans_are_byte_offsets() {
        let (tokens, _) = lexer().parse("ab = \"cd\"").into_output_errors();
        let tokens = tokens.expect("the source should lex");
        assert_eq!(tokens[0].1.start, 0);
        assert_eq!(tokens[0].1.end, 2);
        assert_eq!(tokens[1].1.start, 3);
        assert_eq!(tokens[2].1.start, 5);
        assert_eq!(tokens[2].1.end, 9);
    }

    #[rstest]
    #[case("\"\"", "empty literal")]
    #[case("\"   \"", "empty literal")]
    #[case("\"team (red)\"", "can't be spoken")]
    #[case("\"open map", "unclosed '\"'")]
    #[case("a = b; c", "unexpected character ';'")]
    fn test_reports_lexical_problems(#[case] source: &str, #[case] expected: &str) {
        let errors = lex_errors(source);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "expected an error containing {expected:?}, got: {errors:?}"
        );
    }

    #[test]
    fn test_errors_accumulate_and_lexing_continues() {
        let (tokens, errors) = lexer().parse("a ; b ; c").into_output_errors();
        assert_eq!(errors.len(), 2, "both stray characters should be reported");
        assert_eq!(
            tokens.expect("the good tokens should survive").len(),
            3,
            "the words around the bad characters should still lex"
        );
    }
}
