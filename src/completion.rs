//! Completion state: the popup's candidates and the prefix they replace.
//!
//! The candidates come from a language server (see [`crate::lsp`]); this module
//! only knows how to keep them in step with what has been typed since the
//! request went out.  Filtering is fuzzy, so a candidate stays reachable after
//! a typo, and the original list is kept so backspacing brings items back.

use crate::buffer::BufferId;
use crate::fuzzy;
use crate::lsp::CompletionItem;

/// How many candidates the popup keeps.
pub const LIMIT: usize = 200;

/// An open completion popup.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Every candidate the server offered, in its order.
    pub all: Vec<CompletionItem>,
    /// Indices into `all` that match the current prefix, best first.
    pub visible: Vec<usize>,
    /// Index into `visible` of the highlighted row.
    pub selected: usize,
    /// Byte offset where the replaced prefix starts.
    pub start: usize,
    /// The request this list answers, so a late answer can be ignored.
    pub request: i64,
    /// The buffer the request was made for.
    pub buffer: BufferId,
}

impl Completion {
    pub fn new(buffer: BufferId, request: i64, start: usize, all: Vec<CompletionItem>) -> Self {
        let mut completion = Self {
            all,
            visible: Vec::new(),
            selected: 0,
            start,
            request,
            buffer,
        };
        completion.refilter("");
        completion
    }

    /// True when there is something to show.
    pub fn is_visible(&self) -> bool {
        !self.visible.is_empty()
    }

    /// The highlighted candidate.
    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.all.get(*index))
    }

    /// Keep only the candidates matching `prefix`, best first.
    ///
    /// The selection follows the item it was on when that item survives, so a
    /// keystroke that narrows the list does not jump to a different candidate.
    pub fn refilter(&mut self, prefix: &str) {
        let previous = self.selected_item().map(|item| item.label.clone());
        let scored = fuzzy::filter_and_sort(&self.all, prefix, true, LIMIT, |item| &item.label);
        self.visible = scored.into_iter().map(|(index, _)| index).collect();
        self.selected = previous
            .and_then(|label| {
                self.visible
                    .iter()
                    .position(|index| self.all[*index].label == label)
            })
            .unwrap_or(0)
            .min(self.visible.len().saturating_sub(1));
    }

    /// Move the highlight, wrapping at the ends.
    pub fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }
}

/// Text inserted when an item is accepted, and where the caret lands.
///
/// Snippet payloads (`insertTextFormat: 2`) carry tab stops like `$0` and
/// `${1:default}`; the editor has no multi-stop snippet sessions, so the first
/// stop wins its placeholders are kept as editable default text — and every
/// other marker is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The literal text to insert.
    pub text: String,
    /// Byte offset into `text` where the caret should end up.
    pub caret: usize,
}

/// Expand LSP snippet syntax into plain text plus a caret position.
///
/// `${1:expr}` keeps `expr` and aims the caret at it; the first numbered stop
/// wins, with `$0` — the exit position — as the fallback aim, matching how
/// snippet sessions start on tab stop 1.  Everything else about the snippet is
/// dropped.  `\$` and `\\` are literal escapes, as the protocol requires.
pub fn expand_snippet(snippet: &str) -> Expanded {
    let mut text = String::with_capacity(snippet.len());
    // Where the caret goes, tracked as a position in `text`.  The first
    // numbered stop wins; `$0` is only the aim when there are no others.
    let mut exit: Option<usize> = None;
    let mut first_stop: Option<usize> = None;
    let mut chars = snippet.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            if character == '\\' {
                // A backslash escapes the next character in a snippet.
                match chars.next() {
                    Some(escaped @ ('$' | '\\')) => text.push(escaped),
                    Some(other) => {
                        text.push('\\');
                        text.push(other);
                    }
                    None => text.push('\\'),
                }
            } else {
                text.push(character);
            }
            continue;
        }
        // A lone '$' that does not start a stop is literal.
        let mark = text.len();
        match chars.peek() {
            Some('0'..='9') => {
                let mut number = String::new();
                while matches!(chars.peek(), Some('0'..='9')) {
                    number.push(chars.next().expect("peeked digit"));
                }
                if number == "0" {
                    exit = Some(mark);
                } else if first_stop.is_none() {
                    first_stop = Some(mark);
                }
            }
            Some('{') => {
                chars.next();
                let mut number = String::new();
                while matches!(chars.peek(), Some('0'..='9')) {
                    number.push(chars.next().expect("peeked digit"));
                }
                if chars.peek() == Some(&':') {
                    chars.next();
                    // `${1:default}` — insert the default and aim at it.
                    let start = text.len();
                    let mut depth = 1usize;
                    for inner in chars.by_ref() {
                        match inner {
                            '{' => {
                                depth += 1;
                                text.push(inner);
                            }
                            '}' if depth > 1 => {
                                depth -= 1;
                                text.push(inner);
                            }
                            '}' => break,
                            other => text.push(other),
                        }
                    }
                    if number == "0" {
                        exit = Some(start);
                    } else if first_stop.is_none() {
                        first_stop = Some(start);
                    }
                } else if number == "0" {
                    exit = Some(mark);
                } else if first_stop.is_none() {
                    first_stop = Some(mark);
                }
            }
            _ => text.push('$'),
        }
    }
    let caret = first_stop.or(exit).unwrap_or(text.len());
    Expanded { text, caret }
}

/// What accepting `item` inserts, and where the caret lands.
///
/// Snippet payloads are expanded to plain text aimed at the first stop.  A
/// macro (label ending in `!`) typed in front of nothing grows its call shape —
/// `println!("");`, or without the semicolon when the line continues — with
/// the caret between the quotes, because a bare macro name is never what the
/// user meant to stop at.  That holds whether the server sent the bare name or
/// rust-analyzer's snippet form `println!($0)`.  `tail` is the rest of the
/// line after the caret.
pub fn accepted(item: &crate::lsp::CompletionItem, tail: &str) -> Expanded {
    let mut expanded = if item.snippet {
        expand_snippet(item.text())
    } else {
        Expanded {
            text: item.text().to_string(),
            caret: item.text().len(),
        }
    };
    // A macro is a name ending in `!` before any call decoration — rust-analyzer
    // labels them `println!` or, with label details, `println!(…)`.
    let is_macro = std::iter::once(item.label.as_str())
        .chain(std::iter::once(item.text()))
        .any(|candidate| {
            candidate
                .split('(')
                .next()
                .is_some_and(|head| head.ends_with('!'))
        });
    if !is_macro || tail.starts_with('(') {
        return expanded;
    }
    let statement = tail.chars().all(char::is_whitespace);
    if expanded.text.ends_with('!') {
        // The bare name: grow the whole call.
        let base = std::mem::take(&mut expanded.text);
        expanded.text = format!("{base}(\"\"{}", if statement { ");" } else { ")" });
        expanded.caret = base.len() + 2;
        return expanded;
    }
    // A snippet that already opened the call: fill its empty argument list.
    if let Some(open) = expanded.text.find("()") {
        let rest = &expanded.text[open + 2..];
        let filled = format!(
            "{}(\"\"{}{}",
            &expanded.text[..open],
            if statement && rest.is_empty() {
                ");"
            } else {
                ")"
            },
            rest
        );
        expanded.caret = open + 2;
        expanded.text = filled;
    }
    expanded
}

/// The identifier-ish run ending at `cursor`, and where it starts.
///
/// A `.` or `::` immediately before the caret also counts as a completion
/// context with an empty prefix: that is how member and path lists are asked
/// for.  A lone `:` does not — it is half of a `::` being typed, and asking
/// about it gets global symbols instead of the module's members.
pub fn prefix_at(buffer: &crate::buffer::Buffer, cursor: usize) -> Option<(usize, String)> {
    let cursor = buffer.clamp_byte(cursor);
    let line_start = buffer.line_start(buffer.line_of(cursor));
    let before = buffer.slice(crate::selection::Span::new(line_start, cursor));
    let mut start = before.len();
    for (offset, character) in before.char_indices().rev() {
        if character.is_alphanumeric() || character == '_' {
            start = offset;
        } else {
            break;
        }
    }
    let prefix = before[start..].to_string();
    if !prefix.is_empty() {
        return Some((line_start + start, prefix));
    }
    // An empty prefix is only interesting right after a member access.  A lone
    // `:` does not count: Rust paths use `::`, and firing on the first colon
    // asks the server about `use std:` — broken syntax it answers with global
    // symbols that then sit in the popup while the second colon is typed.
    if before.ends_with('.') || before.ends_with("::") {
        return Some((cursor, String::new()));
    }
    None
}

/// The word characters immediately before `cursor`, for filtering the popup.
///
/// Unlike [`prefix_at`] this never spans a path or member separator: after
/// `use std::io` the filter text is `io`, and after `std::io::` it is empty.
pub fn filter_prefix(buffer: &crate::buffer::Buffer, cursor: usize) -> String {
    let cursor = buffer.clamp_byte(cursor);
    let line_start = buffer.line_start(buffer.line_of(cursor));
    let before = buffer.slice(crate::selection::Span::new(line_start, cursor));
    let mut start = before.len();
    for (offset, character) in before.char_indices().rev() {
        if character.is_alphanumeric() || character == '_' {
            start = offset;
        } else {
            break;
        }
    }
    before[start..].to_string()
}

/// The trigger character a completion request should report, if any.
///
/// Servers advertise `:` and `.` as triggers; sending `triggerKind: 2` with
/// the character is how a member or path list is properly asked for.
pub fn trigger_char(buffer: &crate::buffer::Buffer, cursor: usize) -> Option<String> {
    let cursor = buffer.clamp_byte(cursor);
    let line_start = buffer.line_start(buffer.line_of(cursor));
    let before = buffer.slice(crate::selection::Span::new(line_start, cursor));
    if before.ends_with('.') {
        return Some(".".to_string());
    }
    if before.ends_with(':') {
        return Some(":".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::config::EditorConfig;

    fn buffer(text: &str) -> Buffer {
        Buffer::new(
            BufferId(1),
            None,
            text.to_string(),
            &EditorConfig::default(),
        )
    }

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: label.to_string(),
            snippet: false,
            kind: None,
            detail: None,
            documentation: None,
            replace: None,
        }
    }

    #[test]
    fn the_prefix_is_the_word_before_the_caret() {
        let buffer = buffer("let value = pri");
        let (start, prefix) = prefix_at(&buffer, buffer.len_bytes()).expect("a prefix");
        assert_eq!(prefix, "pri");
        assert_eq!(&buffer.text()[start..], "pri");
    }

    #[test]
    fn a_member_access_has_an_empty_prefix() {
        let buffer = buffer("value.");
        let (start, prefix) = prefix_at(&buffer, buffer.len_bytes()).expect("a context");
        assert_eq!(prefix, "");
        assert_eq!(start, buffer.len_bytes());
    }

    #[test]
    fn a_path_access_has_an_empty_prefix() {
        let buffer = buffer("use std::io::");
        let (start, prefix) = prefix_at(&buffer, buffer.len_bytes()).expect("a context");
        assert_eq!(prefix, "");
        assert_eq!(start, buffer.len_bytes());
    }

    #[test]
    fn a_lone_colon_is_not_a_completion_context() {
        // The first colon of a `::` being typed asks the server about broken
        // syntax, and it answers with global symbols — not the module list.
        let buffer = buffer("use std:");
        assert_eq!(prefix_at(&buffer, buffer.len_bytes()), None);
    }

    #[test]
    fn the_filter_never_carries_separators() {
        let word = buffer("use std::io");
        assert_eq!(filter_prefix(&word, word.len_bytes()), "io");
        let path = buffer("use std::io::");
        assert_eq!(filter_prefix(&path, path.len_bytes()), "");
    }

    #[test]
    fn a_trigger_character_names_the_separator() {
        let colon = buffer("use std::io:");
        assert_eq!(
            trigger_char(&colon, colon.len_bytes()).as_deref(),
            Some(":")
        );
        let dot = buffer("value.");
        assert_eq!(trigger_char(&dot, dot.len_bytes()).as_deref(), Some("."));
        let word = buffer("value");
        assert_eq!(trigger_char(&word, word.len_bytes()), None);
    }

    fn snippet(label: &str, text: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: text.to_string(),
            snippet: true,
            kind: None,
            detail: None,
            documentation: None,
            replace: None,
        }
    }

    #[test]
    fn a_plain_item_inserts_its_text_at_the_end() {
        let expanded = accepted(&item("Vec"), "");
        assert_eq!(expanded.text, "Vec");
        assert_eq!(expanded.caret, 3);
    }

    #[test]
    fn a_macro_grows_its_call_shape_in_statement_position() {
        let expanded = accepted(&item("println!"), "");
        assert_eq!(expanded.text, "println!(\"\");");
        assert_eq!(&expanded.text[..expanded.caret], "println!(\"");
    }

    #[test]
    fn a_macro_midline_grows_no_semicolon() {
        let expanded = accepted(&item("println!"), " + 1");
        assert_eq!(expanded.text, "println!(\"\")");
        assert_eq!(&expanded.text[..expanded.caret], "println!(\"");
    }

    #[test]
    fn a_macro_already_in_front_of_parens_stays_bare() {
        let expanded = accepted(&item("print!"), "(\"hi\")");
        assert_eq!(expanded.text, "print!");
    }

    #[test]
    fn snippets_expand_to_plain_text_aimed_at_the_first_stop() {
        // rust-analyzer's call snippet: aim at `$1`, inside the parens.
        let expanded = expand_snippet("main($1);$0");
        assert_eq!(expanded.text, "main();");
        assert_eq!(&expanded.text[..expanded.caret], "main(");

        let expanded = expand_snippet("for ${1:i} in 0..${2:n} $0");
        assert_eq!(expanded.text, "for i in 0..n ");
        assert_eq!(&expanded.text[..expanded.caret], "for ");

        // With no numbered stop, `$0` is the aim.
        let expanded = expand_snippet("x$0");
        assert_eq!(expanded.text, "x");
        assert_eq!(expanded.caret, 1);

        // A dollar that starts nothing is literal, and `\$` escapes.
        let expanded = expand_snippet("cost: \\$5$0");
        assert_eq!(expanded.text, "cost: $5");
        assert_eq!(expanded.caret, 8);
    }

    #[test]
    fn a_snippet_item_accepts_through_the_expander() {
        let expanded = accepted(&snippet("main()", "main($0)"), "");
        assert_eq!(expanded.text, "main()");
        assert_eq!(expanded.caret, 5);
    }

    #[test]
    fn a_macro_snippet_grows_its_call_shape_too() {
        // rust-analyzer sends macros as `println!($0)` once snippet support is
        // on; the empty call still gets filled with the caret between quotes.
        let expanded = accepted(&snippet("println!", "println!($0)"), "");
        assert_eq!(expanded.text, "println!(\"\");");
        assert_eq!(&expanded.text[..expanded.caret], "println!(\"");

        let midline = accepted(&snippet("println!", "println!($0)"), " + 1");
        assert_eq!(
            midline.text, "println!(\"\")",
            "no semicolon when the line continues"
        );
        assert_eq!(&midline.text[..midline.caret], "println!(\"");

        // The label-details spelling (`println!(…)`) is a macro as well.
        let detailed = accepted(&snippet("println!(…)", "println!($0)"), "");
        assert_eq!(detailed.text, "println!(\"\");");
    }

    #[test]
    fn whitespace_is_not_a_completion_context() {
        let buffer = buffer("let a = ");
        assert_eq!(prefix_at(&buffer, buffer.len_bytes()), None);
    }

    #[test]
    fn filtering_narrows_and_backspacing_restores() {
        let mut completion = Completion::new(
            BufferId(1),
            1,
            0,
            vec![item("print"), item("println"), item("push")],
        );
        assert_eq!(completion.visible.len(), 3);
        completion.refilter("prin");
        assert_eq!(completion.visible.len(), 2);
        completion.refilter("p");
        assert_eq!(completion.visible.len(), 3, "the list comes back");
    }

    #[test]
    fn the_selection_follows_its_item() {
        let mut completion = Completion::new(
            BufferId(1),
            1,
            0,
            vec![item("alpha"), item("beta"), item("gamma")],
        );
        completion.move_selection(1);
        assert_eq!(
            completion.selected_item().map(|i| i.label.as_str()),
            Some("beta")
        );
        completion.refilter("a");
        assert_eq!(
            completion.selected_item().map(|i| i.label.as_str()),
            Some("beta"),
            "beta still matches, so it stays selected"
        );
    }
}
