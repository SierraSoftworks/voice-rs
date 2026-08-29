//! Source-located grammar diagnostics, rendered by ariadne.
//!
//! Every problem a grammar has — lexical, grammatical, or found by static
//! analysis — is a [`Diagnostic`] tied to a byte range of the source.
//! Diagnostics accumulate across all three stages so one load reports every
//! problem, and [`user_error`] folds them into the crate's single error type
//! at the boundary.

use std::io;

use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, Source};
use chumsky::{error::RichReason, prelude::Rich, span::SimpleSpan};

use super::ast::Span;

/// The stage that produced a diagnostic, and its severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    /// The source could not be tokenized or parsed. An error.
    Syntax,
    /// Static analysis found a problem the grammar cannot load with. An error.
    Analysis,
    /// Static analysis found something suspicious the grammar can still load
    /// with. A warning.
    Lint,
}

impl DiagnosticKind {
    /// Whether this kind of diagnostic prevents the grammar from loading.
    pub fn is_error(self) -> bool {
        !matches!(self, Self::Lint)
    }
}

/// An actionable problem tied to a byte range of the grammar source.
///
/// The message carries all dynamic detail (names, suggestions) in the crate's
/// second-person voice; `help` is the worked example or next step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
}

impl Diagnostic {
    /// Creates a syntax diagnostic.
    pub(super) fn syntax(message: impl Into<String>, span: Span) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            message: message.into(),
            span,
            help: None,
        }
    }

    /// Creates a static-analysis error diagnostic.
    pub(super) fn analysis(message: impl Into<String>, span: Span) -> Self {
        Self {
            kind: DiagnosticKind::Analysis,
            message: message.into(),
            span,
            help: None,
        }
    }

    /// Creates a lint (warning) diagnostic.
    pub(super) fn lint(message: impl Into<String>, span: Span) -> Self {
        Self {
            kind: DiagnosticKind::Lint,
            message: message.into(),
            span,
            help: None,
        }
    }

    /// Attaches recovery guidance — the worked example, the next step.
    #[must_use]
    pub(super) fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Renders the diagnostic with its source excerpt as plain (ANSI-free)
    /// UTF-8 text.
    pub fn render(&self, source: &str) -> String {
        let mut output = Vec::new();
        match self.write(source, &mut output) {
            Ok(()) => String::from_utf8_lossy(&output).into_owned(),
            // Rendering can only fail on a writer error, which a Vec never
            // produces — but a diagnostic must never eat itself, so fall back
            // to the bare message.
            Err(_) => self.message.clone(),
        }
    }

    /// Writes the ariadne report for this diagnostic to `writer`.
    fn write(&self, source: &str, writer: impl io::Write) -> io::Result<()> {
        let kind = match self.kind {
            DiagnosticKind::Lint => ReportKind::Warning,
            _ => ReportKind::Error,
        };
        let start = self.span.start.min(source.len());
        let end = self.span.end.max(start.saturating_add(1)).min(source.len());
        let range = start..end.max(start);

        let mut report = Report::build(kind, ((), range.clone()))
            .with_config(
                Config::new()
                    .with_color(false)
                    .with_index_type(IndexType::Byte),
            )
            .with_message(&self.message)
            .with_label(
                Label::new(((), range))
                    .with_message(&self.message)
                    .with_color(Color::Red),
            );
        if let Some(help) = &self.help {
            report = report.with_help(help);
        }
        report.finish().write(Source::from(source), writer)
    }
}

/// Converts a chumsky [`Rich`] error into a syntax diagnostic.
///
/// Custom messages (ours, already in the house voice) pass through verbatim;
/// chumsky's own expected/found failures are rephrased in second person.
pub(super) fn from_rich<T: std::fmt::Display>(
    error: &Rich<'_, T, SimpleSpan<usize>>,
) -> Diagnostic {
    let span = Span::new(error.span().start, error.span().end);

    let message = match error.reason() {
        RichReason::Custom(message) => message.clone(),
        _ => {
            let found = error
                .found()
                .map(|token| format!("'{token}'"))
                .unwrap_or_else(|| "the end of the grammar".to_owned());

            let mut expected: Vec<String> = error
                .expected()
                .map(|pattern| pattern.to_string())
                .collect();
            expected.sort();
            expected.dedup();

            match expected.len() {
                0 => format!("We didn't expect to find {found} here."),
                1 => format!("We found {found} here, where {} was expected.", expected[0]),
                _ => format!(
                    "We found {found} here, where one of {} was expected.",
                    expected.join(", ")
                ),
            }
        }
    };

    Diagnostic::syntax(message, span)
}

/// Folds a non-empty set of diagnostics into the crate's single error type.
///
/// The message carries every rendered report (with its source excerpt); the
/// advice stays static, as `human_errors` requires.
pub fn user_error(diagnostics: &[Diagnostic], source: &str) -> crate::Error {
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind.is_error())
        .count();
    let problems = match errors {
        1 => "a problem".to_owned(),
        n => format!("{n} problems"),
    };

    let reports = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.render(source))
        .collect::<Vec<_>>()
        .join("\n");

    human_errors::user(
        format!("Your grammar has {problems} we couldn't load past.\n\n{reports}"),
        &[
            "Each report above points at the exact place in your grammar it describes — fix them from the top down and load again.",
            "A grammar is a list of rules like: Map = \"map\" | \"toggle map\" { m } — see the grammar reference in the documentation.",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_is_plain_text_with_the_excerpt() {
        let source = "Map = \"map\" { unknownkey }";
        let diagnostic = Diagnostic::analysis(
            "We don't recognize 'unknownkey' as a key name.",
            Span::new(14, 24),
        )
        .with_help("Key names are the lowercase evdev key names.");

        let rendered = diagnostic.render(source);
        assert!(rendered.contains("Error"), "got: {rendered}");
        assert!(rendered.contains("unknownkey"), "got: {rendered}");
        assert!(rendered.contains("Help"), "got: {rendered}");
        assert!(
            !rendered.contains('\u{1b}'),
            "the report must be ANSI-free: {rendered:?}"
        );
    }

    #[test]
    fn test_lints_render_as_warnings() {
        let diagnostic = Diagnostic::lint("Nothing refers to this rule.", Span::new(0, 4));
        assert!(diagnostic.render("dead = \"x\"").contains("Warning"));
        assert!(!diagnostic.kind.is_error());
    }

    #[test]
    fn test_out_of_range_spans_do_not_panic() {
        let diagnostic = Diagnostic::syntax("We ran out of grammar.", Span::new(90, 99));
        let rendered = diagnostic.render("short");
        assert!(rendered.contains("We ran out of grammar."), "{rendered}");
    }

    #[test]
    fn test_user_error_counts_errors_and_carries_reports() {
        let source = "a = b";
        let diagnostics = vec![
            Diagnostic::analysis("You're referring to a rule called 'b'.", Span::new(4, 5)),
            Diagnostic::lint("Nothing refers to the private rule 'a'.", Span::new(0, 1)),
        ];

        let error = user_error(&diagnostics, source);
        let message = error.to_string();
        assert!(
            message.contains("a problem"),
            "one error should be counted in the singular: {message}"
        );
        assert!(message.contains("referring to a rule"), "{message}");
        assert!(message.contains("Nothing refers"), "{message}");
    }
}
