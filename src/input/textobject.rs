//! Text objects: what `i`/`a` select after an operator.

use crate::buffer::{Buffer, WordClass, next_char_boundary};
use crate::selection::Span;
use crate::syntax::TsObject;

/// A selectable text object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// Bracket pair, keyed by its opening character: `(`, `[`, `{`, `<`.
    Bracket(char),
    /// Quote pair: `"`, `'`, `` ` ``.
    Quote(char),
    /// `w` (word) or `W` (whitespace separated word).
    Word(bool),
    Sentence,
    Paragraph,
    /// The whole line, `il` without the line break, `al` with it.
    Line,
    /// Grammar aware objects: `if` function, `ic` class, `iC` comment.
    TreeSitter(TsObject),
}

/// Map the key typed after `i`/`a` to an object.
pub fn object_from_key(key: char) -> Option<Object> {
    match key {
        '(' | ')' | 'b' => Some(Object::Bracket('(')),
        '[' | ']' => Some(Object::Bracket('[')),
        '{' | '}' | 'B' => Some(Object::Bracket('{')),
        '<' | '>' => Some(Object::Bracket('<')),
        '"' => Some(Object::Quote('"')),
        '\'' => Some(Object::Quote('\'')),
        '`' => Some(Object::Quote('`')),
        'w' => Some(Object::Word(false)),
        'W' => Some(Object::Word(true)),
        's' => Some(Object::Sentence),
        'p' => Some(Object::Paragraph),
        'l' => Some(Object::Line),
        'f' | 'c' | 'C' => TsObject::from_key(key).map(Object::TreeSitter),
        _ => None,
    }
}

impl Object {
    pub fn label(self) -> String {
        match self {
            Object::Bracket(open) => match open {
                '(' => "() block".into(),
                '[' => "[] block".into(),
                '{' => "{} block".into(),
                _ => "<> block".into(),
            },
            Object::Quote(quote) => format!("{quote}…{quote} string"),
            Object::Word(false) => "word".into(),
            Object::Word(true) => "WORD".into(),
            Object::Sentence => "sentence".into(),
            Object::Paragraph => "paragraph".into(),
            Object::Line => "line".into(),
            Object::TreeSitter(object) => object.label().to_string(),
        }
    }

    /// The byte range this object covers, `inside` excluding the delimiters.
    pub fn range(self, buffer: &Buffer, cursor: usize, inside: bool) -> Option<Span> {
        let cursor = buffer.clamp_byte(cursor);
        match self {
            Object::Bracket(open) => {
                let close = match open {
                    '(' => ')',
                    '[' => ']',
                    '{' => '}',
                    _ => '>',
                };
                bracket_range(buffer, cursor, open, close, inside)
            }
            Object::Quote(quote) => quote_range(buffer, cursor, quote, inside),
            Object::Word(big) => word_range(buffer, cursor, big, inside),
            Object::Sentence => sentence_range(buffer, cursor, inside),
            Object::Paragraph => paragraph_range(buffer, cursor, inside),
            Object::Line => line_range(buffer, cursor, inside),
            Object::TreeSitter(object) => ts_range(buffer, cursor, object, inside),
        }
    }
}

fn bracket_range(
    buffer: &Buffer,
    cursor: usize,
    open: char,
    close: char,
    inside: bool,
) -> Option<Span> {
    // Walk backwards to the opening bracket that is still open at `cursor`.
    let mut depth = 0i32;
    let mut start = None;
    if buffer.char_at(cursor) == Some(close) {
        depth += 1;
    } else if buffer.char_at(cursor) == Some(open) {
        start = Some(cursor);
    }
    let mut q = cursor;
    while start.is_none() {
        let Some((offset, ch)) = buffer.char_before(q) else {
            break;
        };
        q = offset;
        if ch == close {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                start = Some(q);
            } else {
                depth -= 1;
            }
        }
    }
    let start = match start {
        Some(start) => start,
        // Outside any pair: vim looks forward for the next one.
        None => find_forward(buffer, cursor, open)?,
    };

    // Walk forwards from there to its partner.
    let mut depth = 0i32;
    let mut p = start;
    let mut end = None;
    while let Some((offset, ch)) = buffer.char_after(p) {
        p = offset;
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                end = Some(p);
                break;
            }
        }
        p = next_char_boundary(&buffer.rope, p);
    }
    let close_at = end?;

    Some(if inside {
        Span::new(start + open.len_utf8(), close_at)
    } else {
        Span::new(start, close_at + close.len_utf8())
    })
}

/// The next occurrence of `needle` at or after `from`.
fn find_forward(buffer: &Buffer, from: usize, needle: char) -> Option<usize> {
    let total = buffer.len_bytes();
    let mut p = from;
    while p < total {
        if buffer.char_at(p) == Some(needle) {
            return Some(p);
        }
        p = next_char_boundary(&buffer.rope, p);
    }
    None
}

fn quote_range(buffer: &Buffer, cursor: usize, quote: char, inside: bool) -> Option<Span> {
    let line = buffer.line_of(cursor);
    let line_start = buffer.line_start(line);
    let line_end = buffer.line_end(line);

    let mut positions: Vec<usize> = Vec::new();
    let mut p = line_start;
    while p < line_end {
        if buffer.char_at(p) == Some(quote) && !is_escaped(buffer, p) {
            positions.push(p);
        }
        p = next_char_boundary(&buffer.rope, p);
    }

    for pair in positions.chunks(2) {
        if pair.len() != 2 {
            continue;
        }
        let (open, close) = (pair[0], pair[1]);
        let within = cursor > open && cursor < close;
        let on_delimiter = cursor == open || cursor == close;
        if within || on_delimiter {
            return Some(pair_span(open, close, quote, inside));
        }
    }

    // Nothing on this line: look forward for the next quoted run, like vim does.
    let total = buffer.len_bytes();
    let open = find_forward(buffer, cursor, quote)?;
    let mut p = next_char_boundary(&buffer.rope, open);
    while p < total {
        if buffer.char_at(p) == Some(quote) && !is_escaped(buffer, p) {
            return Some(pair_span(open, p, quote, inside));
        }
        p = next_char_boundary(&buffer.rope, p);
    }
    None
}

fn pair_span(open: usize, close: usize, quote: char, inside: bool) -> Span {
    if inside {
        Span::new(open + quote.len_utf8(), close)
    } else {
        Span::new(open, close + quote.len_utf8())
    }
}

/// An odd number of backslashes before `byte` means the character is escaped.
fn is_escaped(buffer: &Buffer, byte: usize) -> bool {
    let mut count = 0;
    let mut q = byte;
    while let Some((offset, ch)) = buffer.char_before(q) {
        if ch != '\\' {
            break;
        }
        count += 1;
        q = offset;
    }
    count % 2 == 1
}

fn word_range(buffer: &Buffer, cursor: usize, big: bool, inside: bool) -> Option<Span> {
    let ch = buffer
        .char_at(cursor)
        .or_else(|| buffer.char_before(cursor).map(|(_, c)| c))?;
    let is_word = if big {
        !ch.is_whitespace()
    } else {
        ch.is_alphanumeric() || ch == '_'
    };

    if !is_word {
        // On punctuation or whitespace the object is that run itself.
        let span = if ch.is_whitespace() {
            whitespace_run(buffer, cursor, big)
        } else {
            single_char_span(buffer, cursor)
        };
        return Some(if inside {
            span
        } else {
            extend_to_words(buffer, span)
        });
    }

    let span = if big {
        big_word_range(buffer, cursor)
    } else {
        buffer.word_at(cursor)
    };
    if inside {
        Some(span)
    } else {
        Some(extend_to_words(buffer, span))
    }
}

fn single_char_span(buffer: &Buffer, cursor: usize) -> Span {
    let at = if buffer.char_at(cursor).is_some() {
        cursor
    } else {
        buffer
            .char_before(cursor)
            .map(|(offset, _)| offset)
            .unwrap_or(cursor)
    };
    Span::new(at, next_char_boundary(&buffer.rope, at))
}

fn whitespace_run(buffer: &Buffer, cursor: usize, big: bool) -> Span {
    let mut start = cursor;
    while let Some((offset, ch)) = buffer.char_before(start) {
        if !ch.is_whitespace() {
            break;
        }
        start = offset;
    }
    let mut end = cursor;
    while let Some(ch) = buffer.char_at(end) {
        if !ch.is_whitespace() {
            break;
        }
        end = next_char_boundary(&buffer.rope, end);
    }
    let _ = big;
    Span::new(start, end)
}

fn big_word_range(buffer: &Buffer, cursor: usize) -> Span {
    let mut start = cursor;
    while let Some((offset, ch)) = buffer.char_before(start) {
        if ch.is_whitespace() {
            break;
        }
        start = offset;
    }
    let mut end = cursor;
    while let Some(ch) = buffer.char_at(end) {
        if ch.is_whitespace() {
            break;
        }
        end = next_char_boundary(&buffer.rope, end);
    }
    Span::new(start, end)
}

/// `aw`: add the surrounding whitespace, preferring what follows the word.
fn extend_to_words(buffer: &Buffer, span: Span) -> Span {
    let mut start = span.start;
    let mut end = span.end;
    let mut trailing = 0;
    let mut leading = 0;
    while let Some(ch) = buffer.char_at(end) {
        if !matches!(ch, ' ' | '\t') {
            break;
        }
        end = next_char_boundary(&buffer.rope, end);
        trailing += 1;
    }
    if trailing == 0 {
        while let Some((offset, ch)) = buffer.char_before(start) {
            if !matches!(ch, ' ' | '\t') {
                break;
            }
            start = offset;
            leading += 1;
        }
    }
    // A whole line of words also swallows the line break, like vim.
    if trailing > 0 && end >= buffer.line_end(buffer.line_of(end)) {
        end = next_char_boundary(&buffer.rope, end);
    } else if leading > 0 && start <= buffer.line_start(buffer.line_of(start)) {
        start = buffer
            .char_before(start)
            .map(|(offset, _)| offset)
            .unwrap_or(start);
    }
    Span::new(start, end)
}

fn sentence_range(buffer: &Buffer, cursor: usize, inside: bool) -> Option<Span> {
    let total = buffer.len_bytes();
    if total == 0 {
        return None;
    }
    // Start: after the previous terminator + whitespace.
    let mut start = cursor;
    let mut q = cursor;
    while let Some((offset, ch)) = buffer.char_before(q) {
        if matches!(ch, '.' | '!' | '?') {
            let after = next_char_boundary(&buffer.rope, offset);
            if after < cursor {
                start = after;
                while matches!(buffer.char_at(start), Some(c) if c.is_whitespace()) && start < total
                {
                    start = next_char_boundary(&buffer.rope, start);
                }
                break;
            }
        }
        if offset == 0 {
            start = 0;
            break;
        }
        q = offset;
    }
    // End: the next terminator, then trailing whitespace when outside.
    let mut end = cursor.max(start);
    let mut p = cursor.max(start);
    while p < total {
        let ch = buffer.char_at(p).unwrap_or(' ');
        if matches!(ch, '.' | '!' | '?') {
            let following = buffer.char_at(next_char_boundary(&buffer.rope, p));
            if following.is_none_or(|c| c.is_whitespace() || matches!(c, ')' | '"' | '\'')) {
                end = next_char_boundary(&buffer.rope, p);
                break;
            }
        }
        p = next_char_boundary(&buffer.rope, p);
    }
    if !inside {
        while matches!(buffer.char_at(end), Some(c) if c.is_whitespace()) && end < total {
            end = next_char_boundary(&buffer.rope, end);
        }
    }
    (end > start).then_some(Span::new(start, end))
}

fn paragraph_range(buffer: &Buffer, cursor: usize, inside: bool) -> Option<Span> {
    let line = buffer.line_of(cursor);
    let last = buffer.line_count() - 1;
    let mut first = line;
    while first > 0 && !buffer.is_line_blank(first - 1) {
        first -= 1;
    }
    let mut bottom = line;
    while bottom < last && !buffer.is_line_blank(bottom + 1) {
        bottom += 1;
    }
    let start = buffer.line_start(first);
    let mut end = if bottom < last {
        buffer.line_start(bottom + 1)
    } else {
        buffer.len_bytes()
    };
    if !inside {
        // Around: include the blank lines that separate paragraphs.
        while end < buffer.len_bytes() && buffer.is_line_blank(buffer.line_of(end)) {
            end = if end + 1 >= buffer.len_bytes() {
                buffer.len_bytes()
            } else {
                buffer.line_start(buffer.line_of(end) + 1)
            };
        }
    }
    Some(Span::new(start, end))
}

fn line_range(buffer: &Buffer, cursor: usize, inside: bool) -> Option<Span> {
    let line = buffer.line_of(cursor);
    if inside {
        Some(Span::new(buffer.line_start(line), buffer.line_end(line)))
    } else {
        Some(buffer.line_range_with_ending(line))
    }
}

fn ts_range(buffer: &Buffer, cursor: usize, object: TsObject, inside: bool) -> Option<Span> {
    let candidates = crate::syntax::text_objects(buffer, cursor, object);
    let (outer, inner) = candidates.into_iter().next()?;
    Some(if inside { inner } else { outer })
}

/// Words that are `word` class, used to keep `iw` from selecting operators.
pub fn is_word_char(ch: char) -> bool {
    WordClass::of(ch) == WordClass::Word
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EditorConfig;

    fn buffer(text: &str) -> Buffer {
        Buffer::new(
            crate::buffer::BufferId(1),
            None,
            text.to_string(),
            &EditorConfig::default(),
        )
    }

    fn select(text: &str, at: usize, object: Object, inside: bool) -> String {
        let buffer = buffer(text);
        let span = object
            .range(&buffer, at, inside)
            .unwrap_or_else(|| panic!("{object:?} found nothing at {at} in {text:?}"));
        let selected = buffer.slice(span);
        assert!(
            !selected.is_empty(),
            "{object:?} produced an empty selection at {at}"
        );
        selected
    }

    #[test]
    fn bracket_pairs() {
        let text = "foo(bar, baz(qux))";
        assert_eq!(select(text, 5, Object::Bracket('('), true), "bar, baz(qux)");
        assert_eq!(
            select(text, 5, Object::Bracket('('), false),
            "(bar, baz(qux))"
        );
        assert_eq!(select(text, 15, Object::Bracket('('), true), "qux");
        assert_eq!(select(text, 15, Object::Bracket('('), false), "(qux)");
    }

    #[test]
    fn bracket_aliases() {
        let text = "let v = vec![1, 2];";
        assert_eq!(select(text, 12, Object::Bracket('['), true), "1, 2");
        assert_eq!(select(text, 12, Object::Bracket('['), false), "[1, 2]");
        let text = "if x { y }";
        assert_eq!(select(text, 4, Object::Bracket('{'), true), " y ");
        assert_eq!(select(text, 4, Object::Bracket('{'), false), "{ y }");
    }

    #[test]
    fn quote_pairs_respect_escapes() {
        let text = r#"let s = "a\"b"; let t = "x";"#;
        // Inside the first string, the escaped quote must not terminate it.
        let inside_first = text.find('b').unwrap();
        assert_eq!(
            select(text, inside_first, Object::Quote('"'), true),
            r#"a\"b"#
        );
        assert_eq!(
            select(text, inside_first, Object::Quote('"'), false),
            r#""a\"b""#
        );
        let inside_second = text.rfind('x').unwrap();
        assert_eq!(select(text, inside_second, Object::Quote('"'), true), "x");
        // A cursor outside any string finds the next one on the line.
        let before = text.find("; let").unwrap();
        assert_eq!(select(text, before, Object::Quote('"'), true), "x");
    }

    #[test]
    fn word_objects() {
        let text = "let hello_world = 5;";
        assert_eq!(select(text, 6, Object::Word(false), true), "hello_world");
        assert_eq!(select(text, 6, Object::Word(false), false), "hello_world ");
        // `aw` at the start of a line takes the leading whitespace of the next line.
        let text = "one two\nthree";
        assert_eq!(select(text, 0, Object::Word(false), false), "one ");
        assert_eq!(select(text, 11, Object::Word(false), false), "three");
        // Big words ignore punctuation.
        assert_eq!(select(text, 4, Object::Word(true), true), "two");
    }

    #[test]
    fn punctuation_object_is_the_operator_itself() {
        let text = "a + b";
        assert_eq!(select(text, 2, Object::Word(false), true), "+");
    }

    #[test]
    fn sentence_objects() {
        let text = "First one. Second one! Third?";
        assert_eq!(select(text, 3, Object::Sentence, true), "First one.");
        assert_eq!(select(text, 12, Object::Sentence, true), "Second one!");
        assert_eq!(select(text, 25, Object::Sentence, true), "Third?");
        assert_eq!(select(text, 3, Object::Sentence, false), "First one. ");
    }

    #[test]
    fn paragraph_objects() {
        let text = "one\ntwo\n\nthree";
        assert_eq!(select(text, 0, Object::Paragraph, true), "one\ntwo\n");
        assert_eq!(select(text, 0, Object::Paragraph, false), "one\ntwo\n\n");
        assert_eq!(select(text, 10, Object::Paragraph, true), "three");
    }

    #[test]
    fn line_objects() {
        let text = "alpha\nbeta\ngamma";
        let at = text.find("beta").unwrap();
        assert_eq!(select(text, at, Object::Line, true), "beta");
        assert_eq!(select(text, at, Object::Line, false), "beta\n");
    }

    #[test]
    fn key_mapping_covers_vim_objects() {
        assert_eq!(object_from_key('('), Some(Object::Bracket('(')));
        assert_eq!(object_from_key(')'), Some(Object::Bracket('(')));
        assert_eq!(object_from_key('b'), Some(Object::Bracket('(')));
        assert_eq!(object_from_key('B'), Some(Object::Bracket('{')));
        assert_eq!(object_from_key('"'), Some(Object::Quote('"')));
        assert_eq!(object_from_key('w'), Some(Object::Word(false)));
        assert_eq!(object_from_key('W'), Some(Object::Word(true)));
        assert_eq!(object_from_key('p'), Some(Object::Paragraph));
        assert_eq!(
            object_from_key('f'),
            Some(Object::TreeSitter(TsObject::Function))
        );
        assert_eq!(object_from_key('q'), None);
    }

    #[test]
    fn grammar_objects_find_functions() {
        let text = "fn outer() {\n    let inner = 1;\n}\n";
        // Grammar objects need a language and a parsed tree.
        let mut buffer = Buffer::new(
            crate::buffer::BufferId(1),
            Some(std::path::PathBuf::from("outer.rs")),
            text.to_string(),
            &EditorConfig::default(),
        );
        assert!(buffer.reparse_now(), "the Rust grammar should parse this");
        // Point inside the function body.
        let at = text.find("let").unwrap() + 2;
        let span = Object::TreeSitter(TsObject::Function)
            .range(&buffer, at, false)
            .expect("function object");
        assert_eq!(buffer.slice(span), "fn outer() {\n    let inner = 1;\n}");
        let inside = Object::TreeSitter(TsObject::Function)
            .range(&buffer, at, true)
            .expect("function body");
        assert_eq!(buffer.slice(inside), "{\n    let inner = 1;\n}");
    }
}
