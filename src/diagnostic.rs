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

/// One problem at a byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Span,
    pub severity: Severity,
    pub message: String,
    pub source: DiagnosticSource,
    /// Fixes/suggestions, used by the `z=` spelling picker.
    pub suggestions: Vec<String>,
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
        }
    }

    pub fn with_suggestions(mut self, suggestions: Vec<String>) -> Self {
        self.suggestions = suggestions;
        self
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
}
