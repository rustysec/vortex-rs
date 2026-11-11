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
/// Three cases matter:
/// * the line opens a block — the new line is indented one level;
/// * the line opens a block *and* a closing bracket follows — the bracket keeps
///   its own line, dedented back to the block's level;
/// * anything else — the new line keeps the current indentation.
pub fn newline(buffer: &Buffer, at: usize, expand_tab: bool, tab_width: usize) -> Newline {
    let at = buffer.clamp_byte(at);
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

    let opens = opens_block(&before);
    let closes = after.trim_start().starts_with(['}', ')', ']']);
    let inner = if opens {
        format!("{base}{}", unit(expand_tab, tab_width))
    } else {
        base.clone()
    };

    if opens && closes {
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
        .iter()
        .rev()
        .find(|(c, _)| *c == open)
        .map(|(_, at)| *at)
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
