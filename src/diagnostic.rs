//! Diagnostics: the errors, warnings and hints that decorate text.
//!
//! Produced by the (scaffolded) language client and the spell checker; the
//! status area shows a rollup of the counts.

use std::fmt;

use crate::selection::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn tag(self) -> char {
        match self {
            Severity::Hint => 'h',
            Severity::Info => 'i',
            Severity::Warning => 'w',
            Severity::Error => 'e',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Hint => "hint",
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// Where a diagnostic came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticSource {
    Lsp,
    Spell,
    Format,
}

/// What kind of "this is not an error" hint a diagnostic carries.
///
/// LSP reports these out of band in `tags`, and they are what makes "unused
/// variable" read differently from "type mismatch" without reading the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticTag {
    /// `tags: [1]` — the code is unused and could go.
    Unnecessary,
    /// `tags: [2]` — the code is deprecated.
    Deprecated,
}

impl DiagnosticTag {
    /// Decode one LSP `DiagnosticTag` number.
    pub fn from_lsp(value: u64) -> Option<Self> {
        match value {
            1 => Some(DiagnosticTag::Unnecessary),
            2 => Some(DiagnosticTag::Deprecated),
            _ => None,
        }
    }
}

/// One problem at a byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Span,
    pub severity: Severity,
    pub message: String,
    pub source: DiagnosticSource,
    /// Fixes/suggestions, used by the `z=` spelling picker.
    pub suggestions: Vec<String>,
    /// Server-assigned code (`E0308`, `unused_variables`), shown in the picker.
    pub code: Option<String>,
    /// `unnecessary` / `deprecated` hints, which change how the span is drawn.
    pub tags: Vec<DiagnosticTag>,
    /// Other places the message refers to, as `(message, span)`.
    pub related: Vec<(String, Span)>,
}

impl Diagnostic {
    pub fn new(
        range: impl Into<Span>,
        severity: Severity,
        message: impl Into<String>,
        source: DiagnosticSource,
    ) -> Self {
        Self {
            range: range.into(),
            severity,
            message: message.into(),
            source,
            suggestions: Vec::new(),
            code: None,
            tags: Vec::new(),
            related: Vec::new(),
        }
    }

    pub fn with_suggestions(mut self, suggestions: Vec<String>) -> Self {
        self.suggestions = suggestions;
        self
    }

    pub fn with_code(mut self, code: Option<String>) -> Self {
        self.code = code;
        self
    }

    pub fn with_tags(mut self, tags: Vec<DiagnosticTag>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_related(mut self, related: Vec<(String, Span)>) -> Self {
        self.related = related;
        self
    }

    pub fn has_tag(&self, tag: DiagnosticTag) -> bool {
        self.tags.contains(&tag)
    }

    /// A one-line headline: `error[E0308]: mismatched types`.
    ///
    /// The code is what a user pastes into a search engine, so it belongs next
    /// to the severity rather than buried in the message.
    pub fn headline(&self) -> String {
        let mut out = self.severity.label().to_string();
        if let Some(code) = &self.code {
            out.push('[');
            out.push_str(code);
            out.push(']');
        }
        out.push_str(": ");
        out.push_str(&self.message);
        out
    }
}

/// Counts per severity, for the status area rollup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiagnosticCount {
    pub error: usize,
    pub warning: usize,
    pub info: usize,
    pub hint: usize,
}

impl DiagnosticCount {
    pub fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut count = Self::default();
        for diagnostic in diagnostics {
            match diagnostic.severity {
                Severity::Error => count.error += 1,
                Severity::Warning => count.warning += 1,
                Severity::Info => count.info += 1,
                Severity::Hint => count.hint += 1,
            }
        }
        count
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn total(&self) -> usize {
        self.error + self.warning + self.info + self.hint
    }

    /// `"2e 1w"` — empty when there is nothing to report.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (value, tag) in [
            (self.error, 'e'),
            (self.warning, 'w'),
            (self.info, 'i'),
            (self.hint, 'h'),
        ] {
            if value > 0 {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&value.to_string());
                out.push(tag);
            }
        }
        out
    }
}

impl fmt::Display for DiagnosticCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_counts_by_severity() {
        let diagnostics = vec![
            Diagnostic::new(Span::point(0), Severity::Error, "x", DiagnosticSource::Lsp),
            Diagnostic::new(Span::point(1), Severity::Error, "y", DiagnosticSource::Lsp),
            Diagnostic::new(
                Span::point(2),
                Severity::Warning,
                "z",
                DiagnosticSource::Spell,
            ),
        ];
        let count = DiagnosticCount::from_diagnostics(&diagnostics);
        assert_eq!(count.summary(), "2e 1w");
        assert_eq!(count.total(), 3);
        assert!(DiagnosticCount::default().summary().is_empty());
    }

    #[test]
    fn the_headline_carries_the_severity_and_the_code() {
        let diagnostic = Diagnostic::new(
            Span::point(0),
            Severity::Error,
            "mismatched types",
            DiagnosticSource::Lsp,
        )
        .with_code(Some("E0308".into()));
        assert_eq!(diagnostic.headline(), "error[E0308]: mismatched types");

        // No code: the severity still leads, without an empty bracket pair.
        let plain = Diagnostic::new(
            Span::point(0),
            Severity::Warning,
            "unused variable",
            DiagnosticSource::Lsp,
        );
        assert_eq!(plain.headline(), "warning: unused variable");
    }
}
