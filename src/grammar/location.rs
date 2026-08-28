use std::fmt::Display;

/// A position within a command phrase, identifying where a token was found.
///
/// Lines and columns are 1-based; the default `Loc { line: 0, column: 0 }`
/// represents an unknown location and formats as `"unknown location"`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Copy, Clone, Hash, Default)]
pub struct Loc {
    /// The 1-based line number (`0` if unknown).
    pub line: usize,
    /// The 1-based column number (`0` if unknown).
    pub column: usize,
}

impl Loc {
    /// Creates a new location at the given 1-based `line` and `column`.
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

impl Display for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Loc { line: 0, column: 0 } => {
                write!(f, "unknown location")
            }
            Loc { line, column } => {
                write!(f, "line {}, column {}", line, column)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(0, 0, "unknown location")]
    #[case(1, 2, "line 1, column 2")]
    #[case(3, 41, "line 3, column 41")]
    fn test_display(#[case] line: usize, #[case] column: usize, #[case] expected: &str) {
        assert_eq!(format!("{}", Loc::new(line, column)), expected);
    }
}
