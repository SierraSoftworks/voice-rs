use std::fmt::Display;

use super::location::Loc;

/// A lexical token produced by the phrase scanner.
///
/// Every variant carries the source [`Loc`] at which it was found. Word
/// payloads are `&'a str` slices of the phrase source itself — the lexer never
/// allocates.
#[derive(Debug, PartialEq)]
pub enum Token<'a> {
    /// A plain word, carrying its text.
    Word(Loc, &'a str),
    /// An opening bracket `[` (start of an optional group).
    LeftBracket(Loc),
    /// A closing bracket `]` (end of an optional group).
    RightBracket(Loc),
    /// An opening brace `{` (start of an alternates group).
    LeftBrace(Loc),
    /// A closing brace `}` (end of an alternates group).
    RightBrace(Loc),
    /// A comma `,` separating alternate branches.
    Comma(Loc),
}

impl Token<'_> {
    /// Returns the textual lexeme this token was parsed from (e.g. `"["` for
    /// [`Token::LeftBracket`], or the word itself for [`Token::Word`]).
    pub fn lexeme(&self) -> &str {
        match self {
            Token::Word(.., word) => word,
            Token::LeftBracket(..) => "[",
            Token::RightBracket(..) => "]",
            Token::LeftBrace(..) => "{",
            Token::RightBrace(..) => "}",
            Token::Comma(..) => ",",
        }
    }

    /// Returns the source [`Loc`] at which this token appears.
    pub fn location(&self) -> Loc {
        match self {
            Token::Word(loc, ..) => *loc,
            Token::LeftBracket(loc) => *loc,
            Token::RightBracket(loc) => *loc,
            Token::LeftBrace(loc) => *loc,
            Token::RightBrace(loc) => *loc,
            Token::Comma(loc) => *loc,
        }
    }
}

impl Display for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.lexeme())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const LOC: Loc = Loc { line: 3, column: 7 };

    #[rstest]
    #[case(Token::Word(LOC, "deploy"), "deploy")]
    #[case(Token::LeftBracket(LOC), "[")]
    #[case(Token::RightBracket(LOC), "]")]
    #[case(Token::LeftBrace(LOC), "{")]
    #[case(Token::RightBrace(LOC), "}")]
    #[case(Token::Comma(LOC), ",")]
    fn lexemes_and_locations(#[case] token: Token<'_>, #[case] lexeme: &str) {
        assert_eq!(token.lexeme(), lexeme);
        assert_eq!(token.location(), LOC);
        assert_eq!(token.to_string(), lexeme);
    }
}
