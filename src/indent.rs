//! Syntax-aware indentation while typing.
//!
//! The editor stays usable without a grammar, so everything here has a
//! text-only answer: a line that ends in an opening bracket (or a block colon)
//! indents the next one, and a closing bracket alone on its line lines up with
//! the block it closes.  When the tree-sitter tree is current it is consulted
//! for the dedent, because it knows which bracket really opened the block and
//! ignores brackets that are text inside a string or a comment.
//!
//! All functions are pure: they read a [`Buffer`] and return the text to insert,
//! so the behaviour can be tested without a terminal or a running parser.

use crate::buffer::Buffer;
use crate::selection::Span;
use crate::syntax;

/// One indentation level.
pub fn unit(expand_tab: bool, tab_width: usize) -> String {
    if expand_tab {
        " ".repeat(tab_width.max(1))
    } else {
        "\t".to_string()
    }
}

/// What pressing enter at a position should insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newline {
    /// Text to insert at the caret.
    pub text: String,
    /// Caret offset from the start of `text`.
    pub caret: usize,
}

/// The newline (and indentation) that belongs at `at`.
///
/// Five cases matter:
/// * the caret sits in a comment — the new line continues the comment marker;
/// * the caret sits inside an open `(` or `[` — the new line lines up with the
///   first argument, or one level in when the opener ends its line;
/// * the line opens a block — the new line is indented one level;
/// * the line opens a block *and* a closing bracket follows — the bracket keeps
///   its own line, dedented back to the block's level;
/// * anything else — the new line keeps the current indentation.
pub fn newline(buffer: &Buffer, at: usize, expand_tab: bool, tab_width: usize) -> Newline {
    let at = buffer.clamp_byte(at);

    // A comment continues itself: `//` stays `//`, `///` stays `///`, and a
    // block comment keeps its ` * ` gutter.  This wins over every other rule —
    // a `(` inside a comment is text, not a call.
    if let Some(comment) = comment_continuation(buffer, at) {
        let text = format!("\n{comment}");
        return Newline {
            caret: text.len(),
            text,
        };
    }

    let line = buffer.line_of(at);
    let line_start = buffer.line_start(line);
    let line_end = buffer.line_end(line);
    let before = buffer.slice(Span::new(line_start, at));
    let after = buffer.slice(Span::new(at, line_end));

    // The indentation the new line inherits.  When the caret sits in the middle
    // of a line, the whitespace before it is the indentation that matters; the
    // line's own leading whitespace is the fallback.
    let base = if before.chars().all(|c| c == ' ' || c == '\t') {
        before.clone()
    } else {
        buffer.indent_str(line)
    };

    // A call or list that is still open hangs its continuation lines under the
    // first argument.  This is checked before the block rule because `(` and
    // `[` open a block as far as `opens_block` is concerned, and one level in
    // is the wrong answer for `foo(a,` — the arguments should line up.
    let continuation = continuation_indent(buffer, at, expand_tab, tab_width);
    let opens = continuation.is_none() && opens_block(&before);
    let closes = after.trim_start().starts_with(['}', ')', ']']);
    let inner = match &continuation {
        Some(indent) => indent.clone(),
        None if opens => format!("{base}{}", unit(expand_tab, tab_width)),
        None => base.clone(),
    };

    if (opens || continuation.is_some()) && closes {
        // `{|}` + enter: the closing bracket moves to its own line, and the
        // caret lands on the indented line between the two.
        let text = format!("\n{inner}\n{base}");
        let caret = 1 + inner.len();
        Newline { text, caret }
    } else {
        let text = format!("\n{inner}");
        let caret = text.len();
        Newline { text, caret }
    }
}

/// The text a new line should carry when the caret sits in a comment.
///
/// `None` when the caret is not in a comment.  Enter on an *empty* line comment
/// (`//` with nothing after it) returns just the indentation, which is how a
/// comment is ended without deleting the marker by hand.
pub fn comment_continuation(buffer: &Buffer, at: usize) -> Option<String> {
    let at = buffer.clamp_byte(at);
    let line = buffer.line_of(at);
    let line_start = buffer.line_start(line);
    let before = buffer.slice(Span::new(line_start, at));
    let indent = buffer.indent_str(line);
    let body = before.trim_start();

    // A line comment: keep the marker, including `///` and `//!`.
    let token = buffer.language.line_comment;
    if !token.is_empty()
        && let Some(rest) = body.strip_prefix(token)
    {
        let mut prefix = token.to_string();
        let last = token.chars().last()?;
        let mut extra = 0;
        for character in rest.chars() {
            if character == last {
                prefix.push(character);
                extra += 1;
            } else {
                break;
            }
        }
        let mut body_after = &rest[extra..];
        if let Some(stripped) = body_after.strip_prefix('!') {
            prefix.push('!');
            body_after = stripped;
        }
        // Enter on an empty comment ends it rather than extending it.
        if body_after.trim().is_empty() {
            return Some(indent);
        }
        return Some(format!("{indent}{prefix} "));
    }

    // A block comment: keep the ` * ` gutter while the block is still open.
    if let Some((open, close)) = buffer.language.block_comment
        && let Some(rest) = body.strip_prefix(open)
        && !rest.contains(close)
    {
        return Some(format!("{indent} * "));
    }
    None
}

/// The indentation for a line continued inside an open `(` or `[`.
///
/// Two shapes, matching what every other editor does:
/// * the opener ends its line (`foo(`) — one level in from the opener's line;
/// * there is already something after the opener (`foo(a,`) — line up with that
///   first argument, so a list of arguments reads as a column.
///
/// `None` when the caret is not inside an open `(` or `[`, or when the tree is
/// not needed because the text alone answers it.  `{` is deliberately excluded:
/// that is a block, and [`opens_block`] handles it.
pub fn continuation_indent(
    buffer: &Buffer,
    at: usize,
    expand_tab: bool,
    tab_width: usize,
) -> Option<String> {
    let at = buffer.clamp_byte(at);
    let (open, open_at) = unclosed_openers(buffer, at)
        .into_iter()
        .rev()
        .find(|(c, _)| matches!(c, '(' | '['))?;

    let open_line = buffer.line_of(open_at);
    let open_line_end = buffer.line_end(open_line);
    let after_open = buffer.slice(Span::new(open_at + open.len_utf8(), open_line_end));

    if after_open.trim().is_empty() {
        // Nothing after the opener: one level in from the line that opened it.
        return Some(format!(
            "{}{}",
            buffer.indent_str(open_line),
            unit(expand_tab, tab_width)
        ));
    }

    // Align under the first argument.  Alignment is a visual column, so it is
    // always spaces — a tab would land on the next stop, not on the column.
    let column = buffer.display_col(open_at) + 1;
    Some(" ".repeat(column))
}

/// Shift the continuation lines of a paste to the caret's indentation.
///
/// The first line is inserted *at* the caret, which already sits at
/// `target_indent`, so it is only stripped of the block's own base indentation.
/// Every following line is shifted to `target_indent`, so a block copied from
/// column 0 and pasted inside a function keeps its internal shape and lands at
/// the function's depth instead of at the left margin.
///
/// Lines that are blank stay blank (no trailing whitespace).
pub fn reindent_paste(text: &str, target_indent: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    // The pasted block's own base indentation: the first non-blank line's.
    let source_indent = lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| leading_whitespace(line))
        .unwrap_or("");

    // Nothing to do when the block already sits where it is going.
    if source_indent == target_indent {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + target_indent.len() * lines.len());
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            // A blank line carries no indentation; trailing spaces are noise.
            continue;
        }
        let rest = line.strip_prefix(source_indent).unwrap_or(line);
        // The first line continues the caret's own line, which already carries
        // the indentation; prefixing it again would double it.
        if index > 0 {
            out.push_str(target_indent);
        }
        out.push_str(rest);
    }
    out
}

/// The leading run of spaces and tabs.
fn leading_whitespace(line: &str) -> &str {
    let end = line
        .char_indices()
        .find(|(_, c)| *c != ' ' && *c != '\t')
        .map(|(index, _)| index)
        .unwrap_or(line.len());
    &line[..end]
}

/// Does the text before the caret open a block?
fn opens_block(before: &str) -> bool {
    let trimmed = before.trim_end();
    let Some(last) = trimmed.chars().last() else {
        return false;
    };
    if matches!(last, '{' | '(' | '[') {
        return true;
    }
    // Python and friends open a block with a colon.  `::` is a Rust path, not a
    // block, and a `:` inside a string or comment is not one either — but the
    // common case is worth the small guess.
    last == ':' && !trimmed.ends_with("::")
}

/// The indentation a line opened next to `line` should have.
///
/// `below` is `o` (a new line after `line`); `!below` is `O` (before it).  A
/// line that opens a block indents the line opened *below* it one level, so
/// `o` on `fn main() {` lands inside the block.  A line opened above keeps the
/// line's own indentation: it sits outside the block the line opens.
pub fn opened_line_indent(
    buffer: &Buffer,
    line: usize,
    below: bool,
    expand_tab: bool,
    tab_width: usize,
) -> String {
    let base = buffer.indent_str(line);
    if below && opens_block(&buffer.line_str(line)) {
        format!("{base}{}", unit(expand_tab, tab_width))
    } else {
        base
    }
}

/// The indentation a closing bracket typed at `at` should sit at.
///
/// Only applies when the caret is alone on its line: a `}` typed after code
/// belongs where the code is, not where the block started.
pub fn dedent_for_close(buffer: &Buffer, at: usize, close: char) -> Option<String> {
    let at = buffer.clamp_byte(at);
    let line = buffer.line_of(at);
    let line_start = buffer.line_start(line);
    let before = buffer.slice(Span::new(line_start, at));
    if !before.chars().all(|c| c == ' ' || c == '\t') {
        return None;
    }
    let want = match syntax::block_indent_at(buffer, at) {
        Some(indent) => indent,
        None => {
            let open = matching_open(buffer, at, close)?;
            buffer.indent_str(buffer.line_of(open))
        }
    };
    (want != before).then_some(want)
}

/// Byte offset of the opening bracket that `close` at `at` would match.
///
/// A forward scan that skips strings and comments, so a `}` inside a string
/// does not close a block.  The tree-sitter answer is preferred by
/// [`dedent_for_close`]; this is the fallback for a stale tree, which is the
/// common case right after typing the opening bracket.
fn matching_open(buffer: &Buffer, at: usize, close: char) -> Option<usize> {
    let open = match close {
        '}' => '{',
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    unclosed_openers(buffer, at)
        .iter()
        .rev()
        .find(|(c, _)| *c == open)
        .map(|(_, at)| *at)
}

/// The brackets still open at `at`, outermost first.
///
/// One scan that skips strings and comments, shared by the dedent fallback and
/// the continuation indenter so the two cannot disagree about which bracket is
/// really open.
fn unclosed_openers(buffer: &Buffer, at: usize) -> Vec<(char, usize)> {
    let text = buffer.slice(Span::new(0, at));
    let chars: Vec<char> = text.chars().collect();
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut in_string: Option<char> = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut previous = '\0';
    let mut byte = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        let here = byte;
        byte += ch.len_utf8();
        index += 1;
        if ch == '\n' {
            line_comment = false;
            previous = '\0';
            continue;
        }
        if line_comment {
            continue;
        }
        if block_comment {
            if previous == '*' && ch == '/' {
                block_comment = false;
            }
            previous = ch;
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
            previous = ch;
            continue;
        }
        match ch {
            '/' if previous == '/' => line_comment = true,
            '*' if previous == '/' => block_comment = true,
            '"' => in_string = Some('"'),
            '\'' if char_literal_ahead(&chars, index) => in_string = Some('\''),
            '`' => in_string = Some('`'),
            '{' | '(' | '[' => stack.push((ch, here)),
            '}' | ')' | ']' if stack.last().is_some_and(|(top, _)| *top == open_for(ch)) => {
                stack.pop();
            }
            _ => {}
        }
        previous = ch;
    }
    stack
}

fn open_for(close: char) -> char {
    match close {
        '}' => '{',
        ')' => '(',
        ']' => '[',
        other => other,
    }
}

/// Is the quote at `index` the start of a char literal (`'a'`) rather than a
/// Rust lifetime (`'a`)?
fn char_literal_ahead(chars: &[char], index: usize) -> bool {
    let mut at = index;
    let mut steps = 0;
    while at < chars.len() && steps < 4 {
        if chars[at] == '\'' {
            return steps > 0;
        }
        if chars[at] == '\n' {
            return false;
        }
        at += 1;
        steps += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufferId;
    use crate::config::EditorConfig;

    fn buffer(text: &str) -> Buffer {
        Buffer::new(
            BufferId(1),
            Some(std::path::PathBuf::from("sample.rs")),
            text.to_string(),
            &EditorConfig::default(),
        )
    }

    fn parsed(text: &str) -> Buffer {
        let mut buffer = buffer(text);
        buffer.reparse_now();
        buffer
    }

    #[test]
    fn a_line_that_opens_a_block_indents_the_next_one() {
        let buffer = buffer("fn main() {");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    ");
        assert_eq!(plan.caret, plan.text.len());
    }

    #[test]
    fn a_closing_bracket_keeps_its_own_line() {
        // `{|}` + enter: the caret goes between the braces, and the brace is
        // dedented back to the block's level.
        let buffer = buffer("fn main() {}");
        let at = buffer.len_bytes() - 1;
        let plan = newline(&buffer, at, true, 4);
        assert_eq!(plan.text, "\n    \n");
        assert_eq!(plan.caret, 5, "the caret is on the indented line");
    }

    #[test]
    fn a_plain_line_keeps_its_indentation() {
        let buffer = buffer("    let a = 1;");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    ");
    }

    #[test]
    fn a_python_colon_opens_a_block() {
        let buffer = buffer("def f():");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    ");
    }

    #[test]
    fn a_rust_path_is_not_a_block() {
        let buffer = buffer("use std::fmt::");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n");
    }

    #[test]
    fn an_open_call_indents_one_level_in() {
        // Nothing after the `(`: one level in from the line that opened it.
        let buffer = buffer("    foo(");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n        ");
    }

    #[test]
    fn a_continuation_lines_up_with_the_first_argument() {
        // `foo(a,` — the next argument belongs under `a`, not one level in.
        let buffer = buffer("foo(a,");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    ", "aligned under the first argument");
    }

    #[test]
    fn a_continuation_aligns_through_leading_indentation() {
        let buffer = buffer("    foo(a,");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n        ");
    }

    #[test]
    fn a_continuation_inside_a_list_aligns_under_its_own_opener() {
        // The innermost open bracket wins, so a nested call aligns to the inner
        // one rather than the outer.
        let buffer = buffer("outer(inner(a,");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n            ");
    }

    #[test]
    fn a_continuation_keeps_the_closing_bracket_on_its_own_line() {
        let buffer = buffer("foo(a,)");
        let at = buffer.len_bytes() - 1;
        let plan = newline(&buffer, at, true, 4);
        assert_eq!(plan.text, "\n    \n");
        assert_eq!(plan.caret, 5, "the caret is on the aligned line");
    }

    #[test]
    fn a_brace_is_a_block_not_a_continuation() {
        // `{` opens a block; only `(` and `[` hang.
        let buffer = buffer("fn main() {");
        assert_eq!(continuation_indent(&buffer, buffer.len_bytes(), true, 4), None);
    }

    #[test]
    fn a_closed_bracket_is_not_a_continuation() {
        let buffer = buffer("foo(a)");
        assert_eq!(continuation_indent(&buffer, buffer.len_bytes(), true, 4), None);
    }

    #[test]
    fn a_bracket_inside_a_string_does_not_start_a_continuation() {
        let buffer = buffer("let s = \"(\";");
        assert_eq!(continuation_indent(&buffer, buffer.len_bytes(), true, 4), None);
    }

    #[test]
    fn a_line_comment_continues_itself() {
        let buffer = buffer("    // a note");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    // ");
        assert_eq!(plan.caret, plan.text.len());
    }

    #[test]
    fn a_doc_comment_keeps_its_extra_slashes() {
        let docs = buffer("/// docs");
        let plan = newline(&docs, docs.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n/// ");

        let module = buffer("//! module docs");
        let plan = newline(&module, module.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n//! ");
    }

    #[test]
    fn enter_on_an_empty_line_comment_ends_it() {
        // `//` with nothing after it: the next line is plain code, not another
        // empty comment.
        let buffer = buffer("    //");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    ");
    }

    #[test]
    fn a_block_comment_keeps_its_star_gutter() {
        let buffer = buffer("    /* a note");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n     * ");
    }

    #[test]
    fn a_closed_block_comment_does_not_continue() {
        let buffer = buffer("    /* done */");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n    ", "the comment is closed");
    }

    #[test]
    fn a_comment_wins_over_a_bracket_inside_it() {
        // The `(` is text, so the new line continues the comment rather than
        // hanging under a call that does not exist.
        let buffer = buffer("// call foo(");
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n// ");
    }

    #[test]
    fn a_python_hash_comment_continues() {
        let mut buffer = buffer("# a note");
        buffer.set_path(std::path::PathBuf::from("sample.py"));
        let plan = newline(&buffer, buffer.len_bytes(), true, 4);
        assert_eq!(plan.text, "\n# ");
    }

    #[test]
    fn a_pasted_block_moves_to_the_caret_depth() {
        // Copied from column 0, pasted inside a function: the first line lands
        // at the caret (which already carries the indentation) and the rest of
        // the block follows it, keeping its internal shape.
        let text = "let a = 1;\nif a {\n    b();\n}\n";
        let shifted = reindent_paste(text, "    ");
        assert_eq!(shifted, "let a = 1;\n    if a {\n        b();\n    }\n");
    }

    #[test]
    fn a_pasted_block_that_is_already_indented_is_unchanged() {
        let text = "    let a = 1;\n";
        assert_eq!(reindent_paste(text, "    "), text);
    }

    #[test]
    fn a_pasted_block_can_be_dedented() {
        // Pasting an indented block at column 0 strips the common indentation.
        let text = "    let a = 1;\n    let b = 2;\n";
        assert_eq!(reindent_paste(text, ""), "let a = 1;\nlet b = 2;\n");
    }

    #[test]
    fn blank_lines_in_a_paste_carry_no_trailing_whitespace() {
        let text = "a\n\nb\n";
        assert_eq!(reindent_paste(text, "  "), "a\n\n  b\n");
    }

    #[test]
    fn a_paste_keeps_its_own_relative_indentation() {
        // The first line sets the base; deeper lines stay deeper by the same
        // amount, shifted to the caret's depth.
        let text = "fn f() {\n    body();\n}\n";
        let shifted = reindent_paste(text, "        ");
        assert_eq!(
            shifted,
            "fn f() {\n            body();\n        }\n"
        );
    }

    #[test]
    fn an_empty_paste_stays_empty() {
        assert_eq!(reindent_paste("", "    "), "");
    }

    #[test]
    fn a_closing_bracket_alone_on_its_line_dedents() {
        let buffer = buffer("fn main() {\n    let a = 1;\n        ");
        let at = buffer.len_bytes();
        let indent = dedent_for_close(&buffer, at, '}').expect("a dedent");
        assert_eq!(indent, "");
    }

    #[test]
    fn a_closing_bracket_after_code_is_left_alone() {
        let buffer = buffer("fn main() {\n    let a = 1; ");
        let at = buffer.len_bytes();
        assert_eq!(dedent_for_close(&buffer, at, '}'), None);
    }

    #[test]
    fn brackets_inside_strings_and_comments_do_not_match() {
        // The `{` in the string and the comment must not be taken for the
        // block's opening brace.
        let buffer = buffer("fn main() {\n    let s = \"{\";\n    // {\n        ");
        let at = buffer.len_bytes();
        let indent = dedent_for_close(&buffer, at, '}').expect("a dedent");
        assert_eq!(indent, "");
    }

    #[test]
    fn a_rust_lifetime_is_not_a_char_literal() {
        let buffer = buffer("fn f<'a>(x: &'a str) {\n        ");
        let at = buffer.len_bytes();
        let indent = dedent_for_close(&buffer, at, '}').expect("a dedent");
        assert_eq!(indent, "");
    }

    #[test]
    fn the_grammar_dedents_to_the_block_that_opened_it() {
        // A current tree knows the block's own indentation, even when the
        // opening brace is on a line of its own.
        let buffer = parsed("fn main()\n{\n    let a = 1;\n        ");
        let at = buffer.len_bytes();
        let indent = dedent_for_close(&buffer, at, '}').expect("a dedent");
        assert_eq!(indent, "");
    }
}
