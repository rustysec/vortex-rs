//! Operators: the edits that motions and text objects feed into.
//!
//! Every function here is pure — it turns a buffer plus a [`Region`] into a list
//! of [`Change`]s.  The editor applies them, which is what records undo data.

use crate::buffer::{Buffer, Change, char_width, next_char_boundary};
use crate::selection::Span;

/// The bytes an operator will edit.
///
/// `spans` are disjoint and ascending; a block selection contributes one span
/// per line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub spans: Vec<Span>,
    /// Whole lines: yanks remember this so `p` can paste below the cursor line.
    pub linewise: bool,
}

impl Region {
    pub fn char(span: Span) -> Self {
        Self {
            spans: vec![span],
            linewise: false,
        }
    }

    pub fn line(span: Span) -> Self {
        Self {
            spans: vec![span],
            linewise: true,
        }
    }

    pub fn block(spans: Vec<Span>) -> Self {
        Self {
            spans,
            linewise: false,
        }
    }

    pub fn empty() -> Self {
        Self {
            spans: Vec::new(),
            linewise: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.is_empty())
    }

    /// Byte range covering the whole region.
    pub fn extent(&self) -> Span {
        let start = self.spans.first().map(|s| s.start).unwrap_or(0);
        let end = self.spans.last().map(|s| s.end).unwrap_or(start);
        Span::new(start, end)
    }

    pub fn first(&self) -> Option<Span> {
        self.spans.first().copied()
    }

    /// The text the operator will consume.
    pub fn text(&self, buffer: &Buffer) -> String {
        if self.spans.len() == 1 {
            return buffer.slice(self.spans[0]);
        }
        self.spans
            .iter()
            .map(|span| buffer.slice(*span))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The region a motion covers, starting from `from`.
pub fn region_for_motion(
    buffer: &Buffer,
    from: usize,
    cursor: usize,
    linewise: bool,
    inclusive: bool,
) -> Region {
    if linewise {
        let first = buffer.line_of(from.min(cursor));
        let last = buffer.line_of(from.max(cursor));
        return Region::line(lines_span(buffer, first, last));
    }
    let (start, end) = if cursor >= from {
        let end = if inclusive {
            next_char_boundary(&buffer.rope, cursor)
        } else {
            cursor
        };
        (from, end)
    } else {
        let end = if inclusive {
            next_char_boundary(&buffer.rope, from)
        } else {
            from
        };
        (cursor, end)
    };
    Region::char(Span::new(start, end))
}

/// Full lines from `first` to `last`, including line breaks where possible.
pub fn lines_span(buffer: &Buffer, first: usize, last: usize) -> Span {
    let mut start = buffer.line_start(first);
    let end = if last + 1 < buffer.line_count() {
        buffer.line_start(last + 1)
    } else {
        buffer.len_bytes()
    };
    // Deleting the final line, which has no line break of its own, should take
    // the break before it so no empty line is left behind.
    let ends_with_newline = buffer.slice(Span::new(end.saturating_sub(1), end)) == "\n";
    if end >= buffer.len_bytes() && !ends_with_newline && start > 0 {
        if let Some((offset, '\n')) = buffer.char_before(start) {
            start = offset;
        }
    }
    Span::new(start, end)
}

/// Widen a selection to whole lines.
pub fn expand_linewise(buffer: &Buffer, span: Span) -> Span {
    lines_span(
        buffer,
        buffer.line_of(span.start),
        buffer.line_of(span.end.max(span.start)),
    )
}

/// Rectangular spans of a block selection between `anchor` and `cursor`.
pub fn block_spans(buffer: &Buffer, anchor: usize, cursor: usize) -> Vec<Span> {
    let (first_line, last_line) = {
        let a = buffer.line_of(anchor);
        let b = buffer.line_of(cursor);
        (a.min(b), a.max(b))
    };
    let anchor_col = buffer.display_col(anchor);
    let cursor_col = buffer.display_col(cursor);
    let (start_col, end_col) = (anchor_col.min(cursor_col), anchor_col.max(cursor_col));
    // A block selection covers whole cells, so the right edge grows by one.
    let end_col = end_col + 1;
    let mut spans = Vec::new();
    for line in first_line..=last_line {
        let start = buffer.byte_at_display_col(line, start_col);
        let end = buffer.byte_at_display_col(line, end_col);
        if end > start {
            spans.push(Span::new(start, end));
        } else if start == end {
            spans.push(Span::point(start));
        }
    }
    spans
}

// ------------------------------------------------------------------ builders

/// Delete the region.
pub fn delete_changes(region: &Region) -> Vec<Change> {
    let mut changes = Vec::with_capacity(region.spans.len());
    for span in &region.spans {
        if !span.is_empty() {
            changes.push(Change::delete(*span));
        }
    }
    changes
}

/// Replace the region with `text`.
pub fn replace_changes(region: &Region, text: &str) -> Vec<Change> {
    region
        .spans
        .iter()
        .filter(|span| !span.is_empty() || !text.is_empty())
        .map(|span| Change::replace(*span, text))
        .collect()
}

/// `cc` keeps the line's indentation so typing continues the line.
pub fn change_region(buffer: &Buffer, region: &Region) -> (Vec<Change>, usize) {
    if region.linewise {
        let extent = region.extent();
        let first_line = buffer.line_of(extent.start);
        let indent = buffer.indent_str(first_line);
        // `cc` empties the line but keeps it a line: the break stays unless the
        // region ran to the end of the buffer without one.
        let kept = if buffer.slice(extent).ends_with('\n') {
            format!("{indent}\n")
        } else {
            indent.clone()
        };
        let changes = vec![Change::replace(extent, kept)];
        return (changes, extent.start + indent.len());
    }
    let changes = region
        .spans
        .iter()
        .map(|span| Change::replace(*span, ""))
        .collect();
    (changes, region.extent().start)
}

/// `r{char}`: overwrite every character in the region.
pub fn replace_char_changes(buffer: &Buffer, region: &Region, replacement: char) -> Vec<Change> {
    let mut changes = Vec::new();
    for span in &region.spans {
        let text = buffer.slice(*span);
        if text.is_empty() {
            continue;
        }
        let replaced: String = text
            .chars()
            .map(|ch| if ch == '\n' { '\n' } else { replacement })
            .collect();
        changes.push(Change::replace(*span, replaced));
    }
    changes
}

/// `J`: join `lines` lines (at least two) starting at the region's first line.
pub fn join_changes(buffer: &Buffer, region: &Region, lines: usize) -> Vec<Change> {
    let extent = region.extent();
    let start_line = buffer.line_of(extent.start);
    let last = buffer.line_count() - 1;
    let mut changes = Vec::new();
    for offset in 1..lines.max(2) {
        let line = start_line + offset;
        if line > last {
            break;
        }
        let previous_end = buffer.line_end(line - 1);
        let next_first = buffer.first_non_blank(line);
        let previous = buffer.line_str(line - 1);
        let next = buffer.line_str(line);
        // No space after an opening bracket or before a closing one.
        let needs_space = !previous.is_empty()
            && !previous.ends_with(['(', '[', '{'])
            && !next.trim_start().starts_with(')');
        changes.push(Change::replace(
            Span::new(previous_end, next_first),
            if needs_space { " " } else { "" },
        ));
    }
    changes
}

/// `>` / `<`: shift the region by `count` levels.
pub fn indent_changes(
    buffer: &Buffer,
    region: &Region,
    direction: i32,
    count: usize,
    expand_tab: bool,
    tab_width: usize,
) -> Vec<Change> {
    let extent = region.extent();
    let first = buffer.line_of(extent.start);
    let last = buffer.line_of(extent.end.max(extent.start));
    let unit = if expand_tab {
        " ".repeat(tab_width.max(1))
    } else {
        "\t".to_string()
    };
    let mut changes = Vec::new();
    for line in first..=last {
        if direction > 0 {
            let start = buffer.line_start(line);
            for _ in 0..count.max(1) {
                changes.push(Change::insert(start, unit.clone()));
            }
        } else {
            let start = buffer.line_start(line);
            let end = buffer.line_end(line);
            let text = buffer.slice(Span::new(start, end));
            let mut remove = 0;
            let mut columns = 0;
            for ch in text.chars() {
                if columns >= tab_width * count.max(1) {
                    break;
                }
                match ch {
                    ' ' => {
                        remove += 1;
                        columns += 1;
                    }
                    '\t' => {
                        remove += 1;
                        columns += char_width(ch, tab_width.max(1), columns);
                    }
                    _ => break,
                }
            }
            if remove > 0 {
                changes.push(Change::delete(Span::new(start, start + remove)));
            }
        }
    }
    changes
}

/// `=`: reindent using bracket depth.  (A configured formatter handles the whole
/// buffer asynchronously; this keeps `=` useful for snippets.)
pub fn reindent_changes(buffer: &Buffer, region: &Region, tab_width: usize) -> Vec<Change> {
    let extent = region.extent();
    let first = buffer.line_of(extent.start);
    let last = buffer.line_of(extent.end.max(extent.start));
    let depths = bracket_depths(buffer, last);
    let mut changes = Vec::new();
    for line in first..=last {
        let line_text = buffer.line_str(line);
        let trimmed = line_text.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let depth = depths.get(line).copied().unwrap_or(0);
        // A line that opens with a closing bracket dedents itself.
        let extra = if trimmed.starts_with(['}', ')', ']']) {
            -1
        } else {
            0
        };
        let want = (depth + extra).max(0) as usize * tab_width.max(1);
        if buffer.indent_width(line) == want {
            continue;
        }
        let start = buffer.line_start(line);
        let content = buffer.first_non_blank(line);
        changes.push(Change::replace(Span::new(start, content), " ".repeat(want)));
    }
    changes
}

/// Bracket nesting depth at the start of each line up to `last_line`.
///
/// String literals and `//` comments are skipped so brackets inside them do not
/// move the indentation.
fn bracket_depths(buffer: &Buffer, last_line: usize) -> Vec<i32> {
    let mut depths = vec![0i32; last_line + 1];
    let mut stack: Vec<char> = Vec::new();
    let mut line = 0usize;
    let mut at_line_start = true;
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    let mut mut_line_comment = false;
    let mut previous: Option<char> = None;

    for ch in buffer.text().chars() {
        if ch == '\n' {
            line += 1;
            at_line_start = true;
            in_string = None;
            escaped = false;
            mut_line_comment = false;
            previous = None;
            if line > last_line {
                break;
            }
            continue;
        }
        if at_line_start {
            depths[line] = stack.len() as i32;
            at_line_start = false;
        }
        if mut_line_comment {
            previous = Some(ch);
            continue;
        }
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = None;
            }
            previous = Some(ch);
            continue;
        }
        match ch {
            '/' if previous == Some('/') => {
                mut_line_comment = true;
                continue;
            }
            '"' | '\'' => in_string = Some(ch),
            '(' | '[' | '{' => stack.push(ch),
            ')' | ']' | '}' => {
                stack.pop();
            }
            _ => {}
        }
        previous = Some(ch);
    }
    depths
}

/// `~`, `U`, `u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Swap,
    Upper,
    Lower,
}

pub fn case_changes(buffer: &Buffer, region: &Region, case: Case) -> Vec<Change> {
    let mut changes = Vec::new();
    for span in &region.spans {
        let text = buffer.slice(*span);
        if text.is_empty() {
            continue;
        }
        let converted: String = match case {
            Case::Swap => text
                .chars()
                .flat_map(|c| {
                    let converted: Vec<char> = if c.is_uppercase() {
                        c.to_lowercase().collect()
                    } else {
                        c.to_uppercase().collect()
                    };
                    converted
                })
                .collect(),
            Case::Upper => text.to_uppercase(),
            Case::Lower => text.to_lowercase(),
        };
        if converted != text {
            changes.push(Change::replace(*span, converted));
        }
    }
    changes
}

/// `gc`: comment the region in or out.
pub fn comment_changes(buffer: &Buffer, region: &Region, token: &str) -> Vec<Change> {
    if token.is_empty() {
        return Vec::new();
    }
    let extent = region.extent();
    let first = buffer.line_of(extent.start);
    let last = buffer.line_of(extent.end.max(extent.start));
    let lines: Vec<usize> = (first..=last)
        .filter(|line| !buffer.is_line_blank(*line))
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let commented = lines
        .iter()
        .all(|line| buffer.line_str(*line).trim_start().starts_with(token));

    let mut changes = Vec::new();
    for line in lines {
        let at = buffer.first_non_blank(line);
        if commented {
            let trimmed = buffer.line_str(line).trim_start().to_string();
            let after = &trimmed[token.len()..];
            let extra = usize::from(after.starts_with(' '));
            changes.push(Change::delete(Span::new(at, at + token.len() + extra)));
        } else {
            changes.push(Change::insert(at, format!("{token} ")));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EditorConfig;

    fn sample(text: &str) -> Buffer {
        Buffer::new(
            crate::buffer::BufferId(1),
            None,
            text.to_string(),
            &EditorConfig::default(),
        )
    }

    /// Apply changes to a copy of the sample buffer and return the new text.
    fn apply_changes(source: &Buffer, changes: Vec<Change>) -> String {
        let mut buffer = sample(&source.text());
        let mut selection = crate::selection::Selection::point(0);
        buffer.apply(changes, crate::buffer::ChangeGroup::Command, &mut selection);
        buffer.text()
    }

    #[test]
    fn linewise_region_covers_line_breaks() {
        let buffer = sample("one\ntwo\nthree");
        let span = lines_span(&buffer, 0, 1);
        assert_eq!(buffer.slice(span), "one\ntwo\n");
        // The last line has no line break of its own, so the break *before* it
        // goes with it — otherwise `dd` would leave an empty line behind.
        let span = lines_span(&buffer, 2, 2);
        assert_eq!(span.start, 7);
        assert_eq!(span.end, buffer.len_bytes());
        assert_eq!(buffer.slice(span), "\nthree");
    }

    #[test]
    fn char_region_respects_inclusivity() {
        let buffer = sample("hello world");
        let inclusive = region_for_motion(&buffer, 0, 4, false, true);
        assert_eq!(inclusive.text(&buffer), "hello");
        let exclusive = region_for_motion(&buffer, 0, 5, false, false);
        assert_eq!(exclusive.text(&buffer), "hello");
        let backward = region_for_motion(&buffer, 5, 0, false, false);
        assert_eq!(backward.text(&buffer), "hello");
    }

    #[test]
    fn change_linewise_keeps_indentation() {
        let buffer = sample("  let a = 1;\nnext");
        let region = Region::line(lines_span(&buffer, 0, 0));
        let (changes, cursor) = change_region(&buffer, &region);
        let text = apply_changes(&buffer, changes);
        assert_eq!(text, "  \nnext");
        assert_eq!(cursor, 2, "cursor sits after the indentation");
    }

    #[test]
    fn replace_char_overwrites_every_char_in_region() {
        let buffer = sample("hello");
        let region = Region::char(Span::new(0, 5));
        let text = apply_changes(&buffer, replace_char_changes(&buffer, &region, 'x'));
        assert_eq!(text, "xxxxx");
    }

    #[test]
    fn join_lines_adds_a_space() {
        let buffer = sample("alpha\nbeta\ngamma");
        let region = Region::char(Span::point(0));
        let changes = join_changes(&buffer, &region, 3);
        assert_eq!(apply_changes(&buffer, changes), "alpha beta gamma");
    }

    #[test]
    fn join_after_open_bracket_has_no_space() {
        let buffer = sample("f(\n1)\n");
        let region = Region::char(Span::point(0));
        let changes = join_changes(&buffer, &region, 2);
        assert_eq!(apply_changes(&buffer, changes), "f(1)\n");
    }

    #[test]
    fn indent_and_unindent() {
        let buffer = sample("one\ntwo");
        let region = Region::line(lines_span(&buffer, 0, 1));
        let changes = indent_changes(&buffer, &region, 1, 1, true, 4);
        let text = apply_changes(&buffer, changes);
        assert_eq!(text, "    one\n    two");

        let buffer = sample("    one\n    two");
        let region = Region::line(lines_span(&buffer, 0, 1));
        let changes = indent_changes(&buffer, &region, -1, 1, true, 4);
        let text = apply_changes(&buffer, changes);
        assert_eq!(text, "one\ntwo");
    }

    #[test]
    fn unindent_stops_at_the_first_non_whitespace() {
        let buffer = sample("  x\n\ty");
        let region = Region::line(lines_span(&buffer, 0, 1));
        let changes = indent_changes(&buffer, &region, -1, 1, true, 4);
        let text = apply_changes(&buffer, changes);
        assert_eq!(text, "x\ny");
    }

    #[test]
    fn reindent_follows_bracket_depth() {
        let text = "fn f() {\nlet x = 1;\n}\n";
        let buffer = sample(text);
        let region = Region::line(lines_span(&buffer, 0, 2));
        let changes = reindent_changes(&buffer, &region, 4);
        let result = apply_changes(&buffer, changes);
        assert_eq!(result, "fn f() {\n    let x = 1;\n}\n");
    }

    #[test]
    fn case_operators() {
        let buffer = sample("Hello World");
        let region = Region::char(Span::new(0, 5));
        let changes = case_changes(&buffer, &region, Case::Swap);
        assert_eq!(apply_changes(&buffer, changes), "hELLO World");

        let buffer = sample("Hello World");
        let region = Region::char(Span::new(0, 11));
        let changes = case_changes(&buffer, &region, Case::Upper);
        assert_eq!(apply_changes(&buffer, changes), "HELLO WORLD");
    }

    #[test]
    fn comment_toggle_round_trip() {
        let buffer = sample("let a = 1;\nlet b = 2;");
        let region = Region::line(lines_span(&buffer, 0, 1));
        let changes = comment_changes(&buffer, &region, "//");
        let commented = apply_changes(&buffer, changes);
        assert_eq!(commented, "// let a = 1;\n// let b = 2;");

        let buffer = sample(&commented);
        let region = Region::line(lines_span(&buffer, 0, 1));
        let changes = comment_changes(&buffer, &region, "//");
        assert_eq!(apply_changes(&buffer, changes), "let a = 1;\nlet b = 2;");
    }

    #[test]
    fn comment_toggle_skips_blank_lines() {
        let buffer = sample("a\n\nb");
        let region = Region::line(lines_span(&buffer, 0, 2));
        let text = apply_changes(&buffer, comment_changes(&buffer, &region, "#"));
        assert_eq!(text, "# a\n\n# b");
    }

    #[test]
    fn block_spans_cover_a_rectangle() {
        let buffer = sample("abcd\nefgh\nijkl");
        let anchor = buffer.text().find('b').unwrap();
        let cursor = buffer.text().find('j').unwrap();
        let spans = block_spans(&buffer, anchor, cursor);
        let pieces: Vec<String> = spans.iter().map(|span| buffer.slice(*span)).collect();
        assert_eq!(pieces, vec!["b", "f", "j"]);
    }

    #[test]
    fn operator_names_are_stable() {
        // `Pending::echo` and the help output depend on these names.
        assert_eq!(
            crate::input::operator_status(crate::action::Operator::Delete),
            "d"
        );
        assert_eq!(
            crate::input::operator_status(crate::action::Operator::ToggleComment),
            "gc"
        );
    }
}
