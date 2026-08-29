//! The token vocabulary of the grammar v2 language.
//!
//! The lexer produces a flat stream of these; the parser gives them meaning.
//! One deliberate simplification: [`Token::Word`] covers rule names, key
//! names, repetition bounds and duration blobs alike (`subject`, `f1`, `9`,
//! `20ms` are all words). Which of those a word *is* depends entirely on where
//! it appears, so the parser decides — the lexer never has to.

use std::fmt;

/// One lexical token of a grammar source, without its span.
///
/// Spans travel beside tokens as `(Token, SimpleSpan)` pairs so that this type
/// stays comparable with `just(...)` in the parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// A bare word: a rule name, key name, number or duration, depending on
    /// context. Words are runs of ASCII letters, digits and underscores.
    Word(String),
    /// A double-quoted spoken literal, already split on whitespace into its
    /// lowercased word tokens (`"toggle map"` becomes `["toggle", "map"]`).
    Literal(Vec<String>),
    /// `=`, introducing a rule definition.
    Eq,
    /// `|`, separating alternation branches.
    Pipe,
    /// `(`, opening a group or an action argument list.
    LParen,
    /// `)`.
    RParen,
    /// `{`, opening an action block.
    LBrace,
    /// `}`.
    RBrace,
    /// `[`, opening a repetition bound.
    LBracket,
    /// `]`.
    RBracket,
    /// `?`, sugar for `[0..1]`.
    Question,
    /// `*`, sugar for `[0..MAX_REPETITION]`.
    Star,
    /// `+`: repetition sugar for `[1..MAX_REPETITION]` after a term, or the
    /// chord separator inside an action block (`shift+f1`).
    Plus,
    /// `:`, naming a capture.
    Colon,
    /// `,`, separating actions in a block.
    Comma,
    /// `..`, the range separator in a repetition bound.
    DotDot,
    /// `...`, the splice-all action (also the tail of a capture splice).
    Ellipsis,
}

impl fmt::Display for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word(word) => formatter.write_str(word),
            Self::Literal(words) => write!(formatter, "\"{}\"", words.join(" ")),
            Self::Eq => formatter.write_str("="),
            Self::Pipe => formatter.write_str("|"),
            Self::LParen => formatter.write_str("("),
            Self::RParen => formatter.write_str(")"),
            Self::LBrace => formatter.write_str("{"),
            Self::RBrace => formatter.write_str("}"),
            Self::LBracket => formatter.write_str("["),
            Self::RBracket => formatter.write_str("]"),
            Self::Question => formatter.write_str("?"),
            Self::Star => formatter.write_str("*"),
            Self::Plus => formatter.write_str("+"),
            Self::Colon => formatter.write_str(":"),
            Self::Comma => formatter.write_str(","),
            Self::DotDot => formatter.write_str(".."),
            Self::Ellipsis => formatter.write_str("..."),
        }
    }
}
