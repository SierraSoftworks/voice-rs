use std::fmt::Display;

use super::location::Loc;

/// A single term within a parsed command phrase.
///
/// Word payloads borrow from the phrase source; the [`CommandPhrase`] owner
/// type pairs an AST with its pinned source so parsed phrases can live inside
/// config structs.
///
/// [`CommandPhrase`]: super::CommandPhrase
#[derive(Debug, PartialEq, Clone)]
pub enum Node<'a> {
    /// A required word.
    Word(Loc, &'a str),
    /// An `[optional]` group — the whole sequence may be omitted.
    Optional(Loc, Vec<Node<'a>>),
    /// An `{alternate, choices}` group — exactly one branch is spoken.
    Alternates(Loc, Vec<Vec<Node<'a>>>),
}

/// A parsed command phrase: a sequence of [`Node`] terms, spoken in order.
#[derive(Debug, PartialEq, Clone)]
pub struct PhraseExpr<'a>(pub Vec<Node<'a>>);

/// Writes `nodes` separated by single spaces.
fn write_sequence(f: &mut std::fmt::Formatter<'_>, nodes: &[Node<'_>]) -> std::fmt::Result {
    for (i, node) in nodes.iter().enumerate() {
        if i > 0 {
            f.write_str(" ")?;
        }
        write!(f, "{node}")?;
    }
    Ok(())
}

impl Display for Node<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Node::Word(_, word) => f.write_str(word),
            Node::Optional(_, nodes) => {
                f.write_str("[")?;
                write_sequence(f, nodes)?;
                f.write_str("]")
            }
            Node::Alternates(_, branches) => {
                f.write_str("{")?;
                for (i, branch) in branches.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write_sequence(f, branch)?;
                }
                f.write_str("}")
            }
        }
    }
}

impl Display for PhraseExpr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_sequence(f, &self.0)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::{lexer::Scanner, parser::Parser};

    /// Parsing a phrase and displaying its AST yields the whitespace-normalized
    /// source, and that normalized form is a fixpoint of parse → display.
    #[rstest]
    #[case("deploy", "deploy")]
    #[case("deploy   the    sentry", "deploy the sentry")]
    #[case("[ the ]", "[the]")]
    #[case("{a,b}", "{a, b}")]
    #[case("{autocannon,auto cannon}", "{autocannon, auto cannon}")]
    #[case(
        "deploy [the] {autocannon, auto cannon} [sentry]",
        "deploy [the] {autocannon, auto cannon} [sentry]"
    )]
    #[case(
        "[{optional,elective}]  combinations",
        "[{optional, elective}] combinations"
    )]
    #[case("deploy\n  [the]\nsentry", "deploy [the] sentry")]
    fn test_display_round_trip(#[case] input: &str, #[case] expected: &str) {
        let expr = Parser::parse(Scanner::new(input)).expect("the phrase should parse");
        assert_eq!(expr.to_string(), expected);

        let reparsed =
            Parser::parse(Scanner::new(expected)).expect("the display output should parse");
        assert_eq!(reparsed.to_string(), expected);
    }
}
