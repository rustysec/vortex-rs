//! Motions: everything that moves the cursor (and therefore what operators act on).
//!
//! A motion is a pure function `(cursor, buffer, context) -> destination`.  The
//! editor turns a destination into a selection or, when an operator is pending,
//! into the region the operator edits.

use crate::action::{FindDir, Motion};
use crate::buffer::{Buffer, WordClass, next_char_boundary};
use crate::selection::Span;

/// Whether a motion covers characters or whole lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Char,
    Line,
}

/// Where a motion lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dest {
    pub cursor: usize,
    pub kind: Kind,
    /// Inclusive motions include the character under the landing cursor.
    pub inclusive: bool,
    /// Set when a vertical motion wants a specific display column.
    pub wanted_col: Option<usize>,
}

impl Dest {
    fn char(cursor: usize) -> Self {
        Self {
            cursor,
            kind: Kind::Char,
            inclusive: false,
            wanted_col: None,
        }
    }

    fn inclusive_char(cursor: usize) -> Self {
        Self {
            cursor,
            kind: Kind::Char,
            inclusive: true,
            wanted_col: None,
        }
    }

    fn line(cursor: usize) -> Self {
        Self {
            cursor,
            kind: Kind::Line,
            inclusive: true,
            wanted_col: None,
        }
    }
}

/// A search term, resolved by the editor from `/` and `?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub pattern: String,
    pub case_sensitive: bool,
    /// Search backwards by default (`N`).
    pub backward: bool,
}

/// Context a motion may need beyond the buffer and the cursor.
#[derive(Debug, Clone, Copy)]
pub struct Env<'a> {
    /// Rows of text visible in the pane.
    pub height: usize,
    pub scroll_line: usize,
    pub wanted_col: Option<usize>,
    pub last_find: Option<(FindDir, char)>,
    pub search: Option<&'a SearchQuery>,
}

impl Default for Env<'static> {
    fn default() -> Self {
        Self {
            height: 20,
            scroll_line: 0,
            wanted_col: None,
            last_find: None,
            search: None,
        }
    }
}

/// Apply `motion` `count` times starting at `cursor`.
pub fn apply(
    motion: Motion,
    count: usize,
    cursor: usize,
    buffer: &Buffer,
    env: &Env<'_>,
) -> Option<Dest> {
    let count = count.max(1);
    let cursor = buffer.clamp_byte(cursor);
    match motion {
        Motion::Left => {
            let mut p = cursor;
            for _ in 0..count {
                match buffer.char_before(p) {
                    Some((offset, '\n')) => p = offset,
                    Some((offset, _)) => p = offset,
                    None => break,
                }
            }
            Some(Dest::char(p))
        }
        Motion::Right => {
            let mut p = cursor;
            for _ in 0..count {
                let next = next_char_boundary(&buffer.rope, p);
                if next == p {
                    break;
                }
                // The raw step may land on the line break; the editor clamps
                // normal-mode cursors back to the last character so operators
                // never eat the break.
                p = next;
            }
            Some(Dest::char(p))
        }
        Motion::Up => vertical(buffer, cursor, env, count, -1),
        Motion::Down => vertical(buffer, cursor, env, count, 1),
        Motion::LineStart => Some(Dest::inclusive_char(
            buffer.line_start(buffer.line_of(cursor)),
        )),
        Motion::FirstNonBlank => Some(Dest::inclusive_char(
            buffer.first_non_blank(buffer.line_of(cursor)),
        )),
        Motion::LineEnd | Motion::TillLineEnd => {
            // `$` (and its `D` / `C` aliases) can move down `count - 1` lines,
            // but always stops on a character — never on the line break.
            let line = (buffer.line_of(cursor) + count.saturating_sub(1)).min(buffer.last_line());
            Some(Dest::inclusive_char(buffer.last_char_of_line(line)))
        }
        Motion::LineEndFirstNonBlank => Some(Dest::inclusive_char(
            buffer.first_non_blank(last_non_blank_line(buffer, cursor, env)?),
        )),
        Motion::Column => {
            let line = buffer.line_of(cursor);
            let byte = buffer.byte_at_display_col(line, count.saturating_sub(1));
            Some(Dest::inclusive_char(
                byte.min(last_byte_on_line(buffer, line)),
            ))
        }
        Motion::WordStart => Some(Dest::char(word_forward(buffer, cursor, count, false))),
        Motion::WordEnd => Some(Dest::inclusive_char(word_end(buffer, cursor, count, false))),
        Motion::WordBack => Some(Dest::char(word_back(buffer, cursor, count, false))),
        Motion::WordEndBack => Some(Dest::inclusive_char(word_end_back(
            buffer, cursor, count, false,
        ))),
        Motion::BigWordStart => Some(Dest::char(word_forward(buffer, cursor, count, true))),
        Motion::BigWordEnd => Some(Dest::inclusive_char(word_end(buffer, cursor, count, true))),
        Motion::BigWordBack => Some(Dest::char(word_back(buffer, cursor, count, true))),
        Motion::BigWordEndBack => Some(Dest::inclusive_char(word_end_back(
            buffer, cursor, count, true,
        ))),
        Motion::CharAhead => {
            // `x`, `s`: `count` characters starting at the cursor.
            let mut end = cursor;
            let mut last = cursor;
            for _ in 0..count.max(1) {
                let next = next_char_boundary(&buffer.rope, end);
                if next == end {
                    break;
                }
                last = end;
                end = next;
            }
            if end == cursor {
                return None;
            }
            Some(Dest::inclusive_char(last))
        }
        Motion::FindChar(_dir) => None, // resolved with a character by the editor
        Motion::RepeatFind | Motion::RepeatFindReverse => None, // needs the last find
        Motion::DocumentStart => {
            let line = if count > 1 {
                (count - 1).min(buffer.last_line())
            } else {
                0
            };
            Some(Dest::line(buffer.first_non_blank(line)))
        }
        Motion::DocumentEnd => {
            let line = if count > 1 {
                (count - 1).min(buffer.last_line())
            } else {
                buffer.last_line()
            };
            Some(Dest::line(buffer.first_non_blank(line)))
        }
        Motion::ViewTop => view_line(buffer, env, env.scroll_line),
        Motion::ViewMiddle => view_line(buffer, env, env.scroll_line + env.height / 2),
        Motion::ViewBottom => {
            view_line(buffer, env, env.scroll_line + env.height.saturating_sub(1))
        }
        Motion::PageForward => page(buffer, cursor, env, count, 1),
        Motion::PageBackward => page(buffer, cursor, env, count, -1),
        Motion::HalfPageForward => {
            let half = env.max_half();
            page(buffer, cursor, &half, count, 1)
        }
        Motion::HalfPageBackward => {
            let half = env.max_half();
            page(buffer, cursor, &half, count, -1)
        }
        Motion::ParagraphNext => paragraph(buffer, cursor, count, 1).map(Dest::line),
        Motion::ParagraphPrev => paragraph(buffer, cursor, count, -1).map(Dest::line),
        Motion::SentenceNext => Some(Dest::char(sentence(buffer, cursor, count, 1))),
        Motion::SentencePrev => Some(Dest::char(sentence(buffer, cursor, count, -1))),
        Motion::MatchBracket => match_bracket(buffer, cursor).map(Dest::inclusive_char),
        Motion::SearchNext | Motion::SearchPrev => {
            let query = env.search?;
            let backward = match motion {
                Motion::SearchPrev => !query.backward,
                _ => query.backward,
            };
            search_next(buffer, cursor, query, backward, count).map(Dest::inclusive_char)
        }
        Motion::TextObject { .. } => None, // handled by the text object module
    }
}

impl Env<'_> {
    /// Half page motions move at least one line.
    fn max_half(&self) -> Env<'_> {
        let mut env = *self;
        env.height = (self.height / 2).max(1);
        env
    }
}

fn vertical(
    buffer: &Buffer,
    cursor: usize,
    env: &Env<'_>,
    count: usize,
    direction: i32,
) -> Option<Dest> {
    let line = buffer.line_of(cursor);
    let column = env.wanted_col.unwrap_or_else(|| buffer.display_col(cursor));
    let target = if direction > 0 {
        line.saturating_add(count)
    } else {
        line.saturating_sub(count)
    };
    let target = target.min(buffer.last_line());
    // Clamped to the last character: a short line must not swallow the cursor
    // onto the line break, and the wanted column is remembered for the next step.
    let byte = buffer
        .byte_at_display_col(target, column)
        .min(last_byte_on_line(buffer, target));
    Some(Dest {
        cursor: byte,
        kind: Kind::Char,
        inclusive: false,
        wanted_col: Some(column),
    })
}

/// The last character of a line (the line start if the line is empty).  Vim's
/// cursor sits *on* a character, so `$` and vertical motion stop here.
fn last_byte_on_line(buffer: &Buffer, line: usize) -> usize {
    buffer.last_char_of_line(line)
}

fn view_line(buffer: &Buffer, _env: &Env<'_>, line: usize) -> Option<Dest> {
    let line = line.min(buffer.last_line());
    Some(Dest {
        cursor: buffer.first_non_blank(line),
        kind: Kind::Line,
        inclusive: true,
        wanted_col: None,
    })
}

fn page(
    buffer: &Buffer,
    cursor: usize,
    env: &Env<'_>,
    count: usize,
    direction: i32,
) -> Option<Dest> {
    let line = buffer.line_of(cursor);
    let step = env.height.saturating_mul(count).max(1);
    let target = if direction > 0 {
        line.saturating_add(step)
    } else {
        line.saturating_sub(step)
    };
    let target = target.min(buffer.last_line());
    let column = env.wanted_col.unwrap_or_else(|| buffer.display_col(cursor));
    Some(Dest {
        cursor: buffer.byte_at_display_col(target, column),
        kind: Kind::Char,
        inclusive: false,
        wanted_col: Some(column),
    })
}

/// The next line whose content is not blank, for `_`.
fn last_non_blank_line(buffer: &Buffer, cursor: usize, env: &Env<'_>) -> Option<usize> {
    let _ = env;
    let line = buffer.line_of(cursor);
    let last = buffer.last_line();
    if buffer.is_line_blank(line) {
        for candidate in line..=last {
            if !buffer.is_line_blank(candidate) {
                return Some(candidate);
            }
        }
    }
    Some(line)
}

// ---------------------------------------------------------------- word motions

/// Class of a character for word motions (`big` = whitespace separated only).
fn class_of(ch: char, big: bool) -> WordClass {
    if big {
        if ch.is_whitespace() {
            WordClass::Space
        } else {
            WordClass::Word
        }
    } else {
        WordClass::of(ch)
    }
}

fn is_camel_hump(previous: Option<char>, current: char) -> bool {
    previous
        .map(|p| (p.is_lowercase() || p.is_numeric()) && current.is_uppercase())
        .unwrap_or(false)
}

/// `w` skips whitespace, line breaks included: from the end of a line it lands on
/// the next line's first word, like vim.
fn is_inline_space(buffer: &Buffer, byte: usize) -> bool {
    buffer.char_at(byte).is_some_and(char::is_whitespace)
}

/// `w` / `W`: start of the next word.
pub fn word_forward(buffer: &Buffer, from: usize, count: usize, big: bool) -> usize {
    let end = buffer.len_bytes();
    let mut p = from;
    for _ in 0..count {
        if p >= end {
            break;
        }
        let Some(first) = buffer.char_at(p) else {
            break;
        };
        let class = class_of(first, big);
        // Consume the run we are standing in.
        let mut q = p;
        let mut advanced = false;
        loop {
            let Some(ch) = buffer.char_at(q) else {
                q = end;
                break;
            };
            if class_of(ch, big) != class {
                break;
            }
            let previous = buffer.char_before(q).map(|(_, c)| c);
            if advanced && !big && class == WordClass::Word && is_camel_hump(previous, ch) {
                break;
            }
            let next = next_char_boundary(&buffer.rope, q);
            if next == q {
                break;
            }
            advanced = true;
            q = next;
        }
        p = q;
        // Then hop over inline spaces so `w` lands on the next word, stopping at
        // line breaks like vim.
        while p < end && is_inline_space(buffer, p) {
            p = next_char_boundary(&buffer.rope, p);
        }
    }
    p
}

/// `e` / `E`: last character of the next word (inclusive).
pub fn word_end(buffer: &Buffer, from: usize, count: usize, big: bool) -> usize {
    let end = buffer.len_bytes();
    if end == 0 {
        return 0;
    }
    let mut p = from;
    for _ in 0..count {
        if p + 1 >= end {
            break;
        }
        // Always move at least one character.
        p = next_char_boundary(&buffer.rope, p);
        while p < end && is_inline_space(buffer, p) {
            p = next_char_boundary(&buffer.rope, p);
        }
        let Some(first) = buffer.char_at(p) else {
            break;
        };
        let class = class_of(first, big);
        loop {
            let next = next_char_boundary(&buffer.rope, p);
            if next >= end {
                break;
            }
            let Some(ch) = buffer.char_at(next) else {
                break;
            };
            if class_of(ch, big) != class {
                break;
            }
            if !big && class == WordClass::Word && is_camel_hump(buffer.char_at(p), ch) {
                break;
            }
            p = next;
        }
    }
    p
}

/// `b` / `B`: start of the previous word.
pub fn word_back(buffer: &Buffer, from: usize, count: usize, big: bool) -> usize {
    let mut p = from;
    for _ in 0..count {
        if p == 0 {
            break;
        }
        // Step back one character.
        match buffer.char_before(p) {
            Some((offset, _)) => p = offset,
            None => {
                p = 0;
                break;
            }
        }
        // Skip whitespace backwards.
        loop {
            match buffer.char_before(p) {
                Some((offset, ch)) if ch == ' ' || ch == '\t' => p = offset,
                _ => break,
            }
        }
        let Some((_, last)) = buffer.char_before(p) else {
            break;
        };
        let class = class_of(last, big);
        loop {
            let Some((offset, ch)) = buffer.char_before(p) else {
                p = 0;
                break;
            };
            if class_of(ch, big) != class {
                break;
            }
            // A hump means `ch` starts a new word: stop before it.
            if !big
                && class == WordClass::Word
                && is_camel_hump(Some(ch), buffer.char_at(p).unwrap_or(' '))
            {
                break;
            }
            p = offset;
        }
    }
    p
}

/// `ge` / `gE`: last character of the previous word (inclusive).
pub fn word_end_back(buffer: &Buffer, from: usize, count: usize, big: bool) -> usize {
    let mut p = from;
    for _ in 0..count {
        loop {
            if p == 0 {
                break;
            }
            match buffer.char_before(p) {
                Some((offset, _)) => p = offset,
                None => {
                    p = 0;
                    break;
                }
            }
            if is_run_end(buffer, p, big) {
                break;
            }
        }
    }
    p
}

/// Is `byte` the last character of a word run?
fn is_run_end(buffer: &Buffer, byte: usize, big: bool) -> bool {
    let Some(ch) = buffer.char_at(byte) else {
        return false;
    };
    if ch.is_whitespace() {
        return false;
    }
    let class = class_of(ch, big);
    match buffer.char_at(next_char_boundary(&buffer.rope, byte)) {
        None => true,
        Some(next) => {
            class_of(next, big) != class
                || (!big && class == WordClass::Word && is_camel_hump(Some(ch), next))
        }
    }
}

// ---------------------------------------------------------------- find / search

/// `f`, `F`, `t`, `T` — within the current line.
pub fn find_char(
    buffer: &Buffer,
    from: usize,
    direction: FindDir,
    needle: char,
    count: usize,
) -> Option<usize> {
    let line = buffer.line_of(from);
    let line_start = buffer.line_start(line);
    let line_end = buffer.line_end(line);
    let _ = line_start;
    let mut p = from;
    let mut found = None;
    for _ in 0..count.max(1) {
        let mut hit = None;
        match direction {
            FindDir::Forward | FindDir::TillForward => {
                let mut q = next_char_boundary(&buffer.rope, p);
                while q < line_end {
                    if buffer.char_at(q) == Some(needle) {
                        hit = Some(q);
                        break;
                    }
                    q = next_char_boundary(&buffer.rope, q);
                }
            }
            FindDir::Backward | FindDir::TillBackward => {
                let mut q = p;
                while let Some((offset, ch)) = buffer.char_before(q) {
                    if offset < line_start {
                        break;
                    }
                    if ch == needle {
                        hit = Some(offset);
                        break;
                    }
                    q = offset;
                }
            }
        }
        match hit {
            Some(at) => {
                found = Some(at);
                p = at;
            }
            None => break,
        }
    }
    let at = found?;
    Some(match direction {
        FindDir::Forward | FindDir::Backward => at,
        // `t`/`T` stop one character short of the match.
        FindDir::TillForward => buffer.char_before(at).map(|(o, _)| o).unwrap_or(at),
        FindDir::TillBackward => next_char_boundary(&buffer.rope, at),
    })
}

/// `/` and `?` with wrap-around.
pub fn search_next(
    buffer: &Buffer,
    from: usize,
    query: &SearchQuery,
    backward: bool,
    count: usize,
) -> Option<usize> {
    if query.pattern.is_empty() {
        return None;
    }
    let total = buffer.len_bytes();
    if total == 0 {
        return None;
    }
    let mut position = from;
    for _ in 0..count.max(1) {
        let found = if backward {
            // Strictly before the cursor, then wrap round to the final match.
            buffer
                .find(&query.pattern, 0, position, query.case_sensitive)
                .or_else(|| last_match(buffer, &query.pattern, query.case_sensitive))
        } else {
            let start = next_char_boundary(&buffer.rope, position);
            buffer
                .find(&query.pattern, start, total, query.case_sensitive)
                .or_else(|| buffer.find(&query.pattern, 0, total, query.case_sensitive))
        };
        match found {
            Some(span) if !span.is_empty() => position = span.start,
            _ => break,
        }
    }
    Some(position)
}

/// The final match in the buffer, used when a backward search wraps around.
fn last_match(buffer: &Buffer, pattern: &str, case_sensitive: bool) -> Option<Span> {
    let total = buffer.len_bytes();
    let mut from = 0;
    let mut last = None;
    while let Some(span) = buffer.find(pattern, from, total, case_sensitive) {
        last = Some(span);
        if span.end <= from {
            break;
        }
        from = span.end.max(from + 1);
    }
    last
}

/// All matches of `query`, for highlighting.
pub fn search_matches(buffer: &Buffer, query: &SearchQuery, limit: usize) -> Vec<Span> {
    let mut out = Vec::new();
    if query.pattern.is_empty() {
        return out;
    }
    let total = buffer.len_bytes();
    let mut from = 0;
    while out.len() < limit {
        match buffer.find(&query.pattern, from, total, query.case_sensitive) {
            Some(span) if !span.is_empty() => {
                from = span.end;
                out.push(span);
            }
            _ => break,
        }
    }
    out
}

// -------------------------------------------------------------- structure

/// `%` — the matching bracket.
pub fn match_bracket(buffer: &Buffer, from: usize) -> Option<usize> {
    const OPEN: &str = "([{";
    const CLOSE: &str = ")]}";

    // Prefer the bracket under the cursor, then the first one on the line.
    let line = buffer.line_of(from);
    let line_end = buffer.line_end(line);
    let mut scan = from;
    let mut target = None;
    if let Some(ch) = buffer.char_at(from) {
        if OPEN.contains(ch) || CLOSE.contains(ch) {
            target = Some((from, ch));
        }
    }
    if target.is_none() {
        while scan < line_end {
            if let Some(ch) = buffer.char_at(scan)
                && (OPEN.contains(ch) || CLOSE.contains(ch))
            {
                target = Some((scan, ch));
                break;
            }
            scan = next_char_boundary(&buffer.rope, scan);
        }
    }
    let (at, ch) = target?;
    let closing = CLOSE.contains(ch);
    let pair = if closing {
        let index = CLOSE.find(ch)?;
        OPEN.chars().nth(index)?
    } else {
        let index = OPEN.find(ch)?;
        CLOSE.chars().nth(index)?
    };
    let step: i32 = if closing { -1 } else { 1 };
    let mut depth = 0i32;
    let mut p = at;
    let limit = buffer.len_bytes();
    loop {
        p = if step > 0 {
            next_char_boundary(&buffer.rope, p)
        } else {
            match buffer.char_before(p) {
                Some((offset, _)) => offset,
                None => break,
            }
        };
        if step > 0 && p >= limit {
            break;
        }
        if step < 0 && p > limit {
            break;
        }
        let Some(current) = buffer.char_at(p) else {
            continue;
        };
        if current == ch {
            depth += 1;
        } else if current == pair {
            depth -= 1;
            if depth < 0 {
                return Some(p);
            }
        }
    }
    None
}

/// `{` / `}` — paragraph boundaries, matching vim.
///
/// The boundary is a *run* of empty lines, and a run the cursor is standing in
/// does not count as having been found: `}` from a text line goes to the blank
/// line that ends its paragraph, and `}` from a blank line crosses the
/// paragraph ahead to the next blank line.  `{` mirrors that.  With no run left
/// `}` runs to the last character of the buffer and `{` to the first line.
///
/// Returns `None` when the motion cannot complete, which leaves the cursor
/// exactly where it started.
pub fn paragraph(buffer: &Buffer, from: usize, count: usize, direction: i32) -> Option<usize> {
    let last = buffer.last_line();
    let mut byte = from;
    let mut line = buffer.line_of(from);
    for _ in 0..count.max(1) {
        let landing = if direction > 0 {
            next_gap(buffer, line, last)
        } else {
            prev_gap(buffer, line)
        };
        let dest = match landing {
            Par::Gap(at) => buffer.line_start(at),
            Par::BufferEnd(at) => last_byte_on_line(buffer, at),
            Par::BufferStart => 0,
        };
        if dest == byte {
            // Nowhere further to go: the whole motion fails, so `2}` at the end
            // of a file does not move at all rather than half-moving.
            return None;
        }
        byte = dest;
        line = buffer.line_of(byte);
    }
    Some(byte)
}

/// Where a paragraph motion lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Par {
    /// An empty line: the first of the next run, or the last of the previous.
    Gap(usize),
    /// No empty run left ahead — the last character of the buffer.
    BufferEnd(usize),
    /// No empty run behind — the first line of the file.
    BufferStart,
}

/// The next run of empty lines after `line`.
///
/// Standing inside an empty run does not count as having found it: `}` from a
/// blank line crosses the paragraph ahead to the *next* blank line, which is
/// why the motion never degenerates into a no-op on a blank line.
fn next_gap(buffer: &Buffer, line: usize, last: usize) -> Par {
    let mut at = line;
    if buffer.is_line_blank(at) {
        while at < last && buffer.is_line_blank(at) {
            at += 1;
        }
    }
    while at < last && !buffer.is_line_blank(at) {
        at += 1;
    }
    if buffer.is_line_blank(at) {
        Par::Gap(at)
    } else {
        Par::BufferEnd(at)
    }
}

/// The previous run of empty lines before `line`, mirroring [`next_gap`].
fn prev_gap(buffer: &Buffer, line: usize) -> Par {
    let mut at = line;
    if buffer.is_line_blank(at) {
        while at > 0 && buffer.is_line_blank(at) {
            at -= 1;
        }
    }
    while at > 0 && !buffer.is_line_blank(at) {
        at -= 1;
    }
    if buffer.is_line_blank(at) {
        Par::Gap(at)
    } else {
        Par::BufferStart
    }
}

/// `(` / `)` — sentence boundaries.
/// Byte offsets at which sentences start, used by `)` and `(`.
///
/// Walking the buffer character by character is fine for a motion: it is the
/// same order of work vim does, and it keeps the rules in one place.
fn sentence_starts(buffer: &Buffer) -> Vec<usize> {
    let total = buffer.len_bytes();
    let mut starts = vec![0usize];
    let mut byte = 0usize;
    while let Some(character) = buffer.char_at(byte) {
        if matches!(character, '.' | '!' | '?') {
            let after = next_char_boundary(&buffer.rope, byte);
            let following = buffer.char_at(after);
            let ends_sentence = following
                .is_none_or(|next| next.is_whitespace() || matches!(next, ')' | '"' | '\'' | ']'));
            if ends_sentence {
                let mut probe = after;
                while probe < total
                    && buffer
                        .char_at(probe)
                        .is_some_and(|next| next.is_whitespace())
                {
                    probe = next_char_boundary(&buffer.rope, probe);
                }
                if probe < total && probe > *starts.last().unwrap_or(&0) {
                    starts.push(probe);
                }
            }
        }
        byte = next_char_boundary(&buffer.rope, byte);
        if byte == total {
            break;
        }
    }
    starts
}

/// Move to the start of the next (`direction` > 0) or previous sentence.
pub fn sentence(buffer: &Buffer, from: usize, count: usize, direction: i32) -> usize {
    let total = buffer.len_bytes();
    if total == 0 {
        return 0;
    }
    let starts = sentence_starts(buffer);
    let count = count.max(1);
    if direction > 0 {
        // Past the last sentence, `)` goes to the end of the buffer.
        let Some(index) = starts.iter().position(|&start| start > from) else {
            return total;
        };
        starts[index.saturating_add(count - 1).min(starts.len() - 1)]
    } else {
        let Some(index) = starts.iter().rposition(|&start| start < from) else {
            return 0;
        };
        starts[index.saturating_sub(count - 1)]
    }
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

    /// The character at a byte offset, so movement tests can read like prose.
    fn char_at(buffer: &Buffer, byte: usize) -> String {
        let end = (byte + 1).min(buffer.len_bytes());
        buffer.slice(Span::new(byte, end))
    }

    fn cursor(motion: Motion, count: usize, at: usize, buffer: &Buffer) -> usize {
        apply(motion, count, at, buffer, &Env::default())
            .unwrap_or_else(|| panic!("{motion:?} produced no destination"))
            .cursor
    }

    #[test]
    fn basic_horizontal_movement() {
        let buffer = buffer("hello\nworld");
        assert_eq!(cursor(Motion::Right, 1, 0, &buffer), 1);
        assert_eq!(cursor(Motion::Left, 1, 1, &buffer), 0);
        assert_eq!(cursor(Motion::LineEnd, 1, 0, &buffer), 4);
        assert_eq!(cursor(Motion::LineStart, 1, 3, &buffer), 0);
        // Left at the very start does not move.
        assert_eq!(cursor(Motion::Left, 1, 0, &buffer), 0);
    }

    #[test]
    fn right_steps_onto_the_line_break() {
        let buffer = buffer("ab\ncd");
        assert_eq!(cursor(Motion::Right, 1, 1, &buffer), 2);
        assert_eq!(cursor(Motion::Right, 1, 2, &buffer), 3);
    }

    #[test]
    fn end_of_line_stops_on_a_character_and_counts_down() {
        let empty = buffer("one\n\ntwo\n");
        let buffer = buffer("hello\nworld\nthird");
        assert_eq!(cursor(Motion::LineEnd, 2, 0, &buffer), 10);
        // `D` and `C` use the same motion as `$`, not one that lands on the
        // break and deletes it.
        assert_eq!(cursor(Motion::TillLineEnd, 1, 0, &buffer), 4);
        assert_eq!(cursor(Motion::TillLineEnd, 2, 0, &buffer), 10);
        assert_eq!(cursor(Motion::LineEnd, 1, 4, &empty), 4);
    }

    #[test]
    fn word_forward_stops_at_boundaries() {
        let buffer = buffer("fn main() {\n    let x = 1;\n}");
        // "fn" → space → "main"
        assert_eq!(
            char_at(&buffer, cursor(Motion::WordStart, 1, 0, &buffer)),
            "m"
        );
        assert_eq!(
            char_at(&buffer, cursor(Motion::WordStart, 2, 0, &buffer)),
            "("
        );
        // `()` is one punctuation word, so the third `w` lands on `{` (the space
        // in between is skipped, as vim does).
        assert_eq!(
            char_at(&buffer, cursor(Motion::WordStart, 3, 0, &buffer)),
            "{"
        );
        assert_eq!(
            char_at(&buffer, cursor(Motion::WordStart, 4, 0, &buffer)),
            "l"
        );
        // `W` jumps over punctuation.
        assert_eq!(
            char_at(&buffer, cursor(Motion::BigWordStart, 1, 0, &buffer)),
            "m"
        );
        assert_eq!(
            char_at(&buffer, cursor(Motion::BigWordStart, 2, 0, &buffer)),
            "{"
        );
    }

    #[test]
    fn word_back_returns_to_word_starts() {
        let buffer = buffer("hello world again");
        assert_eq!(cursor(Motion::WordBack, 1, 12, &buffer), 6);
        assert_eq!(cursor(Motion::WordBack, 2, 12, &buffer), 0);
        assert_eq!(cursor(Motion::WordBack, 1, 0, &buffer), 0);
    }

    #[test]
    fn word_end_is_inclusive() {
        let buffer = buffer("one two three");
        let end = apply(Motion::WordEnd, 1, 0, &buffer, &Env::default()).unwrap();
        assert_eq!(end.cursor, 2);
        assert!(end.inclusive);
        assert_eq!(cursor(Motion::WordEnd, 2, 0, &buffer), 6);
    }

    #[test]
    fn camel_case_splits_words() {
        let buffer = buffer("drawFrameBuffer");
        assert_eq!(cursor(Motion::WordStart, 1, 0, &buffer), 4);
        assert_eq!(
            char_at(&buffer, cursor(Motion::WordStart, 1, 0, &buffer)),
            "F"
        );
        assert_eq!(cursor(Motion::BigWordStart, 1, 0, &buffer), 15);
    }

    #[test]
    fn vertical_movement_keeps_the_column() {
        let buffer = buffer("aaaa\nb\neeee");
        let env = Env {
            wanted_col: Some(3),
            ..Env::default()
        };
        let dest = apply(Motion::Down, 1, 3, &buffer, &env).unwrap();
        assert_eq!(dest.cursor, 5, "short line keeps the wanted column");
        assert_eq!(dest.wanted_col, Some(3));
        // Back to a long line the wanted column is restored: column 3 is the
        // fourth character, at byte 10.
        let dest = apply(Motion::Down, 1, 5, &buffer, &env).unwrap();
        assert_eq!(dest.cursor, 10);
    }

    #[test]
    fn find_char_within_line() {
        let buffer = buffer("hello world\nagain");
        assert_eq!(find_char(&buffer, 0, FindDir::Forward, 'w', 1), Some(6));
        assert_eq!(find_char(&buffer, 0, FindDir::TillForward, 'w', 1), Some(5));
        assert_eq!(find_char(&buffer, 10, FindDir::Backward, 'o', 1), Some(7));
        assert_eq!(find_char(&buffer, 0, FindDir::Forward, 'z', 1), None);
        // Never crosses the line break.
        assert_eq!(find_char(&buffer, 0, FindDir::Forward, 'a', 1), None);
    }

    #[test]
    fn repeated_find_with_count() {
        let buffer = buffer("a-b-c-d");
        // `a-b-c-d`: the dashes are at 1, 3 and 5.
        assert_eq!(find_char(&buffer, 0, FindDir::Forward, '-', 2), Some(3));
        assert_eq!(find_char(&buffer, 6, FindDir::Backward, '-', 2), Some(3));
    }

    #[test]
    fn bracket_matching() {
        let buffer = buffer("fn f(a: i32) { let v = vec![1, 2]; }");
        let open = buffer.text().find('(').unwrap();
        assert_eq!(match_bracket(&buffer, open), buffer.text().find(')'));
        let brace = buffer.text().find('{').unwrap();
        assert_eq!(match_bracket(&buffer, brace), buffer.text().find('}'));
        assert_eq!(
            match_bracket(&buffer, buffer.text().find('}').unwrap()),
            Some(brace)
        );
    }

    #[test]
    fn paragraph_motion_matches_vim() {
        // Destinations read out of `nvim --headless -l`, not out of a reading of
        // the docs: (start line, count, forward, expected line and column),
        // 1-based, `None` meaning the motion fails and the cursor stays put.
        let body = buffer("one\ntwo\n\nthree\n\nfour");
        check_paragraph_table(
            &body,
            &[
                (1, 1, true, Some((3, 1))),
                (1, 1, false, None),
                (1, 2, true, Some((5, 1))),
                (1, 2, false, None),
                (2, 1, true, Some((3, 1))),
                (2, 1, false, Some((1, 1))),
                (2, 2, true, Some((5, 1))),
                (2, 2, false, None),
                (3, 1, true, Some((5, 1))),
                (3, 1, false, Some((1, 1))),
                (3, 2, true, Some((6, 4))),
                (3, 2, false, None),
                (4, 1, true, Some((5, 1))),
                (4, 1, false, Some((3, 1))),
                (4, 2, true, Some((6, 4))),
                (4, 2, false, Some((1, 1))),
                (5, 1, true, Some((6, 4))),
                (5, 1, false, Some((3, 1))),
                (5, 2, true, None),
                (5, 2, false, Some((1, 1))),
                (6, 1, true, Some((6, 4))),
                (6, 1, false, Some((5, 1))),
                (6, 2, true, None),
                (6, 2, false, Some((3, 1))),
            ],
        );

        // Blank lines at both ends: the run at the start of the file is a
        // boundary too, and `}` from inside it runs to the end.
        let edges = buffer("\n\nhead\ntail\n\n\nend");
        check_paragraph_table(
            &edges,
            &[
                (1, 1, true, Some((5, 1))),
                (1, 1, false, None),
                (2, 1, true, Some((5, 1))),
                (2, 1, false, Some((1, 1))),
                (3, 1, true, Some((5, 1))),
                (3, 1, false, Some((2, 1))),
                (6, 1, true, Some((7, 3))),
                (6, 1, false, Some((2, 1))),
                (7, 1, true, Some((7, 3))),
                (7, 1, false, Some((6, 1))),
            ],
        );
    }

    /// One row of the table: start line, count, forward, expected landing.
    type ParCase = (usize, usize, bool, Option<(usize, usize)>);

    /// Run the table above against [`paragraph`], reporting failures by line.
    fn check_paragraph_table(buffer: &Buffer, cases: &[ParCase]) {
        for &(start, count, forward, expected) in cases {
            let from = buffer.line_start(start - 1);
            let direction = if forward { 1 } else { -1 };
            let keys = if forward { "}" } else { "{" };
            let Some(dest) = paragraph(buffer, from, count, direction) else {
                assert_eq!(
                    expected, None,
                    "{keys} x{count} from line {start} should not move"
                );
                continue;
            };
            let line = buffer.line_of(dest);
            let column = dest - buffer.line_start(line) + 1;
            assert_eq!(
                expected,
                Some((line + 1, column)),
                "{keys} x{count} from line {start}"
            );
        }
    }

    #[test]
    fn sentence_motion() {
        let buffer = buffer("Hello world. Second one! Third?");
        let first = sentence(&buffer, 0, 1, 1);
        assert_eq!(buffer.slice(first..first + 6), "Second");
        let back = sentence(&buffer, first + 7, 1, -1);
        assert_eq!(buffer.slice(back..back + 6), "Second");
    }

    #[test]
    fn search_wraps_around() {
        let buffer = buffer("alpha beta alpha");
        let query = SearchQuery {
            pattern: "alpha".into(),
            case_sensitive: true,
            backward: false,
        };
        assert_eq!(search_next(&buffer, 0, &query, false, 1), Some(11));
        assert_eq!(search_next(&buffer, 11, &query, false, 1), Some(0), "wraps");
        assert_eq!(
            search_next(&buffer, 0, &query, true, 1),
            Some(11),
            "backwards wraps"
        );
    }

    #[test]
    fn case_insensitive_search_maps_back_to_source_offsets() {
        let buffer = buffer("Alpha BETA gamma");
        let query = SearchQuery {
            pattern: "beta".into(),
            case_sensitive: false,
            backward: false,
        };
        let at = search_next(&buffer, 0, &query, false, 1).unwrap();
        assert_eq!(buffer.slice(at..at + 4), "BETA");
    }

    #[test]
    fn document_and_view_motions_are_linewise() {
        let buffer = buffer("  one\n  two\n  three");
        assert_eq!(
            apply(Motion::DocumentEnd, 1, 0, &buffer, &Env::default())
                .unwrap()
                .kind,
            Kind::Line
        );
        assert_eq!(
            buffer.line_of(cursor(Motion::DocumentEnd, 1, 0, &buffer)),
            2
        );
        assert_eq!(cursor(Motion::DocumentStart, 1, 20, &buffer), 2);
    }

    #[test]
    fn go_to_line_with_count() {
        let buffer = buffer("one\ntwo\nthree\nfour");
        assert_eq!(
            buffer.line_of(cursor(Motion::DocumentEnd, 3, 0, &buffer)),
            2
        );
    }

    #[test]
    fn column_motion_uses_display_columns() {
        let buffer = buffer("\tx\tz");
        // tab occupies 4 columns, so column 5 is the first 'x'
        let dest = apply(Motion::Column, 5, 0, &buffer, &Env::default()).unwrap();
        assert_eq!(buffer.slice(dest.cursor..dest.cursor + 1), "x");
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        let buffer = buffer("汉a");
        assert_eq!(buffer.display_col(0), 0);
        // 汉 is two cells wide, so `a` sits at column 2.
        assert_eq!(buffer.display_col(3), 2);
        assert_eq!(buffer.byte_at_display_col(0, 2), 3);
        assert_eq!(buffer.slice(0..3), "汉");
    }
}
