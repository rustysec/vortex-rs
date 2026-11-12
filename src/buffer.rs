//! Rope backed text buffers: editing, undo history, file I/O.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ropey::Rope;

use crate::config::EditorConfig;
use crate::error::{Error, Result};
use crate::lang::{self, LanguageInfo};
use crate::selection::{Edit, Selection, Span};
use crate::syntax::SyntaxState;

/// How many applied changes a buffer tracks before it gives up and asks the
/// language client to resync the whole document.
///
/// A buffer nobody drains — LSP off, or a long run of edits between ticks —
/// must not grow this without bound.  The cap is generous: a tick is 30ms, so
/// reaching it means something is wrong rather than that the user typed fast.
const MAX_PENDING_CHANGES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(pub u32);

/// Why a change happened — drives undo grouping and `.` repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeGroup {
    /// Typing in insert mode; consecutive inserts share one undo step.
    Insert,
    /// An operator (`d`, `c`, `>`, …).
    Operator,
    /// A command (`:write`, `:delete`, paste, …).
    Command,
    /// An automatic formatter rewrote the buffer.
    Format,
    /// Loaded from disk / created.
    Initial,
}

impl ChangeGroup {
    fn groups_with(self, other: Self) -> bool {
        self == other && matches!(self, ChangeGroup::Insert)
    }
}

/// A replacement of `range` with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub range: Span,
    pub text: String,
}

/// One applied change, as LSP positions in the document *before* it landed.
///
/// Byte offsets cannot be replayed later: by the time the language client drains
/// them the rope has moved on, and an offset that was valid then may now be
/// inside a different line.  Positions are captured at apply time instead, which
/// is also exactly what `contentChanges` wants — each entry is applied to the
/// document the previous one produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedChange {
    /// `(line, UTF-16 column)` of the start, 0-based.
    pub start: (usize, usize),
    /// `(line, UTF-16 column)` of the end, 0-based.
    pub end: (usize, usize),
    pub text: String,
}

/// `(line, UTF-16 column)` of a byte offset, the way LSP counts positions.
fn utf16_position(rope: &Rope, byte: usize) -> (usize, usize) {
    let byte = floor_boundary(rope, byte.min(rope.len_bytes()));
    let line = if rope.len_bytes() == 0 {
        0
    } else {
        rope.byte_to_line(byte)
    };
    let line_start = rope.line_to_byte(line);
    let column = rope
        .slice(rope.byte_to_char(line_start)..rope.byte_to_char(byte))
        .chars()
        .map(char::len_utf16)
        .sum();
    (line, column)
}

impl Change {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: Span::point(at),
            text: text.into(),
        }
    }

    pub fn replace(range: impl Into<Span>, text: impl Into<String>) -> Self {
        Self {
            range: range.into(),
            text: text.into(),
        }
    }

    pub fn delete(range: impl Into<Span>) -> Self {
        Self {
            range: range.into(),
            text: String::new(),
        }
    }

    fn is_noop(&self) -> bool {
        self.range.is_empty() && self.text.is_empty()
    }
}

/// One undo step: the text it removed, the text it inserted, and the selections
/// on either side of it.
#[derive(Debug, Clone)]
struct UndoItem {
    group: ChangeGroup,
    at: Instant,
    changes: Vec<UndoChange>,
    before: Selection,
    after: Selection,
    /// Redo replays a coalesced step oldest first, so this is set on every item
    /// of a step except the one that ends it.  See [`Buffer::undo`].
    redo_more: bool,
}

#[derive(Debug, Clone)]
struct UndoChange {
    at: usize,
    removed: String,
    inserted: String,
}

/// Undo / redo stacks.
#[derive(Debug, Clone)]
pub struct History {
    undo: Vec<UndoItem>,
    redo: Vec<UndoItem>,
    limit: usize,
    group_timeout: std::time::Duration,
}

impl History {
    fn new(config: &EditorConfig) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: config.undo_limit.max(16),
            group_timeout: config.undo_group_timeout.to_std(),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Undo steps: user changes, with the formatter runs stacked on top of each
    /// one walked back by the same press (see [`Buffer::undo`]).
    pub fn undo_depth(&self) -> usize {
        self.undo
            .iter()
            .filter(|item| item.group != ChangeGroup::Format)
            .count()
    }

    /// Redo steps, counting a coalesced batch once.
    pub fn redo_depth(&self) -> usize {
        self.redo.iter().filter(|item| !item.redo_more).count()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// How a file ended its lines, so saving preserves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

/// A loaded file: path, text, and the conventions we need to preserve.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub text: String,
    pub line_ending: LineEnding,
    pub final_newline: bool,
    pub had_invalid_utf8: bool,
}

/// A text buffer.  One file (or scratch buffer) shared by any number of panes.
#[derive(Debug)]
pub struct Buffer {
    pub id: BufferId,
    path: Option<PathBuf>,
    pub rope: Rope,
    pub language: &'static LanguageInfo,
    /// Bumped by every change so background jobs can detect stale work.
    pub version: u64,
    saved_version: u64,
    pub read_only: bool,
    pub line_ending: LineEnding,
    pub final_newline: bool,
    pub had_invalid_utf8: bool,
    pub syntax: SyntaxState,
    /// Diagnostics from the language client and the spell checker.
    pub diagnostics: Vec<crate::diagnostic::Diagnostic>,
    /// Named marks (`m{a-zA-Z}`), byte offsets.
    pub marks: std::collections::HashMap<char, usize>,
    history: History,
    last_group_at: Option<Instant>,
    /// Where the last change happened, for `` `. `` and `gi`.
    pub last_change_pos: Option<usize>,
    /// mtime of the file the last time the buffer matched it, so `:write` can
    /// tell an outside edit from nothing at all.
    disk_mtime: Option<std::time::SystemTime>,
    /// Changes applied since the last [`Buffer::take_changes`], in the order
    /// they were applied.
    ///
    /// The language client needs the edits themselves, not just the fact that
    /// the version moved: incremental sync sends a range per change, and the
    /// version number alone cannot say where the text went.  Positions are
    /// captured at apply time because byte offsets do not survive the edit.
    pending_changes: Vec<RecordedChange>,
    /// Set when `pending_changes` overflowed, so the next drain reports "send
    /// the whole document" instead of a partial list of edits.
    changes_overflowed: bool,
}

impl Buffer {
    /// An empty scratch buffer.
    pub fn new(id: BufferId, path: Option<PathBuf>, text: String, config: &EditorConfig) -> Self {
        let first_line = text.lines().next();
        let language = lang::detect(path.as_deref(), first_line);
        let rope = Rope::from(text.as_str());
        let line_ending = if text.contains("\r\n") {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        };
        let final_newline = text.ends_with('\n');
        let disk_mtime = file_mtime(path.as_deref());
        Self {
            id,
            path,
            rope,
            language,
            version: 1,
            saved_version: 1,
            read_only: false,
            line_ending,
            final_newline,
            had_invalid_utf8: false,
            syntax: SyntaxState::default(),
            diagnostics: Vec::new(),
            marks: std::collections::HashMap::new(),
            history: History::new(config),
            last_group_at: None,
            last_change_pos: None,
            disk_mtime,
            pending_changes: Vec::new(),
            changes_overflowed: false,
        }
    }

    /// Read a file from disk.
    pub fn open(id: BufferId, path: &Path, config: &EditorConfig) -> Result<Self> {
        let loaded = load_file(path)?;
        let mut buffer = Self::new(id, Some(loaded.path.clone()), loaded.text, config);
        buffer.line_ending = loaded.line_ending;
        buffer.final_newline = loaded.final_newline;
        buffer.had_invalid_utf8 = loaded.had_invalid_utf8;
        buffer.saved_version = buffer.version;
        Ok(buffer)
    }

    /// Read a file, mapping "it does not exist" to a fresh empty buffer.
    pub fn open_or_new(id: BufferId, path: &Path, config: &EditorConfig) -> Result<Self> {
        match Self::open(id, path, config) {
            Ok(buffer) => Ok(buffer),
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::new(
                id,
                Some(path.to_path_buf()),
                String::new(),
                config,
            )),
            Err(err) => Err(err),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: PathBuf) {
        let first_line = self.line_str(0);
        self.language = lang::detect(Some(&path), Some(&first_line));
        self.path = Some(path);
    }

    /// Name shown in pane labels and the status area.
    pub fn display_name(&self) -> String {
        match &self.path {
            Some(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            None => "*scratch*".into(),
        }
    }

    /// Shorter label used when the pane is narrow.
    pub fn label(&self) -> String {
        self.display_name()
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Number of lines; an empty buffer still has one empty line.
    /// Does the text end with a line break?
    pub fn ends_with_newline(&self) -> bool {
        let len = self.len_bytes();
        len > 0 && self.char_at(len - 1) == Some('\n')
    }

    /// Lines as a reader counts them: text ending in a line break does not have
    /// a trailing empty line, so the editor draws `~` there.
    pub fn display_line_count(&self) -> usize {
        let total = self.line_count();
        if total > 1 && self.ends_with_newline() {
            total - 1
        } else {
            total
        }
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines().max(1)
    }

    /// The last line the cursor can occupy.
    ///
    /// A trailing newline makes the rope report one more (empty) line than the
    /// pane ever draws, so motions and scrolling stop here: vim puts `G` on the
    /// last line with text, not on the phantom row after it.
    pub fn last_line(&self) -> usize {
        self.display_line_count().saturating_sub(1)
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Text of a byte range.
    ///
    /// `Rope::slice` works in *characters*, so the byte offsets are converted —
    /// getting that wrong silently corrupts non-ASCII text.
    pub fn slice(&self, span: impl Into<Span>) -> String {
        slice_bytes(&self.rope, span.into().clamp(self.len_bytes()))
    }

    // ---------------------------------------------------------------- edits

    /// Apply changes, record undo information and map the selection through them.
    ///
    /// Changes may be given in any order; overlapping changes are merged.
    pub fn apply(
        &mut self,
        changes: Vec<Change>,
        group: ChangeGroup,
        selection: &mut Selection,
    ) -> Vec<Edit> {
        let changes = normalise_changes(changes, &self.rope);
        if changes.is_empty() {
            return Vec::new();
        }

        let before = selection.clone();
        let mut undo_changes = Vec::with_capacity(changes.len());
        let mut edits = Vec::with_capacity(changes.len());

        // Apply back to front so earlier byte offsets stay valid.
        for change in changes.iter().rev() {
            // Positions are captured *before* the rope moves: a byte offset
            // recorded now would point somewhere else by the time the language
            // client drains it.
            let start = utf16_position(&self.rope, change.range.start);
            let end = utf16_position(&self.rope, change.range.end);
            let removed = slice_bytes(&self.rope, change.range);
            replace(&mut self.rope, change.range, &change.text);
            undo_changes.push(UndoChange {
                at: change.range.start,
                removed,
                inserted: change.text.clone(),
            });
            edits.push(Edit::new(change.range, change.text.len()));
            // Recorded in application order (highest offset first), which is
            // also the order LSP wants: each `contentChanges` entry is applied
            // to the document the previous one produced, so a range at a lower
            // offset stays valid only after the higher ones have landed.
            self.pending_changes.push(RecordedChange {
                start,
                end,
                text: change.text.clone(),
            });
        }
        undo_changes.reverse();
        edits.reverse();

        // A buffer nobody drains (LSP off, or a long run of edits) must not grow
        // this without bound.  Past the cap the ranges are dropped and the
        // client is told to resync the whole document instead.
        if self.pending_changes.len() > MAX_PENDING_CHANGES {
            self.pending_changes.clear();
            self.changes_overflowed = true;
        }

        self.version += 1;
        self.last_change_pos = Some(changes[0].range.start);

        let after = selection.map(&edits);
        self.record(group, undo_changes, before, after.clone());
        *selection = after;
        edits
    }

    /// Take the changes applied since the last call, for incremental sync.
    ///
    /// `None` means the changes were dropped because there were too many to
    /// track: the caller has to send the whole document instead.  An empty
    /// vector means nothing changed.
    ///
    /// The changes come back in the order they were applied, which is the order
    /// LSP applies a `contentChanges` array.
    pub fn take_changes(&mut self) -> Option<Vec<RecordedChange>> {
        if self.changes_overflowed {
            self.changes_overflowed = false;
            self.pending_changes.clear();
            return None;
        }
        Some(std::mem::take(&mut self.pending_changes))
    }

    fn record(
        &mut self,
        group: ChangeGroup,
        changes: Vec<UndoChange>,
        before: Selection,
        after: Selection,
    ) {
        if matches!(group, ChangeGroup::Initial) {
            return;
        }
        let now = Instant::now();
        let merge = self
            .last_group_at
            .is_some_and(|at| now.duration_since(at) < self.history.group_timeout)
            && self
                .history
                .undo
                .last()
                .is_some_and(|item| item.group.groups_with(group));

        if merge && let Some(item) = self.history.undo.last_mut() {
            item.changes.extend(changes);
            item.after = after;
            item.at = now;
            self.last_group_at = Some(now);
            self.history.redo.clear();
            return;
        }

        self.history.undo.push(UndoItem {
            group,
            at: now,
            changes,
            before,
            after,
            redo_more: false,
        });
        if self.history.undo.len() > self.history.limit {
            let overflow = self.history.undo.len() - self.history.limit;
            self.history.undo.drain(0..overflow);
        }
        self.history.redo.clear();
        self.last_group_at = Some(now);
    }

    /// Close the current undo group (leaving insert mode, running a command, …).
    pub fn break_undo_group(&mut self) {
        self.last_group_at = None;
    }

    /// Undo one step; returns the selection to restore.
    ///
    /// A formatter run is not something the user did, so it never costs a `u` of
    /// its own: the `Format` entries on top of the stack are pulled into the edit
    /// underneath them and walked back in the same press.  That is what "back to
    /// before I did that" means when the buffer was reformatted after the edit —
    /// undoing only the edit would leave the text in a state nobody typed.
    pub fn undo(&mut self) -> Option<UndoOutcome> {
        // Newest first: the automatic runs sit on top of the change they follow.
        let mut batch: Vec<UndoItem> = Vec::new();
        while self
            .history
            .undo
            .last()
            .is_some_and(|item| item.group == ChangeGroup::Format)
        {
            batch.push(self.history.undo.pop()?);
        }
        if let Some(item) = self.history.undo.pop() {
            batch.push(item);
        }
        if batch.is_empty() {
            return None;
        }
        // An item's offsets are only valid for the text its own apply produced,
        // so the newest has to go back first.
        for item in batch.iter() {
            for change in item.changes.iter().rev() {
                let range = Span::new(change.at, change.at + change.inserted.len());
                replace(&mut self.rope, range, &change.removed);
            }
        }
        self.version += 1;
        let restored = batch.last().expect("a non-empty batch").before.clone();
        // Redo walks forward, so the oldest item has to end up on top; every
        // item but the one that ends the step tells redo to keep going.
        for (index, mut item) in batch.into_iter().enumerate() {
            item.redo_more = index > 0;
            self.history.redo.push(item);
        }
        self.last_group_at = None;
        Some(UndoOutcome {
            selection: restored,
            action: "undo",
        })
    }

    /// Redo one step; returns the selection to restore.
    pub fn redo(&mut self) -> Option<UndoOutcome> {
        let first = self.history.redo.pop()?;
        // Oldest first: the step `undo` coalesced comes back as one step.
        let mut batch = vec![first];
        while batch.last().is_some_and(|item| item.redo_more) {
            match self.history.redo.pop() {
                Some(next) => batch.push(next),
                None => break,
            }
        }
        for item in batch.iter() {
            for change in item.changes.iter() {
                let range = Span::new(change.at, change.at + change.removed.len());
                replace(&mut self.rope, range, &change.inserted);
            }
        }
        self.version += 1;
        let restored = batch.last().expect("a non-empty batch").after.clone();
        for item in batch {
            self.history.undo.push(item);
        }
        self.last_group_at = None;
        Some(UndoOutcome {
            selection: restored,
            action: "redo",
        })
    }

    /// Steps left on the undo stack, for the `:undolist`-style message.
    pub fn undo_depth(&self) -> usize {
        self.history.undo_depth()
    }

    /// Steps left on the redo stack.
    pub fn redo_depth(&self) -> usize {
        self.history.redo_depth()
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Replace the whole buffer, preserving cursor position through a diff.
    pub fn set_text(&mut self, text: &str, group: ChangeGroup, selection: &mut Selection) -> bool {
        let changes = crate::format::diff_changes(&self.rope, text);
        if changes.is_empty() {
            return false;
        }
        self.apply(changes, group, selection);
        true
    }

    // ------------------------------------------------------------- file I/O

    /// Mark the buffer as saved at `path` without writing (used by tests/undo).
    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
        self.disk_mtime = file_mtime(self.path.as_deref());
    }

    /// Write the buffer to disk.  Returns `None` when there is nothing to write.
    pub fn save(&mut self) -> Result<Option<SaveOutcome>> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| Error::message("no file name; use :write <path>"))?;
        if !self.is_dirty() {
            return Ok(None);
        }
        write_file(&path, &self.content_for_disk())?;
        self.saved_version = self.version;
        self.disk_mtime = file_mtime(Some(&path));
        Ok(Some(SaveOutcome {
            path,
            lines: self.display_line_count(),
            bytes: self.rope.len_bytes(),
        }))
    }

    /// Write to a new path, adopting it as the buffer's file.
    pub fn save_as(&mut self, path: &Path) -> Result<SaveOutcome> {
        let path = expand_home(path);
        write_file(&path, &self.content_for_disk())?;
        let outcome = SaveOutcome {
            path: path.clone(),
            lines: self.display_line_count(),
            bytes: self.rope.len_bytes(),
        };
        self.path = Some(path);
        self.saved_version = self.version;
        self.disk_mtime = file_mtime(self.path.as_deref());
        self.read_only = false;
        Ok(outcome)
    }

    /// The bytes that belong on disk: original line endings restored.
    pub fn content_for_disk(&self) -> Vec<u8> {
        let text = self.rope.to_string();
        let mut out = match self.line_ending {
            LineEnding::Lf => text.into_bytes(),
            LineEnding::CrLf => text
                .replace("\r\n", "\n")
                .replace('\n', "\r\n")
                .into_bytes(),
        };
        if !self.final_newline {
            while out.last() == Some(&b'\n') {
                out.pop();
            }
            if self.line_ending == LineEnding::CrLf && out.ends_with(b"\r") {
                out.pop();
            }
        } else if !out.is_empty() && !out.ends_with(b"\n") {
            match self.line_ending {
                LineEnding::Lf => out.push(b'\n'),
                LineEnding::CrLf => out.extend_from_slice(b"\r\n"),
            }
        }
        out
    }

    /// Re-read the file from disk, discarding unsaved changes.
    pub fn reload(&mut self, selection: &mut Selection) -> Result<usize> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| Error::message("buffer has no file to reload"))?;
        let loaded = load_file(&path)?;
        let bytes = loaded.text.len();
        self.rope = Rope::from(loaded.text.as_str());
        self.line_ending = loaded.line_ending;
        self.final_newline = loaded.final_newline;
        self.had_invalid_utf8 = loaded.had_invalid_utf8;
        self.version += 1;
        self.saved_version = self.version;
        self.history.clear();
        self.last_group_at = None;
        self.syntax.invalidate();
        self.disk_mtime = file_mtime(Some(&path));
        selection.collapse(0);
        Ok(bytes)
    }

    /// True when the file on disk is not the one this buffer was read from or
    /// written to — and the buffer has edits that would be lost.
    pub fn file_modified_on_disk(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        match (file_mtime(Some(path.as_path())), self.disk_mtime) {
            (Some(current), Some(recorded)) => current != recorded,
            // Never compared against a file we saved: only speak up if there is
            // something unsaved to clobber.
            (Some(_), None) => self.is_dirty(),
            (None, _) => false,
        }
    }

    // -------------------------------------------------------- text queries

    /// Clamp to a valid character boundary in the buffer.
    pub fn clamp_byte(&self, byte: usize) -> usize {
        floor_boundary(&self.rope, byte)
    }

    pub fn line_of(&self, byte: usize) -> usize {
        if self.rope.len_bytes() == 0 {
            return 0;
        }
        self.rope.byte_to_line(self.floor_byte(byte))
    }

    /// First byte of `line`.
    pub fn line_start(&self, line: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        self.rope.line_to_byte(line)
    }

    /// Last byte of `line`, excluding the line ending.
    pub fn line_end(&self, line: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let start = self.rope.line_to_byte(line);
        let slice = self.rope.line(line);
        let mut end = start + slice.len_bytes();
        // `as_str()` only works when the slice lives in one chunk, so copy.
        let text = slice.to_string();
        if text.ends_with('\n') {
            end -= 1;
            if text.ends_with("\r\n") {
                end -= 1;
            }
        }
        end
    }

    /// Byte range of the whole line including its line ending.
    pub fn line_range_with_ending(&self, line: usize) -> Span {
        let line = line.min(self.line_count().saturating_sub(1));
        let start = self.rope.line_to_byte(line);
        let end = if line + 1 >= self.line_count() {
            self.rope.len_bytes()
        } else {
            self.rope.line_to_byte(line + 1)
        };
        Span::new(start, end)
    }

    pub fn line_str(&self, line: usize) -> String {
        let text = self.rope.line(line.min(self.line_count() - 1)).to_string();
        text.trim_end_matches(['\r', '\n']).to_string()
    }

    /// Byte offset of the first non-whitespace character, or the line end.
    pub fn first_non_blank(&self, line: usize) -> usize {
        let start = self.line_start(line);
        let end = self.line_end(line);
        let text = self.slice(start..end);
        match text.char_indices().find(|(_, c)| !c.is_whitespace()) {
            Some((offset, _)) => start + offset,
            None => end,
        }
    }

    pub fn is_line_blank(&self, line: usize) -> bool {
        let start = self.line_start(line);
        let end = self.line_end(line);
        self.slice(start..end).chars().all(char::is_whitespace)
    }

    /// Leading whitespace of a line.
    pub fn indent_str(&self, line: usize) -> String {
        let start = self.line_start(line);
        let end = self.line_end(line);
        let text = self.slice(start..end);
        let indent: String = text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        indent
    }

    pub fn indent_width(&self, line: usize) -> usize {
        display_width(&self.indent_str(line), self.tab_width())
    }

    pub fn tab_width(&self) -> usize {
        self.language.indent_width.max(1) as usize
    }

    /// Character index of `byte` within its line.
    pub fn char_in_line(&self, byte: usize) -> usize {
        let byte = self.floor_byte(byte);
        let line = self.line_of(byte);
        let start = self.line_start(line);
        self.rope.byte_to_char(byte) - self.rope.byte_to_char(start)
    }

    /// UTF-16 column of `byte` within its line, the inverse of
    /// [`Buffer::byte_at_utf16`].
    ///
    /// LSP positions are UTF-16 code units, so a byte offset has to be converted
    /// before it can be sent to a server.
    pub fn utf16_col_in_line(&self, byte: usize) -> usize {
        let byte = self.floor_byte(byte);
        let line = self.line_of(byte);
        let start = self.line_start(line);
        self.slice(Span::new(start, byte))
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    /// `byte`, moved back to the start of the character containing it.
    ///
    /// ropey panics when handed an offset inside a character, and callers pass
    /// whatever a motion or a diagnostic produced.
    fn floor_byte(&self, byte: usize) -> usize {
        floor_boundary(&self.rope, self.clamp_byte(byte))
    }

    pub fn byte_at_char_in_line(&self, line: usize, char_col: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let start_char = self.rope.byte_to_char(self.line_start(line));
        let end_char = self.rope.byte_to_char(self.line_end(line));
        let target = start_char + char_col.min(end_char - start_char);
        self.rope.char_to_byte(target)
    }

    /// Byte offset of an LSP position: `line` is 0-based, `character` counts
    /// UTF-16 code units from the start of the line.
    ///
    /// LSP measures columns in UTF-16 units, not bytes or chars, so a line with
    /// any non-ASCII text puts every later column at a different byte offset.
    /// A `character` that lands inside a surrogate pair clamps to the start of
    /// that character.
    pub fn byte_at_utf16(&self, line: usize, character: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let start = self.line_start(line);
        let end = self.line_end(line);
        start + utf16_col_to_byte(&self.slice(Span::new(start, end)), character)
    }

    /// Column of `byte` on its line, counting tabs and wide characters.
    pub fn display_col(&self, byte: usize) -> usize {
        let byte = self.clamp_byte(byte);
        let line = self.line_of(byte);
        let start = self.line_start(line);
        let text = self.slice(start..byte);
        display_width(&text, self.tab_width())
    }

    /// Byte offset closest to display column `col` on `line`.
    pub fn byte_at_display_col(&self, line: usize, col: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let start = self.line_start(line);
        let end = self.line_end(line);
        let text = self.slice(start..end);
        let mut width = 0;
        for (index, ch) in text.char_indices() {
            if width >= col {
                return start + index;
            }
            width += char_width(ch, self.tab_width(), width);
        }
        end
    }

    /// The character at `byte` (treating the cursor as sitting on it).
    pub fn char_at(&self, byte: usize) -> Option<char> {
        self.char_after(byte).map(|(_, c)| c)
    }

    /// `(byte offset, character)` of the character ending at or before `byte`.
    pub fn char_before(&self, byte: usize) -> Option<(usize, char)> {
        if byte == 0 {
            return None;
        }
        let end = self.clamp_byte(byte);
        let start = floor_boundary(&self.rope, end.saturating_sub(1));
        if start >= end {
            return None;
        }
        self.char_at_boundary(start).map(|c| (start, c))
    }

    /// `(byte offset, character)` of the character starting at or after `byte`.
    pub fn char_after(&self, byte: usize) -> Option<(usize, char)> {
        if self.clamp_byte(byte) >= self.rope.len_bytes() {
            return None;
        }
        // A byte in the middle of a character resolves forwards, to the next one.
        let start = ceil_boundary(&self.rope, byte);
        self.char_at_boundary(start).map(|c| (start, c))
    }

    /// The character whose first byte is `byte`, which must be a char boundary.
    ///
    /// `Rope::slice` is character addressed while everything else here counts in
    /// bytes, so the hop goes through the char index — mixing the two is a panic
    /// on any file containing a multi-byte character.
    fn char_at_boundary(&self, byte: usize) -> Option<char> {
        let index = self.rope.try_byte_to_char(byte).ok()?;
        self.rope.get_char(index)
    }

    /// Span of the word containing `byte`.
    pub fn word_at(&self, byte: usize) -> Span {
        let byte = self.clamp_byte(byte);
        let end = self.rope.len_bytes();
        if end == 0 {
            return Span::point(0);
        }
        let class = match self.char_after(byte) {
            Some((_, c)) => WordClass::of(c),
            None => match self.char_before(byte) {
                Some((_, c)) => WordClass::of(c),
                None => return Span::point(byte),
            },
        };
        if class == WordClass::Space {
            let mut start = byte;
            while let Some((offset, c)) = self.char_before(start) {
                if WordClass::of(c) != WordClass::Space {
                    break;
                }
                start = offset;
            }
            let mut stop = byte;
            while let Some((offset, c)) = self.char_after(stop) {
                if WordClass::of(c) != WordClass::Space {
                    break;
                }
                stop = next_char_boundary(&self.rope, offset);
            }
            return Span::new(start, stop);
        }
        let mut start = byte;
        while let Some((offset, c)) = self.char_before(start) {
            if !class.continues(c, self.char_before(offset).map(|(_, p)| p)) {
                break;
            }
            start = offset;
        }
        let mut stop = match self.char_after(byte) {
            Some((offset, _)) => next_char_boundary(&self.rope, offset),
            None => end,
        };
        while let Some((_, c)) = self.char_after(stop) {
            let previous = self.char_before(stop).map(|(_, p)| p);
            if !class.continues(c, previous) {
                break;
            }
            stop = next_char_boundary(&self.rope, stop);
        }
        Span::new(start, stop)
    }

    /// Search for `needle` within `[from..to)`, optionally case insensitively.
    pub fn find(&self, needle: &str, from: usize, to: usize, case_sensitive: bool) -> Option<Span> {
        if needle.is_empty() {
            return None;
        }
        let from = self.clamp_byte(from);
        let to = self.clamp_byte(to).max(from);
        let haystack = self.slice(from..to);
        if case_sensitive {
            return haystack
                .find(needle)
                .map(|i| Span::new(from + i, from + i + needle.len()));
        }

        // Lowercase the haystack but keep a map from lowered byte index back to
        // the original byte offset, so matches map out exactly.
        let needle_lower = needle.to_lowercase();
        let mut lowered = String::with_capacity(haystack.len());
        let mut offsets: Vec<usize> = Vec::with_capacity(haystack.len());
        for (index, ch) in haystack.char_indices() {
            for low in ch.to_lowercase() {
                let start = lowered.len();
                lowered.push(low);
                for _ in start..lowered.len() {
                    offsets.push(index);
                }
            }
        }
        lowered.find(&needle_lower).map(|at| {
            let start = from + offsets[at];
            let end_index = (at + needle_lower.len()).min(offsets.len());
            let end = from + offsets.get(end_index).copied().unwrap_or(haystack.len());
            Span::new(start, end)
        })
    }

    /// The word under the cursor, for `*` and `#`.
    pub fn word_under_cursor(&self, byte: usize) -> Option<String> {
        let span = self.word_at(byte);
        if span.is_empty() {
            return None;
        }
        let text = self.slice(span);
        if text.chars().all(|c| c.is_alphanumeric() || c == '_') {
            Some(text)
        } else {
            None
        }
    }

    /// Language id used for `[format.command]` and `[lsp.servers]` lookups.
    pub fn language_id(&self) -> &'static str {
        self.language.id
    }

    /// Counts of the buffer's diagnostics, for the status area rollup.
    pub fn diagnostic_count(&self) -> crate::diagnostic::DiagnosticCount {
        crate::diagnostic::DiagnosticCount::from_diagnostics(&self.diagnostics)
    }

    /// Parse the buffer on this thread.  Used by tests and one-shot queries;
    /// the app parses asynchronously instead.
    pub fn reparse_now(&mut self) -> bool {
        crate::syntax::parse_in_place(self)
    }
}

/// Outcome of an undo or redo step.
pub struct UndoOutcome {
    pub selection: Selection,
    pub action: &'static str,
}

/// Bytes written by a successful save.
#[derive(Debug, Clone)]
pub struct SaveOutcome {
    pub path: PathBuf,
    pub lines: usize,
    pub bytes: usize,
}

impl SaveOutcome {
    pub fn summary(&self) -> String {
        format!(
            "wrote {} lines ({} bytes) to {}",
            self.lines,
            self.bytes,
            self.path.display()
        )
    }
}

/// Replace a **byte** range with `text`.
///
/// Every offset in the editor is a byte offset; ropey indexes `remove`/`insert`
/// by *character*.  Translating here, in one place, is what keeps ASCII-only
/// text from hiding the mistake: on a buffer containing `é`, byte 20 is not
/// character 20, and the wrong span gets cut out.
fn replace(rope: &mut Rope, range: impl Into<Span>, text: &str) {
    let span = range.into();
    let start = ceil_boundary(rope, span.start);
    let end = floor_boundary(rope, span.end).max(start);
    if end > start {
        let chars = rope.byte_to_char(start)..rope.byte_to_char(end);
        rope.remove(chars);
    }
    if !text.is_empty() {
        let index = rope.byte_to_char(start.min(rope.len_bytes()));
        rope.insert(index, text);
    }
}

/// The text of a byte range, snapped to character boundaries.
fn slice_bytes(rope: &Rope, range: Span) -> String {
    let start = ceil_boundary(rope, range.start);
    let end = floor_boundary(rope, range.end);
    if end <= start {
        return String::new();
    }
    rope.slice(rope.byte_to_char(start)..rope.byte_to_char(end))
        .to_string()
}

/// Snap, sort and de-duplicate changes.
///
/// Ranges are byte offsets, and callers can legitimately produce one that falls
/// inside a character (a motion landing mid `é`).  Snapping to boundaries
/// *here*, before anything is applied, keeps the edit, the undo log and the
/// `Edit`s handed back for selection mapping describing the same text — the
/// alternative is an undo that removes something else entirely.
///
/// Callers must not produce overlapping changes; if they do, the later change
/// is dropped rather than corrupting the buffer.
fn normalise_changes(mut changes: Vec<Change>, rope: &Rope) -> Vec<Change> {
    changes.retain(|c| !c.is_noop());
    for change in changes.iter_mut() {
        let range = change.range.clamp(rope.len_bytes());
        let start = ceil_boundary(rope, range.start);
        let end = floor_boundary(rope, range.end).max(start);
        change.range = Span::new(start, end);
    }
    changes.retain(|c| !c.is_noop());
    changes.sort_by_key(|c| (c.range.start, c.range.end));
    let mut result: Vec<Change> = Vec::with_capacity(changes.len());
    for change in changes {
        match result.last() {
            Some(last) if change.range.start < last.range.end => {
                debug_assert!(false, "overlapping changes are not supported");
            }
            _ => result.push(change),
        }
    }
    result
}

/// Expand a leading `~`.
pub fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Read a file into a normalised string plus the conventions to restore.
pub fn load_file(path: &Path) -> Result<Loaded> {
    let bytes = std::fs::read(path)?;
    let (text, had_invalid_utf8) = match String::from_utf8(bytes.clone()) {
        Ok(text) => (text, false),
        Err(err) => (
            String::from_utf8_lossy(&err.into_bytes()).into_owned(),
            true,
        ),
    };
    let line_ending = if text.contains("\r\n") {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let final_newline = text.ends_with('\n');
    let text = text.replace("\r\n", "\n");
    Ok(Loaded {
        path: path.to_path_buf(),
        text,
        line_ending,
        final_newline,
        had_invalid_utf8,
    })
}

/// Write a file atomically (same-directory temp file + rename).
/// The file's modification time, if it has one.
fn file_mtime(path: Option<&Path>) -> Option<std::time::SystemTime> {
    path.and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
}

pub fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "buffer".into());
    let temp = path.with_file_name(format!(".{file_name}.vortex.tmp"));
    std::fs::write(&temp, bytes)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(Error::Io(err))
        }
    }
}

/// Character classes used by `w`/`b`/`e` and word-wise searching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordClass {
    Space,
    Punctuation,
    Word,
}

impl WordClass {
    pub fn of(c: char) -> Self {
        if c.is_whitespace() {
            WordClass::Space
        } else if c.is_alphanumeric() || c == '_' {
            WordClass::Word
        } else {
            WordClass::Punctuation
        }
    }

    /// Should `c` extend a run that started in `self` after `previous`?
    pub fn continues(self, c: char, previous: Option<char>) -> bool {
        match self {
            WordClass::Space => c.is_whitespace(),
            WordClass::Punctuation => !c.is_whitespace() && !(c.is_alphanumeric() || c == '_'),
            WordClass::Word => {
                (c.is_alphanumeric() || c == '_')
                    && !previous
                        .map(|p| (p.is_lowercase() || p.is_numeric()) && c.is_uppercase())
                        .unwrap_or(false)
            }
        }
    }
}

/// `big word` classes: whitespace separated only.
pub fn is_big_word_char(c: char) -> bool {
    !c.is_whitespace()
}

/// Byte offset of UTF-16 column `character` within `text`.
///
/// A column inside a surrogate pair clamps to the start of that character; a
/// column past the end clamps to the end of the text.
pub fn utf16_col_to_byte(text: &str, character: usize) -> usize {
    let mut units = 0usize;
    for (offset, ch) in text.char_indices() {
        if units >= character {
            return offset;
        }
        let next = units + ch.len_utf16();
        if next > character {
            return offset;
        }
        units = next;
    }
    text.len()
}

/// Display width of a string, expanding tabs to `tab_width` stops.
pub fn display_width(text: &str, tab_width: usize) -> usize {
    let mut width = 0;
    for ch in text.chars() {
        width += char_width(ch, tab_width, width);
    }
    width
}

/// Display width of one character at column `col`.
pub fn char_width(ch: char, tab_width: usize, col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    match ch {
        '\t' => {
            let tab = tab_width.max(1);
            tab - (col % tab)
        }
        '\n' | '\r' => 0,
        c if c.is_control() => 1,
        c => UnicodeWidthChar::width(c).unwrap_or(0).max(1),
    }
}

/// Is `byte` the start of a character?  The end of the text counts.
///
/// `Rope::try_byte_to_char` does *not* answer this — it happily rounds an
/// interior byte down — so the check goes through the chunk's own `str`.
fn is_boundary(rope: &Rope, byte: usize) -> bool {
    if byte == 0 || byte >= rope.len_bytes() {
        return true;
    }
    let (chunk, start_byte, _, _) = rope.chunk_at_byte(byte);
    chunk.is_char_boundary(byte - start_byte)
}

/// Smallest character boundary strictly after `byte`.
pub fn next_char_boundary(rope: &Rope, byte: usize) -> usize {
    ceil_boundary(rope, byte.saturating_add(1))
}

/// Largest character boundary at or before `byte`.
pub fn floor_boundary(rope: &Rope, byte: usize) -> usize {
    let mut candidate = byte.min(rope.len_bytes());
    while !is_boundary(rope, candidate) {
        candidate -= 1;
    }
    candidate
}

/// Smallest character boundary at or after `byte`.
pub fn ceil_boundary(rope: &Rope, byte: usize) -> usize {
    let mut candidate = byte.min(rope.len_bytes());
    while !is_boundary(rope, candidate) {
        candidate += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selection;

    fn temp_path(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "vortex-buffer-test-{}-{label}-{unique}",
            std::process::id()
        ))
    }

    fn buffer_with(text: &str) -> Buffer {
        Buffer::new(
            BufferId(1),
            None,
            text.to_string(),
            &EditorConfig::default(),
        )
    }

    #[test]
    fn editing_at_every_byte_offset_of_multibyte_text_round_trips() {
        // Byte offsets inside `é`, `…` and `漢` used to be applied at the wrong
        // place because the rope counts characters.
        let source = "aé…漢 b\ncöd\n";
        for offset in 0..=source.len() {
            let mut buffer = buffer_with(source);
            let mut selection = Selection::point(offset);
            buffer.apply(
                vec![Change::insert(offset, "øx")],
                ChangeGroup::Insert,
                &mut selection,
            );
            assert!(
                buffer.text().contains("øx"),
                "insert at byte {offset} did not land"
            );
            buffer.undo().expect("undo the insert");
            assert_eq!(buffer.text(), source, "undo of an insert at byte {offset}");
            buffer.redo().expect("redo the insert");
            assert!(buffer.text().contains("øx"), "redo at byte {offset}");

            // Delete from this offset to the end, then undo.
            let mut buffer = buffer_with(source);
            let mut selection = Selection::point(offset);
            buffer.apply(
                vec![Change::delete(offset..source.len())],
                ChangeGroup::Operator,
                &mut selection,
            );
            if buffer.undo_depth() == 0 {
                // The offset snapped to the end of the text, so there was
                // nothing to delete and nothing to record.
                assert_eq!(offset, source.len(), "no-op delete at byte {offset}");
                continue;
            }
            buffer.undo().expect("undo the delete");
            assert_eq!(buffer.text(), source, "undo of a delete from byte {offset}");
        }
    }

    #[test]
    fn edits_on_multibyte_text_move_whole_characters() {
        // The ellipsis is bytes 6..9, the kanji three bytes each.  Every offset
        // below is a byte offset while the rope counts characters.
        let mut buffer = buffer_with("alpha … beta 漢字\n");
        let mut selection = Selection::point(6);
        buffer.apply(
            vec![Change::delete(6..9)],
            ChangeGroup::Operator,
            &mut selection,
        );
        assert_eq!(buffer.text(), "alpha  beta 漢字\n");

        let at = buffer.text().find("漢字").expect("kanji still there");
        buffer.apply(
            vec![Change::insert(at, "ok ")],
            ChangeGroup::Insert,
            &mut selection,
        );
        assert_eq!(buffer.text(), "alpha  beta ok 漢字\n");

        buffer.undo().expect("undo the insert");
        buffer.undo().expect("undo the delete");
        assert_eq!(buffer.text(), "alpha … beta 漢字\n");

        buffer.redo().expect("redo the delete");
        buffer.redo().expect("redo the insert");
        assert_eq!(buffer.text(), "alpha  beta ok 漢字\n");
    }

    #[test]
    fn character_reads_count_bytes_not_characters() {
        // `…` is 3 bytes, `é` 2, `漢` 3 — the offsets below are byte offsets,
        // and handing them to a character-indexed rope slice panics.
        let buffer = buffer_with("a…é漢\nnext…\n");
        assert_eq!(buffer.char_at(0), Some('a'));
        assert_eq!(buffer.char_at(1), Some('\u{2026}'));
        assert_eq!(
            buffer.char_at(2),
            Some('é'),
            "a byte inside a character resolves forwards"
        );
        assert_eq!(buffer.char_at(5), Some('漢'));
        assert_eq!(buffer.char_before(1), Some((0, 'a')));
        // The character before byte 4 (the start of `é`) is the ellipsis.
        assert_eq!(buffer.char_before(4), Some((1, '\u{2026}')));
        // A byte inside a character floors to that character first, so the one
        // before it is what precedes the whole character.
        assert_eq!(buffer.char_before(2), Some((0, 'a')));
        assert_eq!(buffer.char_before(0), None);
        assert_eq!(buffer.char_after(buffer.len_bytes()), None);
        assert!(buffer.ends_with_newline());
        assert_eq!(buffer.display_line_count(), 2);
    }

    #[test]
    fn an_empty_buffer_has_no_last_character() {
        let buffer = buffer_with("");
        assert_eq!(buffer.char_at(0), None);
        assert_eq!(buffer.char_before(0), None);
        assert_eq!(buffer.char_after(0), None);
        assert!(!buffer.ends_with_newline());
        assert_eq!(buffer.display_line_count(), 1);
    }

    fn insert(buffer: &mut Buffer, at: usize, text: &str, group: ChangeGroup) {
        let mut selection = Selection::point(at);
        buffer.apply(vec![Change::insert(at, text)], group, &mut selection);
    }

    #[test]
    fn open_and_save_round_trip() {
        let path = temp_path("round-trip");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let config = EditorConfig::default();
        let mut buffer = Buffer::open(BufferId(1), &path, &config).unwrap();
        assert_eq!(buffer.text(), "one\ntwo\n");
        assert!(!buffer.is_dirty());

        insert(&mut buffer, 0, "zero\n", ChangeGroup::Insert);
        assert!(buffer.is_dirty());
        let outcome = buffer.save().expect("writes").expect("something to write");
        assert_eq!(outcome.lines, 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zero\none\ntwo\n");
        assert!(!buffer.is_dirty());
        // A second save has nothing to do.
        assert!(buffer.save().expect("writes").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn outside_edits_are_noticed_before_overwriting() {
        let path = temp_path("mtime");
        std::fs::write(&path, "one\n").unwrap();
        let config = EditorConfig::default();
        let mut buffer = Buffer::open(BufferId(1), &path, &config).unwrap();
        assert!(!buffer.file_modified_on_disk(), "just read, so it agrees");

        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(&path, "someone else\n").unwrap();
        assert!(
            buffer.file_modified_on_disk(),
            "an outside edit should be noticed"
        );

        buffer.reload(&mut Selection::point(0)).unwrap();
        assert_eq!(buffer.text(), "someone else\n");
        assert!(!buffer.file_modified_on_disk());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn slices_are_byte_safe_with_multibyte_text() {
        let buffer = buffer_with("héllo wörld\n");
        let hello = buffer.slice(Span::new(0, 6));
        assert_eq!(hello, "héllo");
        // ` wörld` is 7 bytes: the umlaut takes two.
        assert_eq!(buffer.slice(Span::new(6, 13)), " wörld");
    }

    #[test]
    fn a_final_newline_does_not_add_a_display_line() {
        assert_eq!(buffer_with("one\ntwo\n").display_line_count(), 2);
        assert_eq!(buffer_with("one\ntwo").display_line_count(), 2);
        assert_eq!(buffer_with("").display_line_count(), 1);
    }

    #[test]
    fn undo_groups_consecutive_inserts() {
        let mut buffer = buffer_with("");
        insert(&mut buffer, 0, "a", ChangeGroup::Insert);
        insert(&mut buffer, 1, "b", ChangeGroup::Insert);
        insert(&mut buffer, 2, "c", ChangeGroup::Insert);
        assert_eq!(buffer.text(), "abc");
        assert_eq!(buffer.undo_depth(), 1, "typing is one undo step");
        buffer.undo();
        assert_eq!(buffer.text(), "");
    }

    /// The idle formatter rewriting the whole buffer, the way
    /// `Editor::apply_format` does: a `Format` group, then the group is broken
    /// so the next keystroke starts a fresh step.
    fn auto_format(buffer: &mut Buffer, text: &str) {
        let all = Span::new(0, buffer.len_bytes());
        let mut selection = Selection::point(0);
        buffer.apply(
            vec![Change::replace(all, text)],
            ChangeGroup::Format,
            &mut selection,
        );
        buffer.break_undo_group();
    }

    #[test]
    fn applied_changes_are_recorded_for_incremental_sync() {
        let mut buffer = buffer_with("alpha\n");
        insert(&mut buffer, 5, " beta", ChangeGroup::Insert);
        assert_eq!(buffer.text(), "alpha beta\n");

        let changes = buffer.take_changes().expect("changes were tracked");
        assert_eq!(changes.len(), 1);
        // Positions, not byte offsets: an offset recorded here would point
        // somewhere else by the time the client drains it.
        assert_eq!(changes[0].start, (0, 5));
        assert_eq!(changes[0].end, (0, 5));
        assert_eq!(changes[0].text, " beta");

        // Draining clears them: the next tick must not resend the same edit.
        assert_eq!(buffer.take_changes().expect("tracked"), Vec::new());
    }

    #[test]
    fn changes_come_back_in_application_order() {
        // LSP applies a `contentChanges` array in order, each entry against the
        // document the previous one produced.  A multi-cursor edit applies
        // highest-offset first, and that is the order the server needs.
        let mut buffer = buffer_with("one two three\n");
        let mut selection = Selection::point(0);
        buffer.apply(
            vec![
                Change::replace(Span::new(0, 3), "ONE"),
                Change::replace(Span::new(8, 13), "THREE"),
            ],
            ChangeGroup::Command,
            &mut selection,
        );
        assert_eq!(buffer.text(), "ONE two THREE\n");

        let changes = buffer.take_changes().expect("tracked");
        assert_eq!(changes.len(), 2);
        assert!(
            changes[0].start.1 > changes[1].start.1,
            "highest offset first, got {changes:?}"
        );
        assert_eq!(changes[0].start, (0, 8));
        assert_eq!(changes[0].end, (0, 13));
        assert_eq!(changes[1].start, (0, 0));
        assert_eq!(changes[1].end, (0, 3));
    }

    #[test]
    fn an_overflowing_change_log_asks_for_a_full_resync() {
        // A buffer nobody drains must not grow without bound; past the cap the
        // client is told to send the whole document instead of a partial list.
        let mut buffer = buffer_with("");
        for index in 0..(MAX_PENDING_CHANGES + 1) {
            insert(&mut buffer, index, "x", ChangeGroup::Insert);
        }
        assert_eq!(
            buffer.take_changes(),
            None,
            "an overflow reports None, not a truncated list"
        );
        // The flag is consumed, so the next drain is a normal empty one.
        assert_eq!(buffer.take_changes().expect("tracked"), Vec::new());
    }

    #[test]
    fn a_formatter_run_is_not_an_undo_step_of_its_own() {
        let mut buffer = buffer_with("let a=1;\n");
        insert(&mut buffer, 0, "let b=2;\n", ChangeGroup::Command);
        assert_eq!(buffer.text(), "let b=2;\nlet a=1;\n");
        auto_format(&mut buffer, "let b = 2;\nlet a = 1;\n");
        assert_eq!(
            buffer.undo_depth(),
            1,
            "the format rides along with the edit it followed"
        );

        // One press goes back over the formatting to before the edit: the user
        // never has to undo a change they did not make.
        buffer.undo().expect("undo");
        assert_eq!(buffer.text(), "let a=1;\n");
        assert!(buffer.undo().is_none(), "there was one change to undo");

        buffer.redo().expect("redo");
        assert_eq!(
            buffer.text(),
            "let b = 2;\nlet a = 1;\n",
            "redo brings the edit and its formatting back as one step"
        );
        assert_eq!(buffer.redo_depth(), 0);
    }

    #[test]
    fn undo_walks_edits_not_formats() {
        let mut buffer = buffer_with("a\n");
        insert(&mut buffer, 0, "b\n", ChangeGroup::Command);
        auto_format(&mut buffer, "b\na\n");
        insert(&mut buffer, 0, "c\n", ChangeGroup::Command);
        auto_format(&mut buffer, "c\nb\na\n");
        assert_eq!(buffer.undo_depth(), 2, "two edits, two presses");

        buffer.undo().expect("undo c");
        assert_eq!(buffer.text(), "b\na\n");
        buffer.undo().expect("undo b");
        assert_eq!(buffer.text(), "a\n");
        assert!(buffer.undo().is_none());

        buffer.redo().expect("redo b");
        assert_eq!(buffer.text(), "b\na\n");
        buffer.redo().expect("redo c");
        assert_eq!(buffer.text(), "c\nb\na\n");
        assert!(buffer.redo().is_none());
    }

    #[test]
    fn a_format_on_its_own_is_still_reversible() {
        // Nothing of the user's to walk back to: the format is the only thing
        // there is, and `u` still has to do something rather than sit there.
        let mut buffer = buffer_with("x  =  1;\n");
        auto_format(&mut buffer, "x = 1;\n");
        buffer.undo().expect("undo");
        assert_eq!(buffer.text(), "x  =  1;\n");
    }

    #[test]
    fn line_start_and_end_ignore_the_break() {
        let buffer = buffer_with("one\ntwo\n");
        assert_eq!(buffer.line_start(0), 0);
        assert_eq!(buffer.line_end(0), 3);
        assert_eq!(buffer.line_start(1), 4);
        assert_eq!(buffer.line_end(1), 7);
    }
}
