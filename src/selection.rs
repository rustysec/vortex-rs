//! Selections: byte ranges, cursor direction and how they survive edits.

use std::ops::Range;

/// A byte range in a buffer.  `start <= end`; an empty range is a cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start: start.min(end),
            end: start.max(end),
        }
    }

    pub const fn point(pos: usize) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }

    pub fn len(self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains(self, pos: usize) -> bool {
        pos >= self.start && pos < self.end
    }

    pub fn to_range(self) -> Range<usize> {
        self.start..self.end
    }

    pub fn clamp(self, max: usize) -> Self {
        Self {
            start: self.start.min(max),
            end: self.end.min(max),
        }
    }
}

impl From<Range<usize>> for Span {
    fn from(r: Range<usize>) -> Self {
        Self::new(r.start, r.end)
    }
}

/// Which end of a selection the cursor sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dir {
    #[default]
    Forward,
    Backward,
}

impl Dir {
    pub fn forward(self) -> bool {
        self == Dir::Forward
    }
}

/// Where a position snaps to when an edit happens on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assoc {
    /// Snap to the text before the edit.
    Before,
    /// Snap to the text after the edit.
    After,
    /// Keep the cursor at the end of the inserted text (typical for cursors).
    Mid,
}

/// A replacement of `range` with `text` — the unit of an edit, and enough to
/// map positions through the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Span,
    pub text_len: usize,
}

impl Edit {
    pub fn new(range: Span, text_len: usize) -> Self {
        Self { range, text_len }
    }

    fn delta(&self) -> i64 {
        self.text_len as i64 - self.range.len() as i64
    }

    /// Translate `pos` through this edit.
    pub fn map(&self, pos: usize, assoc: Assoc) -> usize {
        if pos < self.range.start {
            return pos;
        }
        if pos > self.range.end {
            return (pos as i64 + self.delta()).max(0) as usize;
        }
        match assoc {
            Assoc::Before => self.range.start,
            Assoc::After => self.range.start + self.text_len,
            Assoc::Mid => {
                if pos == self.range.start {
                    self.range.start
                } else {
                    self.range.start + self.text_len
                }
            }
        }
    }
}

/// Map a position through a set of edits given in ascending order.
pub fn map_position(pos: usize, edits: &[Edit], assoc: Assoc) -> usize {
    let mut pos = pos;
    for edit in edits {
        pos = edit.map(pos, assoc);
    }
    pos
}

/// One or more [`Span`]s with a primary index; a single empty span is a cursor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    spans: Vec<Span>,
    primary: usize,
    dir: Dir,
}

impl Selection {
    /// A single cursor.
    pub fn point(pos: usize) -> Self {
        Self {
            spans: vec![Span::point(pos)],
            primary: 0,
            dir: Dir::Forward,
        }
    }

    pub fn new(spans: Vec<Span>, primary: usize, dir: Dir) -> Self {
        let primary = primary.min(spans.len().saturating_sub(1));
        Self {
            spans,
            primary,
            dir,
        }
    }

    /// A selection from an anchor to a cursor (order defines direction).
    pub fn between(anchor: usize, cursor: usize) -> Self {
        let dir = if cursor >= anchor {
            Dir::Forward
        } else {
            Dir::Backward
        };
        Self {
            spans: vec![Span::new(anchor, cursor)],
            primary: 0,
            dir,
        }
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// True when there is exactly one empty span (a plain cursor).
    pub fn is_cursor(&self) -> bool {
        self.spans.len() == 1 && self.spans[0].is_empty()
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn dir(&self) -> Dir {
        self.dir
    }

    pub fn set_dir(&mut self, dir: Dir) {
        self.dir = dir;
    }

    pub fn primary(&self) -> Span {
        self.spans[self.primary]
    }

    /// Byte offset of the cursor end of the primary span.
    pub fn cursor(&self) -> usize {
        let span = self.primary();
        match self.dir {
            Dir::Forward => span.end,
            Dir::Backward => span.start,
        }
    }

    /// Byte offset of the fixed end of the primary span.
    pub fn anchor(&self) -> usize {
        let span = self.primary();
        match self.dir {
            Dir::Forward => span.start,
            Dir::Backward => span.end,
        }
    }

    pub fn set_primary(&mut self, index: usize) {
        self.primary = index.min(self.spans.len().saturating_sub(1));
    }

    /// Collapse the whole selection to a single cursor.
    pub fn collapse(&mut self, pos: usize) {
        self.spans = vec![Span::point(pos)];
        self.primary = 0;
        self.dir = Dir::Forward;
    }

    /// Move the cursor end to `cursor`, keeping the anchor (vim visual style).
    pub fn extend_to(&mut self, cursor: usize) {
        let anchor = self.anchor();
        self.dir = if cursor >= anchor {
            Dir::Forward
        } else {
            Dir::Backward
        };
        self.spans[self.primary] = Span::new(anchor, cursor);
    }

    /// Replace the primary span with an explicit selection.
    pub fn set_primary_span(&mut self, span: Span, dir: Dir) {
        self.spans[self.primary] = span;
        self.dir = dir;
    }

    /// Replace every span (used by operators acting on all selections).
    pub fn replace_spans(&mut self, spans: Vec<Span>) {
        if !spans.is_empty() {
            self.spans = spans;
            self.primary = self.primary.min(self.spans.len() - 1);
        }
    }

    /// Sort spans and merge overlaps so that they never cross.
    pub fn normalise(&mut self) {
        if self.spans.len() < 2 {
            return;
        }
        let primary_start = self.primary();
        self.spans.sort_by_key(|s| (s.start, s.end));
        let mut merged: Vec<Span> = Vec::with_capacity(self.spans.len());
        for span in self.spans.drain(..) {
            match merged.last_mut() {
                Some(last) if span.start <= last.end => {
                    last.end = last.end.max(span.end);
                }
                _ => merged.push(span),
            }
        }
        self.spans = merged;
        self.primary = self
            .spans
            .iter()
            .position(|s| *s == primary_start)
            .unwrap_or(0);
    }

    /// Map the selection through an `Edit` set.  Inserted text at a boundary is
    /// kept *outside* the selection so typing does not swallow the selection.
    /// Translate through edits.
    ///
    /// Text inserted at a boundary pushes that boundary, so a cursor ends up
    /// *after* what was just inserted at it and a selection keeps covering the
    /// same characters.
    pub fn map(&self, edits: &[Edit]) -> Self {
        let spans: Vec<Span> = self
            .spans
            .iter()
            .map(|span| {
                let start = map_position(span.start, edits, Assoc::After);
                if span.is_empty() {
                    Span::point(start)
                } else {
                    let end = map_position(span.end, edits, Assoc::After);
                    if start <= end {
                        Span { start, end }
                    } else {
                        Span::point(end)
                    }
                }
            })
            .collect();
        Self {
            spans,
            primary: self.primary,
            dir: self.dir,
        }
    }

    /// Total number of selected bytes.
    pub fn selected_bytes(&self) -> usize {
        self.spans.iter().map(|s| s.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: usize, end: usize, new_len: usize) -> Edit {
        Edit::new(Span::new(start, end), new_len)
    }

    #[test]
    fn positions_shift_after_edits() {
        let edits = [edit(0, 2, 5)];
        assert_eq!(map_position(0, &edits, Assoc::Before), 0);
        assert_eq!(map_position(1, &edits, Assoc::After), 5);
        assert_eq!(map_position(10, &edits, Assoc::After), 13);
    }

    #[test]
    fn cursor_after_insertion_at_cursor() {
        let edits = [edit(4, 4, 3)];
        let selection = Selection::point(4);
        let mapped = selection.map(&edits);
        assert_eq!(mapped.cursor(), 7);
    }

    #[test]
    fn selection_follows_text_inserted_at_its_start() {
        let edits = [edit(2, 2, 4)];
        let selection = Selection::between(2, 8);
        let mapped = selection.map(&edits);
        // Both ends move, so the same characters stay selected.
        assert_eq!(mapped.primary(), Span { start: 6, end: 12 });
    }

    #[test]
    fn direction_tracks_cursor() {
        let mut selection = Selection::point(10);
        selection.extend_to(4);
        assert_eq!(selection.dir(), Dir::Backward);
        assert_eq!(selection.cursor(), 4);
        assert_eq!(selection.anchor(), 10);
        selection.extend_to(20);
        assert_eq!(selection.dir(), Dir::Forward);
        assert_eq!(selection.cursor(), 20);
    }

    #[test]
    fn normalise_sorts_and_merges() {
        let mut selection = Selection::new(
            vec![Span::new(10, 12), Span::new(2, 5), Span::new(4, 6)],
            0,
            Dir::Forward,
        );
        selection.normalise();
        assert_eq!(selection.spans(), [Span::new(2, 6), Span::new(10, 12)]);
    }
}
