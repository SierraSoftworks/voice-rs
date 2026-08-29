//! The phrase DSL: lexer, recursive-descent parser, AST, and expansion into
//! concrete recognition phrases. See DESIGN.md §"Grammar DSL".

pub mod expansion;
mod expr;
mod lexer;
mod location;
mod parser;
mod token;
pub mod v2;

#[allow(unused_imports)] // consumed as the wave-2 config/matcher modules land
pub use expr::{Node, PhraseExpr};
#[allow(unused_imports)] // consumed as the wave-2 config/matcher modules land
pub use location::Loc;
#[allow(unused_imports)] // consumed as the wave-2 config/matcher modules land
pub use parser::MAX_NESTING_DEPTH;

/// A parsed command phrase which owns its source.
///
/// The AST borrows `&str` slices from the source string, so the two are paired
/// here: the source is pinned on the heap and the AST is stored with a
/// `'static` lifetime which is narrowed back to the struct's own lifetime on
/// access. This is the same self-referential pattern as `filt_rs::Filter`, and
/// the `unsafe` block in [`CommandPhrase::parse`] is the only `unsafe` in the
/// crate.
///
/// `Deserialize` parses immediately, so a bad phrase inside a profile is a
/// **config-load error** with a precise location — never a runtime surprise.
#[allow(dead_code)] // consumed as the wave-2 config/matcher modules land
pub struct CommandPhrase {
    #[allow(clippy::box_collection)]
    source: std::pin::Pin<Box<String>>,
    expr: PhraseExpr<'static>,
}

#[allow(dead_code)] // consumed as the wave-2 config/matcher modules land
impl CommandPhrase {
    /// Parses the provided phrase source, returning a reusable `CommandPhrase`.
    ///
    /// The source is tokenized and parsed eagerly, so any syntax errors are
    /// reported here rather than at recognition time. Errors include the
    /// location of the problem and guidance on how to correct it.
    pub fn parse(source: String) -> Result<Self, crate::Error> {
        // The AST borrows string slices from the phrase source itself. Pinning
        // the boxed string keeps those borrows valid for the lifetime of this
        // struct without re-allocating the words.
        let source = Box::new(source);
        let source_ptr = std::ptr::NonNull::from(&source);
        let pinned = Box::into_pin(source);

        let tokens = lexer::Scanner::new(unsafe { source_ptr.as_ref() });
        let expr = parser::Parser::parse(tokens)?;
        Ok(Self {
            source: pinned,
            expr,
        })
    }

    /// Returns the phrase exactly as it was written (unlike [`Display`],
    /// which normalizes whitespace by round-tripping through the AST).
    ///
    /// [`Display`]: std::fmt::Display
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the parsed AST, with its lifetime narrowed from `'static` to
    /// this struct's own borrow.
    pub fn expr(&self) -> &PhraseExpr<'_> {
        &self.expr
    }
}

impl Clone for CommandPhrase {
    fn clone(&self) -> Self {
        // The clone cannot share the original's AST (it borrows the original's
        // pinned source), so re-parse — a phrase which parsed once always
        // re-parses.
        Self::parse(self.source().to_string())
            .expect("a previously parsed phrase should always re-parse")
    }
}

impl std::fmt::Debug for CommandPhrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CommandPhrase")
            .field(&self.source())
            .finish()
    }
}

impl std::fmt::Display for CommandPhrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Round-trip via the AST, normalizing whitespace.
        self.expr.fmt(f)
    }
}

impl<'de> serde::Deserialize<'de> for CommandPhrase {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Parsing during deserialization makes a bad phrase a config-load
        // error; the full human-errors message (with its location) is carried
        // through as the custom message.
        let source = String::deserialize(deserializer)?;
        CommandPhrase::parse(source).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("deploy [the] {autocannon, auto cannon} [sentry]")]
    #[case("open [the] terminal")]
    #[case("salute")]
    fn test_display_round_trips(#[case] source: &str) {
        let phrase = CommandPhrase::parse(source.to_string()).expect("the phrase should parse");
        assert_eq!(phrase.source(), source);
        assert_eq!(phrase.to_string(), source);
    }

    #[test]
    fn test_display_normalizes_whitespace_but_source_does_not() {
        let phrase = CommandPhrase::parse("deploy   [ the ]\n {a ,b}".to_string())
            .expect("the phrase should parse");
        assert_eq!(phrase.source(), "deploy   [ the ]\n {a ,b}");
        assert_eq!(phrase.to_string(), "deploy [the] {a, b}");

        // Word case is preserved — lowercasing happens at expansion time.
        let phrase =
            CommandPhrase::parse("Deploy The SENTRY".to_string()).expect("the phrase should parse");
        assert_eq!(phrase.to_string(), "Deploy The SENTRY");
    }

    #[test]
    fn test_parse_errors_carry_locations() {
        let error = CommandPhrase::parse("deploy [the sentry".to_string())
            .expect_err("the phrase should fail to parse");
        assert!(
            error
                .to_string()
                .contains("You have an unclosed '[' at line 1, column 8"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_clone_reparses_equal() {
        let phrase = CommandPhrase::parse("deploy [the] {autocannon, auto cannon} [sentry]".into())
            .expect("the phrase should parse");
        let clone = phrase.clone();

        assert_eq!(clone.source(), phrase.source());
        assert_eq!(clone.expr(), phrase.expr());
        assert_eq!(clone.to_string(), phrase.to_string());
    }

    #[test]
    fn test_expr_exposes_the_ast() {
        let phrase = CommandPhrase::parse("[the] sentry".into()).expect("the phrase should parse");
        match phrase.expr().0.as_slice() {
            [Node::Optional(loc, inner), Node::Word(_, "sentry")] => {
                assert_eq!(*loc, Loc::new(1, 1));
                assert_eq!(inner.as_slice(), &[Node::Word(Loc::new(1, 2), "the")]);
            }
            other => panic!("Unexpected AST: {other:?}"),
        }
    }

    #[derive(Debug, serde::Deserialize)]
    struct Doc {
        name: String,
        phrase: CommandPhrase,
    }

    #[test]
    fn test_deserialize_parses_during_load() {
        let doc: Doc = serde_yaml::from_str(
            "name: Deep Rock Galactic\nphrase: \"deploy [the] {autocannon, auto cannon} [sentry]\"",
        )
        .expect("the document should load");

        assert_eq!(doc.name, "Deep Rock Galactic");
        assert_eq!(
            doc.phrase.to_string(),
            "deploy [the] {autocannon, auto cannon} [sentry]"
        );
        assert_eq!(expansion::count(doc.phrase.expr()), 8);
    }

    #[test]
    fn test_deserialize_surfaces_located_errors_at_load_time() {
        let error = serde_yaml::from_str::<Doc>("name: broken\nphrase: \"deploy [the sentry\"")
            .expect_err("the document should fail to load");
        assert!(
            error
                .to_string()
                .contains("You have an unclosed '[' at line 1, column 8"),
            "unexpected error: {error}"
        );
    }
}
