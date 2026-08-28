use super::{location::Loc, token::Token};

/// Returns whether `c` may appear within a word: letters, digits, apostrophes
/// (`don't`), and hyphens (`auto-cannon`).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '-'
}

/// A zero-copy scanner over a phrase source string.
///
/// The scanner *is* the token stream: it implements
/// `Iterator<Item = Result<Token, Error>>`, with lexing errors surfacing as
/// stream items at exactly the point the parser demands the bad token. Word
/// payloads are slices of the source — no allocation happens while lexing.
pub struct Scanner<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    line: usize,
    line_start: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            chars: source.char_indices().peekable(),
            line: 1,
            line_start: 0,
        }
    }

    /// Returns the byte offset of the next character to be consumed, or the
    /// length of the source if the scanner has reached the end of its input.
    fn position(&mut self) -> usize {
        self.chars
            .peek()
            .map(|(idx, _)| *idx)
            .unwrap_or(self.source.len())
    }

    /// Reads the remainder of a word whose first character (at byte offset
    /// `start`) has already been consumed.
    fn read_word(&mut self, start: usize) -> Token<'a> {
        while matches!(self.chars.peek(), Some((_, c)) if is_word_char(*c)) {
            self.chars.next();
        }

        Token::Word(
            Loc::new(self.line, 1 + start - self.line_start),
            &self.source[start..self.position()],
        )
    }
}

impl<'a> Iterator for Scanner<'a> {
    type Item = Result<Token<'a>, crate::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((idx, c)) = self.chars.next() {
            let location = Loc::new(self.line, 1 + idx - self.line_start);
            match c {
                '\n' => {
                    self.line += 1;
                    self.line_start = idx + 1;
                }
                c if c.is_whitespace() => {}
                '[' => return Some(Ok(Token::LeftBracket(location))),
                ']' => return Some(Ok(Token::RightBracket(location))),
                '{' => return Some(Ok(Token::LeftBrace(location))),
                '}' => return Some(Ok(Token::RightBrace(location))),
                ',' => return Some(Ok(Token::Comma(location))),
                c if is_word_char(c) => return Some(Ok(self.read_word(idx))),
                c => {
                    return Some(Err(human_errors::user(
                        format!("We found an unexpected character '{c}' at {location}."),
                        &[
                            "Phrases may only contain words, '[optional]' groups and '{alternate, choices}' groups.",
                        ],
                    )));
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    macro_rules! assert_sequence {
      ($phrase:expr $(, $item:pat)* $(,)?) => {
        let mut scanner = Scanner::new($phrase);
        $(
          match scanner.next() {
            Some(Ok($item)) => {},
            Some(Ok(item)) => panic!("Expected '{}' but got '{:?}'", stringify!($item), item),
            Some(Err(e)) => panic!("Error: {}", e),
            None => panic!("Expected '{}' but got the end of the token sequence instead", stringify!($item)),
          }
        )*

        assert!(scanner.next().is_none(), "expected end of sequence, but got an item");
      };
    }

    #[test]
    fn test_empty() {
        assert_sequence!("");
    }

    #[test]
    fn test_whitespace() {
        assert_sequence!("  \t\r\n");
    }

    #[test]
    fn test_words() {
        assert_sequence!(
            "deploy the sentry",
            Token::Word(.., "deploy"),
            Token::Word(.., "the"),
            Token::Word(.., "sentry"),
        );
    }

    #[test]
    fn test_word_characters() {
        // Apostrophes and hyphens are word characters, and digits are fine too.
        assert_sequence!(
            "don't auto-cannon mark2",
            Token::Word(.., "don't"),
            Token::Word(.., "auto-cannon"),
            Token::Word(.., "mark2"),
        );
    }

    #[test]
    fn test_brackets_braces_and_commas() {
        assert_sequence!(
            "[] {,}",
            Token::LeftBracket(..),
            Token::RightBracket(..),
            Token::LeftBrace(..),
            Token::Comma(..),
            Token::RightBrace(..),
        );

        // Groups need no whitespace around their delimiters.
        assert_sequence!(
            "[the]{a,b}",
            Token::LeftBracket(..),
            Token::Word(.., "the"),
            Token::RightBracket(..),
            Token::LeftBrace(..),
            Token::Word(.., "a"),
            Token::Comma(..),
            Token::Word(.., "b"),
            Token::RightBrace(..),
        );
    }

    #[test]
    fn test_full_phrase() {
        assert_sequence!(
            "deploy [the] {autocannon, auto cannon} [sentry]",
            Token::Word(Loc { line: 1, column: 1 }, "deploy"),
            Token::LeftBracket(Loc { line: 1, column: 8 }),
            Token::Word(Loc { line: 1, column: 9 }, "the"),
            Token::RightBracket(Loc {
                line: 1,
                column: 12
            }),
            Token::LeftBrace(Loc {
                line: 1,
                column: 14
            }),
            Token::Word(
                Loc {
                    line: 1,
                    column: 15
                },
                "autocannon"
            ),
            Token::Comma(Loc {
                line: 1,
                column: 25
            }),
            Token::Word(
                Loc {
                    line: 1,
                    column: 27
                },
                "auto"
            ),
            Token::Word(
                Loc {
                    line: 1,
                    column: 32
                },
                "cannon"
            ),
            Token::RightBrace(Loc {
                line: 1,
                column: 38
            }),
            Token::LeftBracket(Loc {
                line: 1,
                column: 40
            }),
            Token::Word(
                Loc {
                    line: 1,
                    column: 41
                },
                "sentry"
            ),
            Token::RightBracket(Loc {
                line: 1,
                column: 47
            }),
        );
    }

    #[test]
    fn test_unicode_words_are_sliced_on_character_boundaries() {
        assert_sequence!(
            "café über łódź",
            Token::Word(.., "café"),
            Token::Word(.., "über"),
            Token::Word(.., "łódź"),
        );
    }

    #[test]
    fn test_location_tracking_across_lines() {
        assert_sequence!(
            "deploy\n  [the]\nsentry",
            Token::Word(Loc { line: 1, column: 1 }, "deploy"),
            Token::LeftBracket(Loc { line: 2, column: 3 }),
            Token::Word(Loc { line: 2, column: 4 }, "the"),
            Token::RightBracket(Loc { line: 2, column: 7 }),
            Token::Word(Loc { line: 3, column: 1 }, "sentry"),
        );
    }

    #[rstest]
    #[case("(", "We found an unexpected character '(' at line 1, column 1.")]
    #[case(
        "deploy (now)",
        "We found an unexpected character '(' at line 1, column 8."
    )]
    #[case("fire!", "We found an unexpected character '!' at line 1, column 5.")]
    #[case("a.b", "We found an unexpected character '.' at line 1, column 2.")]
    #[case(
        "deploy\nfire (x)",
        "We found an unexpected character '(' at line 2, column 6."
    )]
    fn test_invalid_characters(#[case] input: &str, #[case] message: &str) {
        let mut scanner = Scanner::new(input);
        let error = loop {
            match scanner.next() {
                Some(Ok(..)) => continue,
                Some(Err(e)) => break e,
                None => panic!("Expected an error while scanning '{input}'"),
            }
        };

        assert!(
            error.to_string().contains(message),
            "Expected error message to contain '{}', got '{}'",
            message,
            error
        );
    }
}
