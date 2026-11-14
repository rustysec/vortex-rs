//! The editor: buffers, panes, modes and every editing operation.
//!
//! [`Editor`] owns the live configuration (so `:set` and toggles take effect
//! immediately) and turns key presses into [`Intent`]s — requests the app layer
//! fulfils by opening prompts, spawning background jobs or exiting.  Keeping that
//! boundary explicit means the whole editing model can be tested without a
//! terminal, and background work never blocks a key press.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use crate::action::{
    Action, Cmd, Direction, FindDir, Motion, Operator, SplitMode, ToggleOption, VisualKind,
};
use crate::buffer::{Buffer, BufferId, Change, ChangeGroup, expand_home, next_char_boundary};
use crate::config::{Config, LineNumber};
use crate::diagnostic::{Diagnostic, DiagnosticScope, DiagnosticSource, ProjectDiagnostic};
use crate::event::{AsyncEvent, JobId};
use crate::input::motions::{self, Dest, Env, Kind, SearchQuery};
use crate::input::operators::{self, Case, Region};
use crate::input::textobject::{self};
use crate::input::{Awaiting, KeyContext, Mode, OperatorPending, Pending, count_digit, keys};
use crate::input::{Keymaps, keymap::Lookup};
use crate::layout::{Axis, Layout, Pane, PaneId};
use crate::selection::{Dir, Selection, Span};

/// A request for the app layer to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Show a message on the transient card over the panes.
    Message(StatusLevel, String),
    /// Open a prompt (the `:` popup, search, picker).
    OpenPrompt(PromptSpec),
    /// Run an ex command line straight away (a keybound `":bnext"`-style action).
    RunCommand(String),
    /// (Re)parse `buffer` with tree-sitter.
    Parse(BufferId),
    /// Run the formatter for `buffer`.
    Format(BufferId),
    /// Ask the language server to format one byte range of `buffer`.
    RangeFormat(BufferId, crate::selection::Span),
    /// Ask the language server what is at the caret.
    Hover,
    /// Ask the language server for quick fixes at the caret.
    CodeAction,
    /// Spell check `buffer`.
    Spell(BufferId),
    /// Start a file-name search.
    FindFiles(String),
    /// Start a project-wide text search.
    Grep(SearchQuery),
    /// Copy text to the terminal clipboard (OSC-52).
    Clipboard(String),
    /// Re-read configuration from disk.
    ReloadConfig,
    /// Keep the cursor on screen after a move.
    EnsureVisible,
    /// Open a workspace file (not yet a buffer) and put the caret at `offset`.
    ///
    /// Used by project-scope diagnostic jumps: the target may live in a file
    /// vortex has no buffer for, so the app layer opens it — which also registers
    /// it with its language server — before the editor can focus it.
    OpenDiagnosticFile { path: PathBuf, offset: usize },
    /// Leave the editor.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    Info,
    Warn,
    Error,
}

/// What a prompt should collect.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptSpec {
    /// `:` command line.
    Command(String),
    /// `/` or `?` — search within this buffer.
    Search { forward: bool, initial: String },
    /// `:write <path>` style path entry.
    Path { initial: String },
    /// `G` — line number.
    GotoLine,
    /// File picker over search results.
    FindFiles(String),
    /// Project grep.
    Grep(String),
    /// Buffer picker.
    SwitchBuffer,
    /// Every `:` command, filterable.
    Palette(String),
    /// Spelling suggestions for the word under the cursor.
    Spell {
        word: String,
        span: Span,
        suggestions: Vec<String>,
    },
    /// Diagnostics of the current buffer.
    Diagnostics(DiagnosticScope),
    /// `:substitute` — a second argument line.
    Substitute(String),
}

/// A register holding yanked or deleted text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
}

impl Register {
    pub fn new(text: impl Into<String>, linewise: bool) -> Self {
        Self {
            text: text.into(),
            linewise,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Named, numbered and unnamed registers, like vim's.
#[derive(Debug, Clone)]
pub struct Registers {
    unnamed: Register,
    /// `0` holds the last yank; `1..=9` the most recent deletes.
    numbered: Vec<Register>,
    named: HashMap<char, Register>,
    /// Last search term, offered as the default at the `/` prompt.
    pub search: Option<String>,
}

impl Registers {
    fn new() -> Self {
        Self {
            unnamed: Register::default(),
            numbered: (0..10).map(|_| Register::default()).collect(),
            named: HashMap::new(),
            search: None,
        }
    }

    pub fn get(&self, name: char) -> Register {
        if let Some(index) = digit_index(name) {
            return self.numbered[index].clone();
        }
        if name == '"' {
            return self.unnamed.clone();
        }
        self.named
            .get(&name)
            .cloned()
            .unwrap_or_else(|| self.unnamed.clone())
    }

    /// Yanking replaces the target register and `0`, and never shifts numbers.
    pub fn yank(&mut self, register: Option<char>, value: Register) {
        if let Some(name) = register {
            match digit_index(name) {
                Some(index) => self.numbered[index] = value.clone(),
                None => {
                    self.named.insert(name, value.clone());
                }
            }
        }
        self.numbered[0] = value.clone();
        self.unnamed = value;
    }

    /// Deleting shifts `1..=9` down, so the last delete is recoverable.
    pub fn delete(&mut self, register: Option<char>, value: Register) {
        match register {
            Some(name) => {
                if let Some(index) = digit_index(name) {
                    self.numbered[index] = value.clone();
                } else {
                    self.named.insert(name, value.clone());
                }
            }
            None => {
                self.numbered.rotate_right(1);
                self.numbered[1] = value.clone();
            }
        }
        self.unnamed = value;
    }

    pub fn unnamed(&self) -> Register {
        self.unnamed.clone()
    }
}

fn digit_index(name: char) -> Option<usize> {
    if name.is_ascii_digit() {
        Some(name as usize - '0' as usize)
    } else {
        None
    }
}

/// Jump list behind `ctrl-o` / `ctrl-i`.
#[derive(Debug, Clone)]
pub struct JumpList {
    entries: Vec<(BufferId, usize)>,
    index: usize,
    limit: usize,
}

impl Default for JumpList {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            index: 0,
            limit: 100,
        }
    }
}

impl JumpList {
    pub fn push(&mut self, entry: (BufferId, usize)) {
        if self
            .entries
            .last()
            .is_some_and(|last| last.0 == entry.0 && (last.1 as isize - entry.1 as isize).abs() < 2)
        {
            return;
        }
        // Truncate any forward history we just walked away from.
        if self.index + 1 < self.entries.len() {
            self.entries.truncate(self.index + 1);
        }
        self.entries.push(entry);
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.entries.drain(..excess);
        }
        self.index = self.entries.len() - 1;
    }

    pub fn back(&mut self) -> Option<(BufferId, usize)> {
        if self.entries.is_empty() || self.index == 0 {
            return None;
        }
        self.index -= 1;
        Some(self.entries[self.index])
    }

    pub fn forward(&mut self) -> Option<(BufferId, usize)> {
        if self.index + 1 >= self.entries.len() {
            return None;
        }
        self.index += 1;
        Some(self.entries[self.index])
    }
}

/// What the `.` key replays.
#[derive(Debug, Clone)]
pub enum Repeatable {
    /// Text typed in insert mode.
    Insert { text: String },
    /// An operator plus the thing it applied to.
    Operator {
        operator: Operator,
        target: Target,
        count: usize,
        char_arg: Option<char>,
    },
    /// A paste.
    Put {
        register: char,
        position: PutPosition,
    },
}

/// The "thing" an operator applied to, replayable at a new cursor position.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Motion(Motion),
    Find {
        dir: FindDir,
        ch: char,
    },
    TextObject {
        inside: bool,
        key: char,
    },
    /// Whole lines (`dd`, `cc`, `S`).
    Line,
    /// The current visual selection (`v` + operator).
    Selection,
    /// The character(s) under the cursor (`x`).
    CharAhead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutPosition {
    AfterCursor,
    BeforeCursor,
    AfterLine,
    BeforeLine,
}

/// Kinds of background work tracked per buffer, so we do not spawn duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    Parse,
    Format,
    Spell,
}

/// Where `i`-like commands put the cursor.
enum InsertAt {
    Cursor,
    AfterCursor,
    WordEnd,
    LineEnd,
    FirstNonBlank,
    AfterFirstNonBlank,
}

/// The editor state.
pub struct Editor {
    pub config: Config,
    pub buffers: Vec<Buffer>,
    pub layout: Layout,
    pub mode: Mode,
    pub pending: Pending,
    pub registers: Registers,
    pub search: Option<SearchQuery>,
    /// Byte spans of the current search term, for highlighting.
    pub search_matches: Vec<Span>,
    pub jumplist: JumpList,
    pub last_find: Option<(FindDir, char)>,
    pub repeatable: Option<Repeatable>,
    /// `ctrl-o`: one normal-mode action, then straight back to insert.
    pub normal_once: bool,
    /// Text typed since insert mode was entered, for `.` repeat.
    insert_batch: String,
    /// Where insert mode started, so `Esc` can park the cursor on the last
    /// character typed the way vim does.
    insert_anchor: usize,
    /// `ctrl-a` in insert mode repeats the last inserted text.
    last_inserted_word: String,
    next_buffer_id: u32,
    pub pending_jobs: HashMap<(BufferId, JobKind), JobId>,
    /// A formatter run waiting for the idle timer, and the buffer that armed
    /// it.  The buffer travels with the deadline: an idle timer armed by typing
    /// in one buffer must not format whichever buffer happens to be on screen
    /// when it fires.
    pub format_due: Option<(BufferId, std::time::Instant)>,
    /// A buffer whose idle format is on hold because insert mode is open in it:
    /// an automatic reformat must never rewrite text under the user's hands.
    pub format_held: Option<BufferId>,
    /// Set when the user asked for the formatter by name (`:format`).  That run
    /// is a change they made, so unlike an automatic one it gets its own undo
    /// step; the flag is consumed when the formatter result lands.
    pub format_requested: Option<BufferId>,
    /// Diagnostics for workspace files that are not open as buffers, keyed by
    /// absolute path.  The language server analyses every source file in the
    /// project, so this holds what it reports about the ones vortex has no
    /// buffer for — which is how `:diagnostic list project` sees the whole
    /// workspace from the start instead of only the open files.
    pub project_diagnostics: HashMap<PathBuf, Vec<ProjectDiagnostic>>,
    /// Terminal area of the last frame, so commands can ask for geometry.
    last_area: AreaCell,
}

impl Editor {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            buffers: Vec::new(),
            layout: Layout::default(),
            mode: Mode::Normal,
            pending: Pending::default(),
            registers: Registers::new(),
            search: None,
            search_matches: Vec::new(),
            jumplist: JumpList::default(),
            last_find: None,
            repeatable: None,
            normal_once: false,
            insert_batch: String::new(),
            insert_anchor: 0,
            last_inserted_word: String::new(),
            next_buffer_id: 1,
            pending_jobs: HashMap::new(),
            format_due: None,
            format_held: None,
            format_requested: None,
            project_diagnostics: HashMap::new(),
            last_area: AreaCell::default(),
        }
    }

    // ------------------------------------------------------------- buffers

    fn alloc_buffer_id(&mut self) -> BufferId {
        let id = BufferId(self.next_buffer_id);
        self.next_buffer_id += 1;
        id
    }

    pub fn buffers(&self) -> &[Buffer] {
        &self.buffers
    }

    pub fn buffer(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.iter().find(|buffer| buffer.id == id)
    }

    pub fn buffer_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        self.buffers.iter_mut().find(|buffer| buffer.id == id)
    }

    pub fn current_buffer_id(&self) -> Option<BufferId> {
        self.layout.focused_pane().map(|pane| pane.buffer)
    }

    pub fn current_buffer(&self) -> Option<&Buffer> {
        self.current_buffer_id().and_then(|id| self.buffer(id))
    }

    pub fn current_pane(&self) -> Option<&Pane> {
        self.layout.focused_pane()
    }

    pub fn buffer_by_path(&self, path: &Path) -> Option<BufferId> {
        // Exact match first: the common case, and cheap.
        if let Some(id) = self
            .buffers
            .iter()
            .find(|buffer| buffer.path() == Some(path))
            .map(|buffer| buffer.id)
        {
            return Some(id);
        }
        // A buffer opened from a relative command-line argument stores a
        // relative path, but a language server echoes back an absolute
        // `file://` URI.  Comparing the two directly never matches, so every
        // diagnostic for such a file was dropped as "no such buffer".  Both
        // sides are absolutized before the second attempt.
        let wanted = std::path::absolute(path).ok()?;
        self.buffers
            .iter()
            .find(|buffer| {
                buffer
                    .path()
                    .and_then(|stored| std::path::absolute(stored).ok())
                    .is_some_and(|stored| stored == wanted)
            })
            .map(|buffer| buffer.id)
    }

    pub fn dirty_buffers(&self) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|buffer| buffer.is_dirty())
            .map(|buffer| buffer.id)
            .collect()
    }

    /// Open a file, reusing a buffer that already shows it.
    ///
    /// Returns the buffer and whether it was newly created; the pane is not
    /// touched — use [`Editor::show_in_pane`] for that.
    pub fn open_path(&mut self, path: &Path) -> (BufferId, bool) {
        let path = expand_home(path);
        if let Some(id) = self.buffer_by_path(&path) {
            return (id, false);
        }
        let id = self.alloc_buffer_id();
        let buffer = match Buffer::open_or_new(id, &path, &self.config.editor) {
            Ok(buffer) => buffer,
            // An unreadable file is still editable: start empty and let the
            // first write fail loudly if it must.
            Err(_) => Buffer::new(id, Some(path.clone()), String::new(), &self.config.editor),
        };
        self.buffers.push(buffer);
        (id, true)
    }

    /// An empty, unnamed buffer.
    pub fn new_scratch(&mut self) -> BufferId {
        let id = self.alloc_buffer_id();
        self.buffers
            .push(Buffer::new(id, None, String::new(), &self.config.editor));
        id
    }

    /// Show `buffer` in the focused pane, or in a new pane when `split` is set.
    pub fn show_in_pane(&mut self, id: BufferId, split: Option<Axis>) -> Option<PaneId> {
        if let Some(axis) = split {
            let area = self.frame_area();
            let new_id =
                self.layout
                    .split_focused(Some(axis), &self.config.layout, area, |pane_id| {
                        Pane::new(pane_id, id)
                    })?;
            self.layout.on_focus_changed(&self.config.layout);
            return Some(new_id);
        }
        // No panes yet (the welcome screen): this pane is the first one.
        if self.layout.root.is_none() {
            let pane_id = self.layout.show_first(id);
            self.layout.on_focus_changed(&self.config.layout);
            return Some(pane_id);
        }
        if self.layout.focused().is_none()
            && let Some(first) = self.layout.panes().first().copied()
        {
            self.layout.focus(first);
        }
        let pane = self.layout.focused_pane_mut()?;
        pane.buffer = id;
        pane.selection = Selection::point(0);
        pane.view = Default::default();
        let pane_id = pane.id;
        self.layout.on_focus_changed(&self.config.layout);
        Some(pane_id)
    }

    /// Open a file in the focused pane, like vim's `:edit`.
    pub fn open_file(&mut self, path: &Path) -> BufferId {
        let (id, _created) = self.open_path(path);
        self.show_in_pane(id, None);
        id
    }

    /// Open a file in a new pane, like `:split` / `:vsplit`.
    pub fn open_file_split(&mut self, path: &Path, axis: Axis) -> BufferId {
        let (id, _created) = self.open_path(path);
        self.show_in_pane(id, Some(axis));
        id
    }

    /// Focus the first pane showing `buffer`, opening one if needed.
    pub fn focus_buffer(&mut self, id: BufferId) -> bool {
        if let Some(pane) = self.layout.panes_showing(id).first().copied() {
            self.layout.focus(pane);
            self.layout.on_focus_changed(&self.config.layout);
            return true;
        }
        self.show_in_pane(id, None).is_some()
    }

    /// Close a buffer; refuses to discard unsaved changes unless `force`.
    pub fn close_buffer(&mut self, id: BufferId, force: bool) -> Result<usize, String> {
        let buffer = self
            .buffer(id)
            .ok_or_else(|| "no such buffer".to_string())?;
        if buffer.is_dirty() && !force {
            return Err(format!(
                "{} has unsaved changes (use :bdelete! to discard)",
                buffer.display_name()
            ));
        }
        let panes = self.layout.panes_showing(id);
        for pane in &panes {
            self.layout.remove(*pane);
        }
        self.buffers.retain(|buffer| buffer.id != id);
        Ok(panes.len())
    }

    // ------------------------------------------------------------- cursors

    pub fn selection(&self) -> Selection {
        self.current_pane()
            .map(|pane| pane.selection.clone())
            .unwrap_or_else(|| Selection::point(0))
    }

    pub fn cursor(&self) -> usize {
        self.selection().cursor()
    }

    pub fn set_selection(&mut self, selection: Selection) {
        if let Some(pane) = self.layout.focused_pane_mut() {
            pane.selection = selection;
            pane.view.wanted_col = None;
        }
    }

    pub fn set_cursor(&mut self, position: usize) {
        let position = self
            .current_buffer()
            .map(|buffer| buffer.clamp_byte(position))
            .unwrap_or(0);
        self.set_selection(Selection::point(position));
    }

    /// Leave visual mode without touching anything else.
    pub fn leave_visual(&mut self) {
        if self.mode.is_visual() {
            self.mode = Mode::Normal;
        }
    }

    /// Apply changes to the focused buffer, recording undo data.
    pub fn apply(&mut self, changes: Vec<Change>, group: ChangeGroup) {
        let Some(id) = self.current_buffer_id() else {
            return;
        };
        if self.buffer(id).is_some_and(|buffer| buffer.read_only) {
            return;
        }
        let Some(pane_id) = self.layout.focused() else {
            return;
        };
        // Take the selection out, edit the buffer, then put it back: the borrow
        // checker is happy and the pane keeps its cursor.
        let Some(pane) = self.layout.pane_mut(pane_id) else {
            return;
        };
        let mut selection = pane.selection.clone();
        let Some(buffer) = self.buffer_mut(id) else {
            return;
        };
        buffer.apply(changes, group, &mut selection);
        if let Some(pane) = self.layout.pane_mut(pane_id) {
            pane.selection = selection;
        }
    }

    /// Apply changes to a specific buffer, keeping every pane's cursor sane.
    pub fn apply_to(
        &mut self,
        id: BufferId,
        changes: Vec<Change>,
        group: ChangeGroup,
    ) -> Option<()> {
        // One buffer edit, then every pane showing it maps its own cursor
        // through the same edits.
        let panes = self.layout.panes_showing(id);
        let mut selections = Vec::with_capacity(panes.len());
        for pane_id in panes {
            let selection = self.layout.pane(pane_id)?.selection.clone();
            selections.push((pane_id, selection));
        }
        let anchor = selections
            .iter()
            .find(|(pane_id, _)| self.layout.is_focused(*pane_id))
            .map(|(pane_id, _)| *pane_id)
            .or_else(|| selections.first().map(|(pane_id, _)| *pane_id))?;
        let mut selection = selections
            .iter()
            .find(|(pane_id, _)| *pane_id == anchor)
            .map(|(_, selection)| selection.clone())
            .unwrap_or_else(|| Selection::point(0));
        let edits = self.buffer_mut(id)?.apply(changes, group, &mut selection);
        for (pane_id, pane_selection) in selections {
            if let Some(pane) = self.layout.pane_mut(pane_id) {
                pane.selection = pane_selection.map(&edits);
            }
        }
        Some(())
    }

    /// The region a visual selection covers.
    pub fn selection_region(&self) -> Region {
        let Some(buffer) = self.current_buffer() else {
            return Region::empty();
        };
        let selection = self.selection();
        let span = selection.primary();
        match self.mode.visual_kind() {
            Some(VisualKind::Line) => {
                let first = buffer.line_of(span.start);
                let last = buffer.line_of(span.end.max(span.start));
                Region::line(operators::lines_span(buffer, first, last))
            }
            Some(VisualKind::Block) => {
                Region::block(operators::block_spans(buffer, span.start, span.end))
            }
            Some(VisualKind::Char) | None => {
                if span.is_empty() {
                    Region::empty()
                } else {
                    // A characterwise selection includes the character under the
                    // cursor, so the region extends past it.
                    Region::char(Span::new(span.start, next_boundary(buffer, span.end)))
                }
            }
        }
    }

    /// Remember the terminal area so commands can ask for geometry later.
    pub fn set_last_area(&mut self, area: Rect) {
        self.last_area.0.set(area);
    }

    // ------------------------------------------------------------- key flow

    /// Which keymap a key press resolves against right now.
    pub fn key_context(&self) -> KeyContext {
        match self.mode {
            Mode::Insert => KeyContext::Insert,
            Mode::Visual(_) => KeyContext::Visual,
            Mode::Normal | Mode::Command => {
                if self.pending.operator.is_some() {
                    KeyContext::Operator
                } else {
                    KeyContext::Normal
                }
            }
        }
    }

    /// Resolve one key press in normal, visual or insert mode.
    pub fn handle_key(&mut self, key: KeyEvent, keymaps: &Keymaps, area: Rect) -> Vec<Intent> {
        self.set_last_area(area);

        if let Some(awaiting) = self.pending.awaiting.take() {
            return self.resolve_awaiting(awaiting, key, keymaps, area);
        }

        // Digits build a count, except in insert mode and except a leading `0`
        // (which is the first-column motion).
        if self.mode != Mode::Insert && self.mode != Mode::Command {
            if let Some(digit) = count_digit(key, self.pending.count.is_some()) {
                self.pending.push_digit(digit);
                return Vec::new();
            }
        }

        // A key can both complete a binding and start a longer one: with
        // `wd = close-pane` in the config, `w` is bound *and* a prefix. When
        // this key does not extend what is already bound, run the shorter
        // binding and read this key again on its own — otherwise binding `wd`
        // would make `w` unusable.
        if let Some(stale) = self.stale_binding(keymaps, key) {
            let pressed = std::mem::take(&mut self.pending.keys);
            self.pending.fallback = None;
            let mut intents = self.dispatch(stale, &pressed, area);
            intents.extend(self.handle_key(key, keymaps, area));
            return intents;
        }

        let action = {
            let map = keymaps.for_context(self.key_context()).clone();
            self.pending.note_key(keys::normalize(key));
            map.lookup(&self.pending.keys)
        };
        if matches!(action, Lookup::Prefix) {
            let map = keymaps.for_context(self.key_context());
            self.pending.fallback = map.bound_at(&self.pending.keys);
        }

        match action {
            Lookup::Prefix => Vec::new(),
            Lookup::Action(action) => {
                let pressed = std::mem::take(&mut self.pending.keys);
                self.pending.fallback = None;
                self.dispatch(action, &pressed, area)
            }
            Lookup::Unknown => {
                if self.mode == Mode::Insert {
                    // Writing text is not a binding: an unbound key still types,
                    // deletes or steps, so a configuration with no
                    // `[keymap.insert]` at all can still write.  A line that
                    // claims one of these keys wins — this branch only runs when
                    // the insert keymap had no answer.
                    if let Some(text) = insertable(key) {
                        self.pending.clear_keys();
                        self.insert_typed(&text);
                        return vec![Intent::EnsureVisible];
                    }
                    if let Some(action) = crate::input::typing_core(key) {
                        let pressed = std::mem::take(&mut self.pending.keys);
                        return self.dispatch(action, &pressed, area);
                    }
                }
                let pressed: Vec<KeyEvent> = std::mem::take(&mut self.pending.keys);
                self.pending.fallback = None;
                let count = self.pending.count.take();
                let operator = self.pending.operator.take();
                let _ = count;
                let rendered = pressed
                    .iter()
                    .map(|key| keys::describe(*key))
                    .collect::<Vec<_>>()
                    .join(" ");
                match operator {
                    Some(_) => vec![Intent::Message(
                        StatusLevel::Warn,
                        "no motion for that operator".to_string(),
                    )],
                    None => vec![Intent::Message(
                        StatusLevel::Warn,
                        format!("no binding for \"{rendered}\""),
                    )],
                }
            }
        }
    }

    fn resolve_awaiting(
        &mut self,
        awaiting: Awaiting,
        key: KeyEvent,
        keymaps: &Keymaps,
        area: Rect,
    ) -> Vec<Intent> {
        // `Esc` cancels whatever we were waiting for.
        if key.code == KeyCode::Esc && !matches!(awaiting, Awaiting::Replace) {
            self.pending.clear();
            return Vec::new();
        }
        match awaiting {
            Awaiting::Register => {
                let Some(name) = keys::typed_char(key) else {
                    return Vec::new();
                };
                self.pending.register = Some(name);
                // `"<reg>p`, `"<reg>y…`: keep waiting for the command unless the
                // key alone already resolves to one in this context.
                let map = keymaps.for_context(self.key_context()).clone();
                let normalized = keys::normalize(key);
                match map.lookup(&[normalized]) {
                    Lookup::Action(action) => self.dispatch(action, &[normalized], area),
                    Lookup::Prefix | Lookup::Unknown => Vec::new(),
                }
            }
            Awaiting::SetMark => {
                let Some(name) = keys::typed_char(key) else {
                    return Vec::new();
                };
                self.set_mark(name)
            }
            Awaiting::Mark { line } => {
                let Some(name) = keys::typed_char(key) else {
                    return Vec::new();
                };
                self.goto_mark(name, line)
            }
            Awaiting::Find(dir) => {
                let Some(ch) = keys::typed_char(key) else {
                    return Vec::new();
                };
                self.last_find = Some((dir, ch));
                let count = self.pending.take_count();
                self.apply_find(dir, ch, count)
            }
            Awaiting::TextObject { inside } => {
                let Some(ch) = keys::typed_char(key) else {
                    return Vec::new();
                };
                if let Some(operator) = self.pending.operator.take() {
                    let count = self.pending.take_count();
                    return self.run_operator(
                        operator.operator,
                        Target::TextObject { inside, key: ch },
                        count,
                        None,
                        area,
                    );
                }
                self.select_object(inside, ch)
            }
            Awaiting::Replace => {
                let Some(ch) = keys::typed_char(key) else {
                    return Vec::new();
                };
                let count = self.pending.take_count();
                self.run_operator(Operator::Replace, Target::CharAhead, count, Some(ch), area)
            }
        }
    }

    /// The binding already typed, when the key just pressed cannot extend it.
    fn stale_binding(&self, keymaps: &Keymaps, key: KeyEvent) -> Option<Action> {
        let fallback = self.pending.fallback.clone()?;
        if self.pending.keys.is_empty() {
            return None;
        }
        let map = keymaps.for_context(self.key_context());
        let mut extended = self.pending.keys.clone();
        extended.push(keys::normalize(key));
        matches!(map.lookup(&extended), Lookup::Unknown).then_some(fallback)
    }

    /// Resolve a key sequence that has gone quiet, the way vim's `timeoutlen`
    /// does: whatever is bound runs (`w`, even though `wd` is also bound), and a
    /// prefix that binds nothing (a stray `g`) is dropped.
    pub fn expire_pending(&mut self, area: Rect) -> Vec<Intent> {
        if !self.pending.prefix_expired() {
            return Vec::new();
        }
        let Some(action) = self.pending.fallback.take() else {
            self.pending.clear_keys();
            return Vec::new();
        };
        let pressed = std::mem::take(&mut self.pending.keys);
        self.dispatch(action, &pressed, area)
    }

    /// Turn a resolved action into work.
    pub fn dispatch(&mut self, action: Action, pressed: &[KeyEvent], area: Rect) -> Vec<Intent> {
        match action {
            Action::Operator(operator) => self.begin_operator(operator, pressed, area),
            Action::OperatorMotion(operator, motion) => {
                let count = self.pending.take_count();
                self.pending.operator = None;
                self.run_operator(operator, Target::Motion(motion), count, None, area)
            }
            Action::OperatorLine(operator) => {
                let count = self.pending.take_count();
                self.pending.operator = None;
                self.run_operator(operator, Target::Line, count, None, area)
            }
            Action::Motion(Motion::FindChar(dir)) => {
                self.pending.awaiting = Some(Awaiting::Find(dir));
                Vec::new()
            }
            Action::Extend(Motion::FindChar(dir)) => {
                self.pending.awaiting = Some(Awaiting::Find(dir));
                Vec::new()
            }
            Action::Motion(motion) => {
                if let Motion::TextObject { inside } = motion {
                    // `ciw`, `da"`: the operator stays pending while the object
                    // itself arrives on the next keypress.
                    self.pending.awaiting = Some(Awaiting::TextObject { inside });
                    return Vec::new();
                }
                if let Some(operator) = self.pending.operator.take() {
                    let count = self.pending.take_count();
                    return self.run_operator(
                        operator.operator,
                        Target::Motion(motion),
                        count,
                        None,
                        area,
                    );
                }
                let count = self.pending.take_count();
                self.apply_motion(motion, count, area)
            }
            Action::Extend(motion) => {
                let count = self.pending.take_count();
                self.extend_selection(motion, count, area)
            }
            Action::Cmd(command) => self.run_command(command, area),
        }
    }

    fn begin_operator(
        &mut self,
        operator: Operator,
        pressed: &[KeyEvent],
        area: Rect,
    ) -> Vec<Intent> {
        // A repeated sequence means "whole line": dd, cc, yy, >>, ==…
        if let Some(existing) = &self.pending.operator {
            if existing.operator == operator && existing.matches(pressed) {
                self.pending.operator = None;
                let count = self.pending.take_count();
                return self.run_operator(operator, Target::Line, count, None, area);
            }
        }
        if operator == Operator::Replace {
            self.pending.awaiting = Some(Awaiting::Replace);
            return Vec::new();
        }
        // In visual mode an operator applies to the selection right away.
        if self.mode.is_visual() {
            self.pending.operator = None;
            let count = self.pending.take_count();
            return self.run_operator(operator, Target::Selection, count, None, area);
        }
        self.pending.operator = Some(OperatorPending {
            operator,
            keys: pressed.to_vec(),
        });
        Vec::new()
    }

    // ------------------------------------------------------------- motion

    /// Context handed to motion functions.
    fn motion_env(&self, area: Rect) -> Env<'_> {
        let pane = self.current_pane();
        Env {
            height: self.text_height(area).max(1),
            scroll_line: pane.map(|pane| pane.view.scroll_line).unwrap_or(0),
            wanted_col: pane.and_then(|pane| pane.view.wanted_col),
            last_find: self.last_find,
            search: self.search.as_ref(),
        }
    }

    /// The rectangle of the pane holding the cursor.
    ///
    /// Callers hand in the whole terminal area.  Everything measured in rows —
    /// scrolling, page moves, `H`/`M`/`L` — belongs to the focused pane rather
    /// than the terminal: after a `:split` the pane is half as tall, and
    /// measuring the terminal instead lets the cursor walk off screen while the
    /// editor still counts it as visible.
    pub fn focused_area(&self, area: Rect) -> Rect {
        let (_status, panes) = crate::ui::areas(area, &self.config);
        let Some(focused) = self.layout.focused() else {
            return panes;
        };
        self.layout
            .rects(panes)
            .into_iter()
            .find(|(id, _)| *id == focused)
            .map(|(_, rect)| rect)
            .unwrap_or(panes)
    }

    /// Rows available for text inside the focused pane.
    pub fn text_height(&self, area: Rect) -> usize {
        let border = if self.config.layout.border == crate::config::BorderStyle::None {
            0
        } else {
            2
        };
        let pane = self.focused_area(area);
        (pane.height as usize).saturating_sub(border).max(1)
    }

    fn apply_motion(&mut self, motion: Motion, count: usize, area: Rect) -> Vec<Intent> {
        if let Motion::TextObject { inside } = motion {
            self.pending.awaiting = Some(Awaiting::TextObject { inside });
            return Vec::new();
        }
        let (cursor, dest, before_line) = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            let cursor = self.cursor();
            let env = self.motion_env(area);
            match motions::apply(motion, count, cursor, buffer, &env) {
                Some(dest) => (cursor, dest, buffer.line_of(cursor)),
                None => return Vec::new(),
            }
        };
        if should_push_jump(motion, before_line, self.line_at(dest.cursor)) {
            if let Some(id) = self.current_buffer_id() {
                self.jumplist.push((id, cursor));
            }
        }
        let clamped = self
            .current_buffer()
            .map(|buffer| clamp_cursor(buffer, dest.cursor))
            .unwrap_or(0);
        self.set_cursor(clamped);
        if let Some(column) = dest.wanted_col {
            if let Some(pane) = self.layout.focused_pane_mut() {
                pane.view.wanted_col = Some(column);
            }
        }
        vec![Intent::EnsureVisible]
    }

    fn line_at(&self, position: usize) -> usize {
        self.current_buffer()
            .map(|buffer| buffer.line_of(position))
            .unwrap_or(0)
    }

    /// Grow the visual selection with a motion.
    fn extend_selection(&mut self, motion: Motion, count: usize, area: Rect) -> Vec<Intent> {
        if let Motion::TextObject { inside } = motion {
            self.pending.awaiting = Some(Awaiting::TextObject { inside });
            return Vec::new();
        }
        let (anchor, cursor, dest) = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            let selection = self.selection();
            let env = self.motion_env(area);
            match motions::apply(motion, count, selection.cursor(), buffer, &env) {
                Some(dest) => (selection.anchor(), selection.cursor(), dest),
                None => return Vec::new(),
            }
        };
        let clamped = self
            .current_buffer()
            .map(|buffer| clamp_cursor(buffer, dest.cursor))
            .unwrap_or(0);
        let mut selection = self.selection();
        if cursor == anchor {
            selection.set_dir(if clamped >= anchor {
                Dir::Forward
            } else {
                Dir::Backward
            });
        }
        selection.extend_to(clamped);
        self.set_selection(selection);
        vec![Intent::EnsureVisible]
    }

    fn apply_find(&mut self, dir: FindDir, ch: char, count: usize) -> Vec<Intent> {
        let found = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            motions::find_char(buffer, self.cursor(), dir, ch, count)
        };
        match found {
            Some(at) => {
                let at = self
                    .current_buffer()
                    .map(|buffer| clamp_cursor(buffer, at))
                    .unwrap_or(0);
                self.set_cursor(at);
                vec![Intent::EnsureVisible]
            }
            None => vec![Intent::Message(
                StatusLevel::Warn,
                format!("can't find '{ch}'"),
            )],
        }
    }

    /// Select a text object: extend in visual mode, otherwise start visual.
    fn select_object(&mut self, inside: bool, key: char) -> Vec<Intent> {
        let Some(object) = textobject::object_from_key(key) else {
            return vec![Intent::Message(
                StatusLevel::Warn,
                format!("unknown text object \"{key}\""),
            )];
        };
        let span = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            object.range(buffer, self.cursor(), inside)
        };
        let Some(span) = span else {
            return vec![Intent::Message(
                StatusLevel::Warn,
                format!("no {} here", object.label()),
            )];
        };
        if span.is_empty() {
            return Vec::new();
        }
        // A characterwise visual selection ends on the last character *inside*
        // the object, so the cursor sits on it rather than past it.
        let end = span.end.saturating_sub(1);
        if self.mode.is_visual() {
            let mut selection = self.selection();
            selection.extend_to(end);
            self.set_selection(selection);
        } else {
            self.mode = Mode::Visual(VisualKind::Char);
            let mut selection = Selection::point(span.start);
            selection.set_primary_span(Span::new(span.start, span.start), Dir::Forward);
            selection.extend_to(end);
            self.set_selection(selection);
        }
        vec![Intent::EnsureVisible]
    }

    // ------------------------------------------------------------- operators

    fn run_operator(
        &mut self,
        operator: Operator,
        target: Target,
        count: usize,
        char_arg: Option<char>,
        area: Rect,
    ) -> Vec<Intent> {
        self.pending.operator = None;
        self.pending.awaiting = None;
        let count = count.max(1);
        let Some(region) = self.region_for(&target, count, area) else {
            self.pending.clear();
            return vec![Intent::Message(
                StatusLevel::Warn,
                format!("nothing to {}", verb_for(operator)),
            )];
        };

        // Take the register first: `buffer` borrows `self` immutably.
        let register = self.pending.register.take();
        let Some(buffer) = self.current_buffer() else {
            return Vec::new();
        };
        let text = region.text(buffer);
        let tab_width = self.tab_width();
        let expand_tab = self.expand_tab();
        let comment_token = {
            let token = buffer.language.line_comment;
            if token.is_empty() { "//" } else { token }
        };
        let start = region.extent().start;
        let mut intents = Vec::new();

        match operator {
            Operator::Yank => {
                self.registers
                    .yank(register, Register::new(text.clone(), region.linewise));
                self.set_cursor(start);
                if self.config.editor.clipboard {
                    intents.push(Intent::Clipboard(text.clone()));
                }
                intents.push(Intent::Message(
                    StatusLevel::Info,
                    chunk_message("yanked", &text, region.linewise),
                ));
            }
            Operator::Delete => {
                let changes = operators::delete_changes(&region);
                self.apply(changes, ChangeGroup::Operator);
                self.registers
                    .delete(register, Register::new(text.clone(), region.linewise));
                if self.config.editor.clipboard {
                    intents.push(Intent::Clipboard(text.clone()));
                }
                intents.push(Intent::Message(
                    StatusLevel::Info,
                    chunk_message("deleted", &text, region.linewise),
                ));
                let cursor = if region.linewise {
                    self.indent_of_line_at(start)
                } else {
                    start
                };
                self.set_cursor(cursor);
            }
            Operator::Change => {
                let (changes, cursor) = {
                    let buffer = self.current_buffer().expect("checked above");
                    operators::change_region(buffer, &region)
                };
                self.apply(changes, ChangeGroup::Operator);
                self.registers
                    .delete(register, Register::new(text, region.linewise));
                self.leave_visual();
                self.set_cursor(cursor);
                self.enter_insert();
            }
            Operator::Replace => {
                let Some(ch) = char_arg else {
                    return Vec::new();
                };
                let changes = {
                    let buffer = self.current_buffer().expect("checked above");
                    operators::replace_char_changes(buffer, &region, ch)
                };
                self.apply(changes, ChangeGroup::Operator);
                self.set_cursor(start);
            }
            Operator::Join => {
                let lines = {
                    let buffer = self.current_buffer().expect("checked above");
                    if region.linewise {
                        let extent = region.extent();
                        buffer
                            .line_of(extent.end)
                            .saturating_sub(buffer.line_of(extent.start))
                            + 1
                    } else {
                        count.max(2)
                    }
                };
                let changes = {
                    let buffer = self.current_buffer().expect("checked above");
                    operators::join_changes(buffer, &region, lines)
                };
                self.apply(changes, ChangeGroup::Operator);
                let cursor = self
                    .current_buffer()
                    .map(|buffer| buffer.clamp_byte(start))
                    .unwrap_or(0);
                self.set_cursor(cursor);
            }
            Operator::Indent | Operator::Unindent => {
                let direction = if operator == Operator::Indent { 1 } else { -1 };
                let changes = {
                    let buffer = self.current_buffer().expect("checked above");
                    operators::indent_changes(
                        buffer, &region, direction, count, expand_tab, tab_width,
                    )
                };
                self.apply(changes, ChangeGroup::Operator);
                self.set_cursor(self.indent_of_line_at(start));
            }
            Operator::Reformat => {
                let changes = {
                    let buffer = self.current_buffer().expect("checked above");
                    operators::reindent_changes(buffer, &region, tab_width)
                };
                self.apply(changes, ChangeGroup::Operator);
                self.set_cursor(self.indent_of_line_at(start));
                // A whole-buffer formatter handles the rest: the language
                // server when one is configured (it can format just this
                // range), otherwise the external formatter.
                let language = self
                    .current_buffer()
                    .map(|buffer| buffer.language.id)
                    .unwrap_or("text");
                let has_formatter = crate::format::formatter_for(language, &self.config).is_some();
                if region.linewise && (has_formatter || self.config.lsp.enable) {
                    let id = self.current_buffer_id().expect("focused buffer");
                    if self.config.lsp.enable {
                        intents.push(Intent::RangeFormat(id, region.extent()));
                    } else {
                        intents.push(Intent::Format(id));
                    }
                }
            }
            Operator::SwapCase | Operator::Upper | Operator::Lower => {
                let case = match operator {
                    Operator::Upper => Case::Upper,
                    Operator::Lower => Case::Lower,
                    _ => Case::Swap,
                };
                let cursor = {
                    let buffer = self.current_buffer().expect("checked above");
                    let changes = operators::case_changes(buffer, &region, case);
                    let next = start;
                    (changes, next)
                };
                self.apply(cursor.0, ChangeGroup::Operator);
                self.set_cursor(cursor.1);
            }
            Operator::ToggleComment => {
                let changes = {
                    let buffer = self.current_buffer().expect("checked above");
                    operators::comment_changes(buffer, &region, comment_token)
                };
                self.apply(changes, ChangeGroup::Operator);
                self.set_cursor(start);
            }
        }

        if self.mode.is_visual() {
            self.leave_visual();
        }
        self.repeatable = Some(Repeatable::Operator {
            operator,
            target,
            count,
            char_arg,
        });
        intents.push(Intent::EnsureVisible);
        intents
    }

    /// Byte offset of the first non-blank character of the line containing
    /// `position` — where most line-wise operators leave the cursor.
    fn indent_of_line_at(&self, position: usize) -> usize {
        self.current_buffer()
            .map(|buffer| {
                let line = buffer.line_of(position);
                let non_blank = buffer.first_non_blank(line);
                if non_blank >= buffer.line_end(line) {
                    buffer.line_end(line)
                } else {
                    non_blank
                }
            })
            .unwrap_or(0)
    }

    /// Resolve a target into the bytes an operator will touch.
    fn region_for(&mut self, target: &Target, count: usize, area: Rect) -> Option<Region> {
        let cursor = self.cursor();
        match target {
            Target::Line => {
                let buffer = self.current_buffer()?;
                let first = buffer.line_of(cursor);
                let last = (first + count - 1).min(buffer.last_line());
                Some(Region::line(operators::lines_span(buffer, first, last)))
            }
            Target::Selection => {
                let region = self.selection_region();
                (!region.is_empty()).then_some(region)
            }
            Target::CharAhead => {
                let buffer = self.current_buffer()?;
                let mut stop = next_boundary(buffer, cursor);
                if stop == cursor {
                    return None;
                }
                for _ in 1..count {
                    stop = next_boundary(buffer, stop);
                }
                Some(Region::char(Span::new(cursor, stop)))
            }
            Target::Find { dir, ch } => {
                let buffer = self.current_buffer()?;
                let at = motions::find_char(buffer, cursor, *dir, *ch, count)?;
                Some(operators::region_for_motion(
                    buffer, cursor, at, false, true,
                ))
            }
            Target::TextObject { inside, key } => {
                let buffer = self.current_buffer()?;
                let object = textobject::object_from_key(*key)?;
                let span = object.range(buffer, cursor, *inside)?;
                (!span.is_empty()).then(|| Region::char(span))
            }
            Target::Motion(motion) => {
                if matches!(motion, Motion::TextObject { .. }) {
                    return None;
                }
                let buffer = self.current_buffer()?;
                let env = self.motion_env(area);
                let Dest {
                    cursor: to,
                    kind,
                    inclusive,
                    ..
                } = motions::apply(*motion, count, cursor, buffer, &env)?;
                if kind == Kind::Line {
                    let first = buffer.line_of(cursor.min(to));
                    let last = buffer.line_of(cursor.max(to));
                    return Some(Region::line(operators::lines_span(buffer, first, last)));
                }
                let region = operators::region_for_motion(buffer, cursor, to, false, inclusive);
                if region.is_empty() {
                    // A motion that goes nowhere still acts on the character
                    // under the cursor (`~`, `x`).
                    let end = next_boundary(buffer, cursor);
                    return (end != cursor).then(|| Region::char(Span::new(cursor, end)));
                }
                Some(region)
            }
        }
    }

    // ------------------------------------------------------------- commands

    fn run_command(&mut self, command: Cmd, area: Rect) -> Vec<Intent> {
        let mut intents = Vec::new();
        let count = self.pending.take_count();
        match command {
            Cmd::Noop => {}
            Cmd::Cancel => {
                self.pending.clear();
                self.leave_visual();
            }
            Cmd::Normal | Cmd::ExitInsert => {
                self.exit_insert();
                self.leave_visual();
                self.pending.clear();
            }
            Cmd::Visual(kind) => {
                if self.mode.visual_kind() == Some(kind) {
                    self.mode = Mode::Normal;
                } else {
                    self.begin_visual(kind);
                }
            }
            Cmd::SwapAnchor => {
                let selection = self.selection();
                let primary = selection.primary();
                let target = if selection.cursor() == selection.anchor() {
                    primary.end
                } else {
                    selection.anchor()
                };
                let mut selection = selection;
                selection.set_dir(match selection.dir() {
                    Dir::Forward => Dir::Backward,
                    Dir::Backward => Dir::Forward,
                });
                selection.extend_to(target);
                self.set_selection(selection);
            }
            // ---- entering insert / visual ----
            Cmd::Insert => self.begin_insert(InsertAt::Cursor),
            Cmd::InsertAtWordEnd => self.begin_insert(InsertAt::WordEnd),
            Cmd::Append => self.begin_insert(InsertAt::AfterCursor),
            Cmd::AppendLineEnd => self.begin_insert(InsertAt::LineEnd),
            Cmd::InsertLineStart => self.begin_insert(InsertAt::FirstNonBlank),
            Cmd::AppendLineStart => self.begin_insert(InsertAt::AfterFirstNonBlank),
            Cmd::InsertPrevious => {
                if let Some(at) = self
                    .current_buffer()
                    .and_then(|buffer| buffer.last_change_pos)
                {
                    self.set_cursor(at);
                }
                self.begin_insert(InsertAt::Cursor);
            }
            Cmd::OpenBelow | Cmd::OpenAbove => {
                intents.extend(self.open_line(command == Cmd::OpenBelow))
            }
            // ---- insert mode editing ----
            Cmd::InsertNewline => self.insert_newline(),
            Cmd::InsertTab => self.insert_tab(),
            Cmd::InsertBacktab => self.insert_backtab(),
            Cmd::InsertBackspace => self.insert_backspace(),
            Cmd::InsertDelete => self.insert_delete(),
            Cmd::InsertDeleteWord => self.insert_delete_word(),
            Cmd::InsertDeleteLineStart => self.insert_delete_line_start(),
            Cmd::InsertIndentLine => {
                let changes = self.current_buffer().map(|buffer| {
                    let line = buffer.line_of(self.cursor());
                    let region = Region::line(operators::lines_span(buffer, line, line));
                    operators::indent_changes(
                        buffer,
                        &region,
                        1,
                        1,
                        self.expand_tab(),
                        self.tab_width(),
                    )
                });
                if let Some(changes) = changes {
                    self.apply(changes, ChangeGroup::Insert);
                }
            }
            Cmd::NormalOnce => {
                self.normal_once = true;
                self.mode = Mode::Normal;
            }
            // ---- character edits ----
            Cmd::DeleteCharNext => {
                intents.extend(self.run_operator(
                    Operator::Delete,
                    Target::CharAhead,
                    count,
                    None,
                    area,
                ));
            }
            Cmd::DeleteCharPrev => {
                let span = self
                    .current_buffer()
                    .map(|buffer| {
                        let cursor = self.cursor();
                        let start = buffer
                            .char_before(cursor)
                            .map(|(offset, _)| offset)
                            .unwrap_or(cursor);
                        Span::new(start, cursor)
                    })
                    .filter(|span| span.end > span.start);
                if let Some(span) = span {
                    let deleted = self
                        .current_buffer()
                        .map(|buffer| buffer.slice(span))
                        .unwrap_or_default();
                    self.apply(vec![Change::delete(span)], ChangeGroup::Command);
                    self.registers.delete(None, Register::new(deleted, false));
                    self.set_cursor(span.start);
                    self.leave_visual();
                }
            }
            Cmd::DeleteSelection => {
                let region = self.selection_region();
                if !region.is_empty() {
                    let deleted = self
                        .current_buffer()
                        .map(|buffer| region.text(buffer))
                        .unwrap_or_default();
                    let start = region.extent().start;
                    self.apply(operators::delete_changes(&region), ChangeGroup::Command);
                    self.registers
                        .delete(None, Register::new(deleted, region.linewise));
                    self.set_cursor(start);
                    self.mode = Mode::Normal;
                }
            }
            Cmd::RemoveEmptyLines => {
                let region = self.selection_region();
                let changes = if region.is_empty() {
                    Vec::new()
                } else {
                    let buffer = self.current_buffer().expect("region implies a buffer");
                    let extent = region.extent();
                    let first = buffer.line_of(extent.start);
                    let last = buffer.line_of(extent.end);
                    (first..=last)
                        .rev()
                        .filter(|line| buffer.is_line_blank(*line))
                        .map(|line| Change::delete(operators::lines_span(buffer, line, line)))
                        .collect()
                };
                self.apply(changes, ChangeGroup::Command);
                self.mode = Mode::Normal;
            }
            Cmd::DuplicateLine => {
                let plan = self.current_buffer().map(|buffer| {
                    let selection = self.selection();
                    let span = if self.mode.is_visual() {
                        let region = self.selection_region();
                        region.extent()
                    } else {
                        let line = buffer.line_of(selection.cursor());
                        operators::lines_span(buffer, line, line)
                    };
                    let text = buffer.slice(span);
                    let text = if text.ends_with('\n') {
                        text
                    } else {
                        format!("{text}\n")
                    };
                    (span.end, text)
                });
                if let Some((at, text)) = plan {
                    self.apply(vec![Change::insert(at, text)], ChangeGroup::Command);
                    self.set_cursor(at);
                    self.mode = Mode::Normal;
                }
            }
            // ---- paste ----
            Cmd::PutAfter => intents.extend(self.put(PutPosition::AfterCursor, count)),
            Cmd::PutBefore => intents.extend(self.put(PutPosition::BeforeCursor, count)),
            Cmd::PutAfterLine => intents.extend(self.put(PutPosition::AfterLine, count)),
            Cmd::PutBeforeLine => intents.extend(self.put(PutPosition::BeforeLine, count)),
            // ---- history ----
            Cmd::Undo => intents.extend(self.undo()),
            Cmd::Redo => intents.extend(self.redo()),
            Cmd::Repeat => intents.extend(self.repeat(area)),
            Cmd::SelectRegister => self.pending.awaiting = Some(Awaiting::Register),
            Cmd::SetMark => self.pending.awaiting = Some(Awaiting::SetMark),
            Cmd::GotoMarkChar => self.pending.awaiting = Some(Awaiting::Mark { line: false }),
            Cmd::GotoMarkLine => self.pending.awaiting = Some(Awaiting::Mark { line: true }),
            Cmd::ReplaceChar => self.pending.awaiting = Some(Awaiting::Replace),
            // ---- viewport ----
            Cmd::Recenter => intents.extend(self.recenter(area)),
            Cmd::ScrollToTop => intents.extend(self.scroll_to_edge(true, area)),
            Cmd::ScrollToBottom => intents.extend(self.scroll_to_edge(false, area)),
            Cmd::ScrollLine(direction) => intents.extend(self.scroll_lines(direction)),
            Cmd::JumpBack => intents.extend(self.jump(true)),
            Cmd::JumpForward => intents.extend(self.jump(false)),
            // ---- files ----
            Cmd::Save => intents.extend(self.save(None)),
            Cmd::SaveAs => intents.push(Intent::OpenPrompt(PromptSpec::Path {
                initial: self
                    .current_buffer()
                    .and_then(|buffer| buffer.path())
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            })),
            Cmd::SaveAll => intents.extend(self.save_all()),
            Cmd::ReloadFile => intents.extend(self.reload_file(false)),
            Cmd::Quit => intents.extend(self.quit(false)),
            Cmd::ForceQuit => intents.extend(self.quit(true)),
            Cmd::QuitAll => intents.extend(self.quit_all()),
            Cmd::ForceQuitAll => {
                // "Quit anyway": close every pane, discarding unsaved changes.
                for id in self
                    .buffers
                    .iter()
                    .map(|buffer| buffer.id)
                    .collect::<Vec<_>>()
                {
                    let _ = self.close_buffer(id, true);
                }
                intents.push(Intent::Quit);
            }
            Cmd::ClosePane => intents.extend(self.close_pane()),
            // ---- prompts ----
            Cmd::CommandMode => {
                intents.push(Intent::OpenPrompt(PromptSpec::Command(String::new())))
            }
            Cmd::SearchForward => intents.push(Intent::OpenPrompt(PromptSpec::Search {
                forward: true,
                initial: self.search_pattern().unwrap_or_default().to_string(),
            })),
            Cmd::SearchBackward => intents.push(Intent::OpenPrompt(PromptSpec::Search {
                forward: false,
                initial: self.search_pattern().unwrap_or_default().to_string(),
            })),
            Cmd::FindWordForward => intents.extend(self.find_word(true)),
            Cmd::FindWordBackward => intents.extend(self.find_word(false)),
            Cmd::ClearSearchHighlight => {
                self.search_matches.clear();
                intents.push(Intent::Message(
                    StatusLevel::Info,
                    "search highlight cleared".into(),
                ));
            }
            Cmd::GotoLine => intents.push(Intent::OpenPrompt(PromptSpec::GotoLine)),
            Cmd::FindFiles => {
                intents.push(Intent::OpenPrompt(PromptSpec::FindFiles(String::new())))
            }
            Cmd::GrepProject => intents.push(Intent::OpenPrompt(PromptSpec::Grep(String::new()))),
            Cmd::SwitchBuffer => intents.push(Intent::OpenPrompt(PromptSpec::SwitchBuffer)),
            Cmd::CommandPalette => {
                intents.push(Intent::OpenPrompt(PromptSpec::Palette(String::new())))
            }
            Cmd::ShowDiagnostics(scope) => {
                intents.push(Intent::OpenPrompt(PromptSpec::Diagnostics(scope)))
            }
            Cmd::DiagnosticNext(scope) => intents.extend(self.diagnostic_jump(true, scope, area)),
            Cmd::DiagnosticPrev(scope) => intents.extend(self.diagnostic_jump(false, scope, area)),
            Cmd::Hover => intents.push(Intent::Hover),
            Cmd::CodeAction => intents.push(Intent::CodeAction),
            Cmd::SpellSuggest => intents.extend(self.spell_suggest()),
            Cmd::FormatBuffer => {
                if let Some(id) = self.current_buffer_id() {
                    self.format_requested = Some(id);
                    intents.push(Intent::Format(id));
                }
            }
            // ---- layout ----
            Cmd::SplitVertical => intents.extend(self.split(Some(Axis::Column), area)),
            Cmd::SplitHorizontal => intents.extend(self.split(Some(Axis::Row), area)),
            Cmd::SplitAuto => intents.extend(self.split(None, area)),
            Cmd::OnlyPane => {
                self.layout.only();
                self.layout.on_focus_changed(&self.config.layout);
            }
            Cmd::ZoomPane => {
                self.layout.zoomed = !self.layout.zoomed;
            }
            Cmd::EqualizePanes => {
                self.layout.equalize();
            }
            Cmd::CycleLayout(mode) => {
                self.config.layout.split_mode = mode;
                if mode == SplitMode::Golden {
                    self.layout.apply_golden(&self.config.layout);
                }
                intents.push(Intent::Message(
                    StatusLevel::Info,
                    format!("split mode: {}", split_mode_label(mode)),
                ));
            }
            Cmd::FocusNext => {
                if self.layout.focus_next(false) {
                    self.layout.on_focus_changed(&self.config.layout);
                }
            }
            Cmd::FocusPrev => {
                if self.layout.focus_next(true) {
                    self.layout.on_focus_changed(&self.config.layout);
                }
            }
            Cmd::FocusDir(direction) => {
                if self.layout.focus_direction(direction, self.text_area(area)) {
                    self.layout.on_focus_changed(&self.config.layout);
                }
            }
            Cmd::MoveDir(direction) => {
                self.layout.move_focused(direction, self.text_area(area));
            }
            Cmd::ResizeDir(direction) => {
                self.layout
                    .resize_focused(direction, &self.config.layout, self.text_area(area));
            }
            Cmd::ToggleOption(option) => intents.extend(self.toggle_option(option)),
            Cmd::Ex(text) => {
                // A bound ex command runs immediately; an empty one just opens
                // the command line, like pressing `:`.
                if text.trim().is_empty() {
                    intents.push(Intent::OpenPrompt(PromptSpec::Command(String::new())));
                } else {
                    intents.push(Intent::RunCommand(text));
                }
            }
        }
        intents
    }

    // ------------------------------------------------------------- inserting

    fn begin_insert(&mut self, at: InsertAt) {
        let cursor = self
            .current_buffer()
            .map(|buffer| {
                let cursor = self.cursor();
                match at {
                    InsertAt::Cursor => cursor,
                    InsertAt::AfterCursor => next_boundary(buffer, cursor),
                    InsertAt::WordEnd => {
                        next_boundary(buffer, motions::word_end(buffer, cursor, 1, false))
                    }
                    InsertAt::LineEnd => buffer.line_end(buffer.line_of(cursor)),
                    InsertAt::FirstNonBlank => buffer.first_non_blank(buffer.line_of(cursor)),
                    InsertAt::AfterFirstNonBlank => {
                        let first = buffer.first_non_blank(buffer.line_of(cursor));
                        if first >= buffer.line_end(buffer.line_of(cursor)) {
                            first
                        } else {
                            next_boundary(buffer, first)
                        }
                    }
                }
            })
            .unwrap_or(0);
        self.set_cursor(cursor);
        self.enter_insert();
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.insert_anchor = self.cursor();
        self.insert_batch.clear();
        self.pending.clear();
        if let Some(id) = self.current_buffer_id()
            && let Some(buffer) = self.buffer_mut(id)
        {
            buffer.break_undo_group();
        }
    }

    fn exit_insert(&mut self) {
        let was_inserting = self.mode == Mode::Insert;
        if was_inserting {
            // Vim leaves the cursor on the last character typed.
            let cursor = self.cursor();
            if cursor > self.insert_anchor {
                let back = self.current_buffer().and_then(|buffer| {
                    let line = buffer.line_of(cursor);
                    let line_start = buffer.line_start(line);
                    buffer
                        .char_before(cursor)
                        .map(|(previous, _)| previous)
                        .filter(|previous| *previous >= line_start)
                });
                if let Some(previous) = back {
                    self.set_cursor(previous);
                }
            }
            if let Some(id) = self.current_buffer_id()
                && let Some(buffer) = self.buffer_mut(id)
            {
                buffer.break_undo_group();
            }
            if !self.insert_batch.is_empty() {
                self.last_inserted_word = std::mem::take(&mut self.insert_batch);
                self.repeatable = Some(Repeatable::Insert {
                    text: self.last_inserted_word.clone(),
                });
            }
        }
        if self.normal_once {
            self.normal_once = false;
            self.mode = Mode::Insert;
        } else {
            self.mode = Mode::Normal;
        }
        self.pending.clear();
        // The idle format held back while typing starts now that hands are off
        // the keyboard (see `schedule_auto_format`).
        self.format_held = None;
        if was_inserting && self.mode != Mode::Insert {
            if let Some(id) = self.current_buffer_id() {
                self.schedule_auto_format(id);
            }
        }
    }

    fn begin_visual(&mut self, kind: VisualKind) {
        if !self.mode.is_visual() {
            let cursor = self.cursor();
            let mut selection = Selection::point(cursor);
            selection.set_primary_span(Span::point(cursor), Dir::Forward);
            self.set_selection(selection);
        }
        self.mode = Mode::Visual(kind);
        self.pending.clear();
    }

    /// `o` / `O`: open a line below or above, keeping indentation, and insert.
    ///
    /// A visual selection is deleted first, like vim.  The delete is its own
    /// apply so that the offsets used to place the new line are live ones: an
    /// insert computed against the pre-delete buffer would land in the wrong
    /// place once the selection is gone.
    fn open_line(&mut self, below: bool) -> Vec<Intent> {
        let selected = {
            let selection = self.selection();
            if self.mode.is_visual() && !selection.primary().is_empty() {
                Some(self.selection_region())
            } else {
                None
            }
        };
        // Where a deleted selection used to be, and how its line was indented.
        let mut hole = None;
        if let Some(region) = &selected {
            let extent = region.extent();
            let indent = self
                .current_buffer()
                .map(|buffer| buffer.indent_str(buffer.line_of(extent.start)))
                .unwrap_or_default();
            self.apply(operators::delete_changes(region), ChangeGroup::Command);
            hole = Some((indent, region.linewise));
        }

        let expand_tab = self.expand_tab();
        let tab_width = self.tab_width();
        let plan = self.current_buffer().map(|buffer| {
            let line = buffer.line_of(self.cursor());
            // A linewise delete took its line ending with it, so the block's
            // place is now the start of the cursor line; a characterwise one
            // leaves the cursor inside the line it came from.
            let (indent, above) = match &hole {
                Some((indent, true)) => (indent.clone(), true),
                _ => (
                    crate::indent::opened_line_indent(buffer, line, below, expand_tab, tab_width),
                    !below,
                ),
            };
            let (at, text, caret) = if buffer.len_bytes() == 0 {
                // Nothing to split: the buffer already is the empty line.
                (0, String::new(), 0)
            } else if above {
                let at = buffer.line_start(line);
                (at, format!("{indent}\n"), at + indent.len())
            } else if line + 1 < buffer.line_count() {
                // Before the *next* line's first byte, with the indentation
                // first: `"\n{indent}"` after this line's ending would hang the
                // indentation on the start of the following line and leave the
                // caret one line too low.
                let at = buffer.line_range_with_ending(line).end;
                (at, format!("{indent}\n"), at + indent.len())
            } else {
                // The last line and it has no ending of its own: end it first.
                let at = buffer.line_end(line);
                (at, format!("\n{indent}"), at + 1 + indent.len())
            };
            (at, text, caret)
        });
        let Some((at, text, caret)) = plan else {
            return Vec::new();
        };
        if !text.is_empty() {
            self.apply(vec![Change::insert(at, text)], ChangeGroup::Command);
        }
        // The caret goes on the new line, after its indentation, *before*
        // `enter_insert`: the insert anchor is what `esc` walks back over, so an
        // anchor on the old line makes `o<esc>` jump a line.
        let landing = self
            .current_buffer()
            .map(|buffer| buffer.clamp_byte(caret))
            .unwrap_or(caret);
        self.set_cursor(landing);
        self.enter_insert();
        vec![Intent::EnsureVisible]
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(buffer) = self.current_buffer() else {
            return;
        };
        if buffer.read_only {
            return;
        }
        let region = if self.mode.is_visual() {
            let region = self.selection_region();
            (!region.is_empty()).then_some(region)
        } else {
            None
        };
        let cursor = self.cursor();
        let mut changes = Vec::new();
        if let Some(region) = &region {
            changes.extend(operators::delete_changes(region));
        }
        let at = region
            .as_ref()
            .map(|region| region.extent().start)
            .unwrap_or(cursor);
        changes.push(Change::insert(at, text));
        self.apply(changes, ChangeGroup::Insert);
        self.insert_batch.push_str(text);
        let landing = at + text.len();
        if let Some(buffer) = self.current_buffer() {
            self.set_cursor(buffer.clamp_byte(landing));
        }
    }

    /// Insert one typed character, with the syntax-aware extras that only make
    /// sense while typing: a closing bracket typed alone on its line dedents to
    /// the block it closes, an opening bracket or quote inserts its partner with
    /// the caret between the two, and typing the closing half of a pair steps
    /// over the partner instead of doubling it.
    fn insert_typed(&mut self, text: &str) {
        let mut chars = text.chars();
        let (Some(character), None) = (chars.next(), chars.next()) else {
            self.insert_text(text);
            return;
        };
        let cursor = self.cursor();
        // Typing the closing half of a pair that is already there steps over it.
        // Without this, `(` inserting `()` and then `)` would leave `())`.
        if self.config.editor.auto_match_pairs
            && is_closing(character)
            && self
                .current_buffer()
                .and_then(|buffer| buffer.char_at(cursor))
                == Some(character)
        {
            self.set_cursor(cursor + character.len_utf8());
            return;
        }
        if self.config.editor.auto_indent
            && let Some(indent) = self
                .current_buffer()
                .and_then(|buffer| crate::indent::dedent_for_close(buffer, cursor, character))
        {
            // Replace the whitespace before the caret with the block's own
            // indentation, then type the bracket at that column.
            let line_start = self
                .current_buffer()
                .map(|buffer| buffer.line_start(buffer.line_of(cursor)))
                .unwrap_or(cursor);
            self.apply(
                vec![Change::replace(Span::new(line_start, cursor), indent)],
                ChangeGroup::Insert,
            );
        }
        if self.config.editor.auto_match_pairs
            && let Some(close) = matching_pair(self.current_buffer(), cursor, character)
        {
            let cursor = self.cursor();
            self.insert_text(&format!("{character}{close}"));
            self.set_cursor(cursor + character.len_utf8());
            return;
        }
        self.insert_text(text);
    }

    fn insert_newline(&mut self) {
        // Syntax-aware: a line that opens a block indents the next one, and a
        // closing bracket that was already there keeps its own line.
        let plan = self.current_buffer().map(|buffer| {
            if self.config.editor.auto_indent {
                crate::indent::newline(buffer, self.cursor(), self.expand_tab(), self.tab_width())
            } else {
                crate::indent::Newline {
                    text: "\n".to_string(),
                    caret: 1,
                }
            }
        });
        let Some(plan) = plan else { return };
        let start = self.cursor();
        self.insert_text(&plan.text);
        // `insert_text` parks the caret after everything it inserted; a split
        // brace wants it between the two lines instead.
        if plan.caret != plan.text.len() {
            self.set_cursor(start + plan.caret);
        }
    }

    fn insert_tab(&mut self) {
        let unit = if self.expand_tab() {
            " ".repeat(self.tab_width())
        } else {
            "\t".to_string()
        };
        self.insert_text(&unit);
    }

    fn insert_backtab(&mut self) {
        let removed = self
            .current_buffer()
            .map(|buffer| {
                let cursor = self.cursor();
                let line_start = buffer.line_start(buffer.line_of(cursor));
                let text = buffer.slice(Span::new(line_start, cursor));
                let spaces = text.len() - text.trim_end_matches(' ').len();
                if spaces > 0 {
                    spaces.min(self.tab_width())
                } else if text.ends_with('\t') {
                    1
                } else {
                    0
                }
            })
            .unwrap_or(0);
        if removed == 0 {
            return;
        }
        let cursor = self.cursor();
        self.apply(
            vec![Change::delete(Span::new(cursor - removed, cursor))],
            ChangeGroup::Insert,
        );
        self.set_cursor(cursor - removed);
        for _ in 0..removed {
            self.insert_batch.pop();
        }
    }

    fn insert_backspace(&mut self) {
        if self.mode.is_visual() {
            let region = self.selection_region();
            if !region.is_empty() {
                let start = region.extent().start;
                self.apply(operators::delete_changes(&region), ChangeGroup::Insert);
                self.set_cursor(start);
                self.mode = Mode::Insert;
            }
            return;
        }
        let span = self
            .current_buffer()
            .map(|buffer| {
                let cursor = self.cursor();
                let start = if self.expand_tab() {
                    // Delete a run of spaces one indent unit at a time.
                    let line_start = buffer.line_start(buffer.line_of(cursor));
                    let text = buffer.slice(Span::new(line_start, cursor));
                    let spaces = text.len() - text.trim_end_matches(' ').len();
                    if spaces > 0 {
                        cursor - spaces.min(self.tab_width())
                    } else {
                        buffer
                            .char_before(cursor)
                            .map(|(offset, _)| offset)
                            .unwrap_or(cursor)
                    }
                } else {
                    buffer
                        .char_before(cursor)
                        .map(|(offset, _)| offset)
                        .unwrap_or(cursor)
                };
                Span::new(start, cursor)
            })
            .filter(|span| span.end > span.start);
        let Some(span) = span else { return };
        let removed = self
            .current_buffer()
            .map(|buffer| buffer.slice(span))
            .unwrap_or_default();
        self.apply(vec![Change::delete(span)], ChangeGroup::Insert);
        self.set_cursor(span.start);
        for _ in removed.chars() {
            self.insert_batch.pop();
        }
    }

    fn insert_delete(&mut self) {
        let span = self
            .current_buffer()
            .map(|buffer| {
                let cursor = self.cursor();
                Span::new(cursor, next_boundary(buffer, cursor))
            })
            .filter(|span| span.end > span.start);
        if let Some(span) = span {
            self.apply(vec![Change::delete(span)], ChangeGroup::Insert);
        }
    }

    fn insert_delete_word(&mut self) {
        let span = self
            .current_buffer()
            .map(|buffer| {
                let cursor = self.cursor();
                let target = motions::word_back(buffer, cursor, 1, false);
                Span::new(target.min(cursor), cursor)
            })
            .filter(|span| span.end > span.start);
        let Some(span) = span else { return };
        let removed = self
            .current_buffer()
            .map(|buffer| buffer.slice(span))
            .unwrap_or_default();
        self.apply(vec![Change::delete(span)], ChangeGroup::Insert);
        self.set_cursor(span.start);
        for _ in removed.chars() {
            self.insert_batch.pop();
        }
    }

    fn insert_delete_line_start(&mut self) {
        let span = self
            .current_buffer()
            .map(|buffer| {
                let cursor = self.cursor();
                Span::new(buffer.line_start(buffer.line_of(cursor)), cursor)
            })
            .filter(|span| span.end > span.start);
        if let Some(span) = span {
            self.apply(vec![Change::delete(span)], ChangeGroup::Insert);
            self.set_cursor(span.start);
        }
    }

    // ------------------------------------------------------------- paste

    fn put(&mut self, position: PutPosition, count: usize) -> Vec<Intent> {
        let register = self.pending.register.take().unwrap_or('"');
        let value = self.registers.get(register);
        if value.is_empty() {
            return vec![Intent::Message(
                StatusLevel::Warn,
                format!("register \"{register}\" is empty"),
            )];
        }
        let plan = self.current_buffer().map(|buffer| {
            let selection = self.selection();
            let replace = self.mode.is_visual() && !selection.primary().is_empty();
            let region = if replace {
                Some(self.selection_region())
            } else {
                None
            };
            let anchor = region
                .as_ref()
                .map(|region| region.extent().start)
                .unwrap_or_else(|| selection.cursor());
            let at = if region.is_some() {
                anchor
            } else if value.linewise {
                match position {
                    PutPosition::BeforeCursor | PutPosition::BeforeLine => {
                        buffer.line_start(buffer.line_of(anchor))
                    }
                    PutPosition::AfterCursor | PutPosition::AfterLine => {
                        buffer.line_range_with_ending(buffer.line_of(anchor)).end
                    }
                }
            } else {
                match position {
                    PutPosition::AfterCursor | PutPosition::AfterLine => {
                        next_boundary(buffer, anchor)
                    }
                    PutPosition::BeforeCursor | PutPosition::BeforeLine => anchor,
                }
            };
            (at, region)
        });
        let Some((at, region)) = plan else {
            return Vec::new();
        };

        let mut payload = value.text.clone();
        if value.linewise && !payload.ends_with('\n') {
            payload.push('\n');
        }
        let mut insertion = String::new();
        for _ in 0..count.max(1) {
            insertion.push_str(&payload);
        }
        let mut changes = Vec::new();
        if let Some(region) = &region {
            changes.extend(operators::delete_changes(region));
        }
        changes.push(Change::insert(at, insertion.clone()));
        self.apply(changes, ChangeGroup::Command);

        let landing = if value.linewise {
            match position {
                PutPosition::BeforeCursor | PutPosition::BeforeLine => at,
                _ => at + insertion.len().saturating_sub(2),
            }
        } else {
            at + insertion.len().saturating_sub(1)
        };
        let landing = self
            .current_buffer()
            .map(|buffer| clamp_cursor(buffer, landing))
            .unwrap_or(0);
        self.set_cursor(landing);
        self.mode = Mode::Normal;
        self.repeatable = Some(Repeatable::Put { register, position });
        vec![Intent::EnsureVisible]
    }

    // ------------------------------------------------------------- history

    fn undo(&mut self) -> Vec<Intent> {
        let Some(id) = self.current_buffer_id() else {
            return Vec::new();
        };
        let selection = self.selection();
        let _selection = selection;
        let outcome = self.buffer_mut(id).and_then(|buffer| buffer.undo());
        match outcome {
            Some(outcome) => {
                self.set_selection(clamped_to_buffer(self.current_buffer(), outcome.selection));
                self.mode = Mode::Normal;
                vec![
                    Intent::Message(StatusLevel::Info, self.undo_message("Undo", id)),
                    Intent::EnsureVisible,
                    Intent::Parse(id),
                ]
            }
            None => vec![Intent::Message(StatusLevel::Warn, "nothing to undo".into())],
        }
    }

    fn redo(&mut self) -> Vec<Intent> {
        let Some(id) = self.current_buffer_id() else {
            return Vec::new();
        };
        let selection = self.selection();
        let _selection = selection;
        let outcome = self.buffer_mut(id).and_then(|buffer| buffer.redo());
        match outcome {
            Some(outcome) => {
                self.set_selection(clamped_to_buffer(self.current_buffer(), outcome.selection));
                self.mode = Mode::Normal;
                vec![
                    Intent::Message(StatusLevel::Info, self.undo_message("Redo", id)),
                    Intent::EnsureVisible,
                    Intent::Parse(id),
                ]
            }
            None => vec![Intent::Message(StatusLevel::Warn, "nothing to redo".into())],
        }
    }

    /// Replay the last change.
    fn repeat(&mut self, area: Rect) -> Vec<Intent> {
        match self.repeatable.clone() {
            Some(Repeatable::Insert { text }) => {
                self.enter_insert();
                self.insert_text(&text);
                self.exit_insert();
                vec![Intent::EnsureVisible]
            }
            Some(Repeatable::Operator {
                operator,
                target,
                count,
                char_arg,
            }) => {
                let mut intents = self.run_operator(operator, target, count, char_arg, area);
                if self.mode == Mode::Insert {
                    self.exit_insert();
                }
                intents.push(Intent::Message(StatusLevel::Info, ".".into()));
                intents
            }
            Some(Repeatable::Put { register, position }) => {
                self.pending.register = Some(register);
                self.put(position, 1)
            }
            None => vec![Intent::Message(
                StatusLevel::Warn,
                "nothing to repeat".into(),
            )],
        }
    }

    // ------------------------------------------------------------- files

    /// Write the focused buffer, or ask for a path when it has none.
    fn save(&mut self, path: Option<&Path>) -> Option<Intent> {
        let id = self.current_buffer_id()?;
        if let Some(path) = path {
            let buffer = self.buffer_mut(id)?;
            return match buffer.save_as(path) {
                Ok(outcome) => {
                    self.layout.on_focus_changed(&self.config.layout);
                    Some(Intent::Message(StatusLevel::Info, outcome.summary()))
                }
                Err(error) => Some(Intent::Message(StatusLevel::Error, error.to_string())),
            };
        }
        if self.buffer(id)?.path().is_none() {
            return Some(Intent::OpenPrompt(PromptSpec::Path {
                initial: String::new(),
            }));
        }
        let name = self.buffer(id)?.display_name();
        match self.buffer_mut(id)?.save() {
            Ok(Some(outcome)) => Some(Intent::Message(StatusLevel::Info, outcome.summary())),
            Ok(None) => Some(Intent::Message(
                StatusLevel::Info,
                format!("{name} unchanged"),
            )),
            Err(error) => Some(Intent::Message(StatusLevel::Error, error.to_string())),
        }
    }

    /// Write the focused buffer to an explicit path (`:write <path>`).
    pub fn write_to(&mut self, path: &Path) -> Intent {
        match self.save(Some(path)) {
            Some(intent) => intent,
            None => Intent::Message(StatusLevel::Error, "no buffer to write".into()),
        }
    }

    fn save_all(&mut self) -> Vec<Intent> {
        let ids: Vec<BufferId> = self.buffers.iter().map(|buffer| buffer.id).collect();
        let mut saved = 0usize;
        let mut failures = Vec::new();
        for id in ids {
            let worth_saving = self
                .buffer(id)
                .is_some_and(|buffer| buffer.path().is_some() && buffer.is_dirty());
            if !worth_saving {
                continue;
            }
            match self
                .buffer_mut(id)
                .and_then(|buffer| buffer.save().transpose())
            {
                Some(Ok(_)) => saved += 1,
                Some(Err(error)) => failures.push(error.to_string()),
                None => {}
            }
        }
        let message = if failures.is_empty() {
            format!("saved {saved} buffer(s)")
        } else {
            format!("saved {saved}; {}", failures.join("; "))
        };
        vec![Intent::Message(
            if failures.is_empty() {
                StatusLevel::Info
            } else {
                StatusLevel::Error
            },
            message,
        )]
    }

    fn reload_file(&mut self, force: bool) -> Option<Intent> {
        let id = self.current_buffer_id()?;
        let dirty = self.buffer(id).is_some_and(|buffer| buffer.is_dirty());
        if dirty && !force {
            return Some(Intent::Message(
                StatusLevel::Error,
                "unsaved changes; :edit! discards them".to_string(),
            ));
        }
        let mut selection = self.selection();
        let result = self.buffer_mut(id)?.reload(&mut selection);
        self.set_selection(selection);
        match result {
            Ok(bytes) => Some(Intent::Message(
                StatusLevel::Info,
                format!("reloaded {bytes} bytes"),
            )),
            Err(error) => Some(Intent::Message(StatusLevel::Error, error.to_string())),
        }
    }

    fn quit(&mut self, force: bool) -> Option<Intent> {
        if self.layout.panes().len() > 1 {
            // `:q` closes the window first, like vim.
            return self.close_pane();
        }
        if !force
            && let Some(buffer) = self.current_buffer_id().and_then(|id| self.buffer(id))
            && buffer.is_dirty()
        {
            return Some(Intent::Message(
                StatusLevel::Error,
                format!(
                    "{} has unsaved changes (use ZQ or :q!)",
                    buffer.display_name()
                ),
            ));
        }
        Some(Intent::Quit)
    }

    fn quit_all(&mut self) -> Option<Intent> {
        let dirty = self.dirty_buffers();
        if !dirty.is_empty() {
            return Some(Intent::Message(
                StatusLevel::Error,
                format!("{} buffer(s) have unsaved changes", dirty.len()),
            ));
        }
        Some(Intent::Quit)
    }

    fn close_pane(&mut self) -> Option<Intent> {
        let focused = self.layout.focused()?;
        if self.layout.panes().len() <= 1 {
            return Some(Intent::Quit);
        }
        self.layout.remove(focused);
        self.layout.on_focus_changed(&self.config.layout);
        Some(Intent::EnsureVisible)
    }

    // ------------------------------------------------------------- layout

    fn split(&mut self, axis: Option<Axis>, area: Rect) -> Option<Intent> {
        let buffer = self.current_buffer_id()?;
        let area = self.text_area(area);
        self.layout
            .split_focused(axis, &self.config.layout, area, |id| Pane::new(id, buffer))?;
        self.layout.on_focus_changed(&self.config.layout);
        Some(Intent::EnsureVisible)
    }

    /// Area available for panes inside the terminal.
    ///
    /// The status row is one row wherever `[status] position` puts it, and there
    /// is no command row any more — transient text floats over the panes on a
    /// card instead of taking a row of its own.
    pub fn text_area(&self, area: Rect) -> Rect {
        let status = 1.min(area.height);
        if self.config.status.position.is_top() {
            Rect {
                y: area.y + status,
                height: area.height.saturating_sub(status),
                ..area
            }
        } else {
            Rect {
                height: area.height.saturating_sub(status),
                ..area
            }
        }
    }

    /// Indentation width for the focused buffer, honouring `[editor.indent.*]`.
    pub fn tab_width(&self) -> usize {
        match self.current_buffer() {
            Some(buffer) => self.config.editor.tab_width_for(buffer.language.id),
            None => self.config.editor.tab_width.max(1) as usize,
        }
    }

    /// Whether the focused buffer indents with spaces, honouring
    /// `[editor.indent.*]`.
    pub fn expand_tab(&self) -> bool {
        match self.current_buffer() {
            Some(buffer) => self.config.editor.expand_tab_for(buffer.language.id),
            None => self.config.editor.expand_tab,
        }
    }

    // ------------------------------------------------------------- search

    /// Set the search term and refresh the highlight set.
    ///
    /// Case follows `[search] ignore-case`, and an uppercase pattern turns case
    /// sensitivity back on when `smart-case` is enabled.
    pub fn set_search(&mut self, pattern: &str) -> Vec<Intent> {
        if pattern.is_empty() {
            self.search = None;
            self.search_matches.clear();
            return Vec::new();
        }
        let case_sensitive = !self.config.search.ignore_case
            || (self.config.search.smart_case && pattern.chars().any(char::is_uppercase));
        self.search = Some(SearchQuery {
            pattern: pattern.to_string(),
            case_sensitive,
            backward: false,
        });
        self.registers.search = Some(pattern.to_string());
        self.refresh_search_matches();
        vec![Intent::Message(
            StatusLevel::Info,
            format!("{} match(es) for \"{pattern}\"", self.search_matches.len()),
        )]
    }

    pub fn refresh_search_matches(&mut self) {
        let matches = match &self.search {
            Some(query) if self.config.search.highlight => self
                .current_buffer()
                .map(|buffer| {
                    motions::search_matches(buffer, query, self.config.search.max_results)
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        self.search_matches = matches;
    }

    /// Jump to the next match of the current term.
    pub fn search_next(&mut self, backward: bool, count: usize) -> Vec<Intent> {
        let Some(query) = self.search.clone() else {
            return vec![Intent::Message(StatusLevel::Warn, "no search term".into())];
        };
        let found = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            let cursor = self.cursor();
            let direction = backward ^ query.backward;
            motions::search_next(buffer, cursor, &query, direction, count.max(1))
                .map(|at| (cursor, at))
        };
        match found {
            Some((before, at)) => {
                if let Some(id) = self.current_buffer_id() {
                    self.jumplist.push((id, before));
                }
                let at = self
                    .current_buffer()
                    .map(|buffer| clamp_cursor(buffer, at))
                    .unwrap_or(0);
                self.set_cursor(at);
                vec![Intent::EnsureVisible]
            }
            None => vec![Intent::Message(
                StatusLevel::Warn,
                format!("pattern not found: {}", query.pattern),
            )],
        }
    }

    /// `*` / `#`: search for the word under the cursor.
    fn find_word(&mut self, forward: bool) -> Vec<Intent> {
        let Some(word) = self
            .current_buffer()
            .and_then(|buffer| buffer.word_under_cursor(self.cursor()))
        else {
            return vec![Intent::Message(
                StatusLevel::Warn,
                "no word under cursor".into(),
            )];
        };
        let intents = self.set_search(&word);
        if self.search_matches.is_empty() {
            return intents;
        }
        let mut intents = intents;
        intents.extend(self.search_next(!forward, 1));
        intents
    }

    /// Move to a search hit in another file or buffer.
    pub fn goto_hit(&mut self, path: &Path, line: usize) -> Vec<Intent> {
        let mut intents = Vec::new();
        let existing = self.buffer_by_path(&expand_home(path));
        let id = match existing {
            Some(id) => id,
            None => {
                let (id, created) = self.open_path(path);
                if created {
                    intents.push(Intent::Parse(id));
                }
                id
            }
        };
        self.focus_buffer(id);
        let target = self
            .buffer(id)
            .map(|buffer| {
                let line = line.saturating_sub(1).min(buffer.last_line());
                buffer.first_non_blank(line)
            })
            .unwrap_or(0);
        self.set_cursor(target);
        intents.push(Intent::EnsureVisible);
        intents
    }

    pub fn goto_line(&mut self, line: usize) -> Vec<Intent> {
        let target = self
            .current_buffer()
            .map(|buffer| {
                let line = line.saturating_sub(1).min(buffer.last_line());
                buffer.first_non_blank(line)
            })
            .unwrap_or(0);
        let before = self.cursor();
        if let Some(id) = self.current_buffer_id() {
            self.jumplist.push((id, before));
        }
        self.set_cursor(target);
        vec![Intent::EnsureVisible]
    }

    // ------------------------------------------------------------- viewport

    /// Keep the cursor inside the focused pane, scrolling when needed.
    pub fn ensure_visible(&mut self, area: Rect) {
        let height = self.text_height(area);
        let margin = (self.config.editor.scroll_margin as usize).min(height / 2);
        let Some(focused) = self.layout.focused() else {
            return;
        };
        let plan = self
            .current_buffer()
            .map(|buffer| (buffer.line_of(self.cursor()), 0usize));
        let Some((line, _)) = plan else { return };
        let scroll = self
            .layout
            .pane(focused)
            .map(|pane| pane.view.scroll_line)
            .unwrap_or(0);
        let mut next = scroll;
        if line < scroll + margin {
            next = line.saturating_sub(margin);
        } else if line + margin + 1 > scroll + height {
            next = line + margin + 1 - height;
        }
        // Keep the viewport inside the buffer for *this* pane's height, so a
        // pane that shrank (a new split, a smaller terminal) cannot be scrolled
        // past its last line.
        let lines = self
            .current_buffer()
            .map(|buffer| buffer.display_line_count())
            .unwrap_or(1);
        next = next.min(lines.saturating_sub(height));
        if let Some(pane) = self.layout.pane_mut(focused) {
            pane.view.scroll_line = next;
        }
    }

    fn recenter(&mut self, area: Rect) -> Option<Intent> {
        let height = self.text_height(area);
        let line = self.line_at(self.cursor());
        let scroll = line.saturating_sub(height / 2);
        if let Some(pane) = self.layout.focused_pane_mut() {
            pane.view.scroll_line = scroll;
        }
        Some(Intent::EnsureVisible)
    }

    /// `]d` / `[d`: move to the next or previous diagnostic, wrapping around.
    ///
    /// Diagnostics are ordered by position so the jump follows the text rather
    /// than the order the servers happened to publish them in.  The target line
    /// is centred, which is what makes a jump read as "take me there" instead of
    /// "nudge the caret".
    ///
    /// Project scope walks every buffer in editor order — buffers the language
    /// clients have reported on — so a jump can leave the current file; the
    /// status message then names the file, since the caret moved somewhere the
    /// user was not looking.
    fn diagnostic_jump(
        &mut self,
        forward: bool,
        scope: DiagnosticScope,
        area: Rect,
    ) -> Vec<Intent> {
        match scope {
            DiagnosticScope::Buffer => self.diagnostic_jump_buffer(forward, area),
            DiagnosticScope::Project => self.diagnostic_jump_project(forward, area),
        }
    }

    fn diagnostic_jump_buffer(&mut self, forward: bool, area: Rect) -> Vec<Intent> {
        let Some(buffer) = self.current_buffer() else {
            return Vec::new();
        };
        let cursor = self.cursor();
        let mut starts: Vec<usize> = buffer
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.range.start)
            .collect();
        if starts.is_empty() {
            return vec![Intent::Message(
                StatusLevel::Info,
                "no diagnostics in this buffer".into(),
            )];
        }
        starts.sort_unstable();
        starts.dedup();
        let target = if forward {
            starts
                .iter()
                .find(|start| **start > cursor)
                .copied()
                .unwrap_or(starts[0])
        } else {
            starts
                .iter()
                .rev()
                .find(|start| **start < cursor)
                .copied()
                .unwrap_or(*starts.last().unwrap())
        };
        let message = buffer
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.range.start == target)
            .map(|diagnostic| format!("{}: {}", diagnostic.severity.label(), diagnostic.message))
            .unwrap_or_default();
        self.set_cursor(target);
        self.center_cursor(area);
        vec![
            Intent::EnsureVisible,
            Intent::Message(StatusLevel::Info, message),
        ]
    }

    /// Project-scope `]d` / `[d`: the targets are `(buffer index, offset)`
    /// pairs over every buffer, so the walk continues into the next file
    /// instead of stopping at the end of this one.
    fn diagnostic_jump_project(&mut self, forward: bool, area: Rect) -> Vec<Intent> {
        // Buffer targets keep their index so ordering and wrap match what they
        // always did.  Workspace files with no open buffer get indices after the
        // last buffer, so a forward walk exhausts open buffers first then moves
        // into unopened project files (and wraps back to them).
        let mut targets: Vec<(usize, usize, String)> = Vec::new();
        for (index, buffer) in self.buffers.iter().enumerate() {
            let mut starts: Vec<usize> = buffer
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.range.start)
                .collect();
            starts.sort_unstable();
            starts.dedup();
            for start in starts {
                let message = buffer
                    .diagnostics
                    .iter()
                    .find(|diagnostic| diagnostic.range.start == start)
                    .map(|diagnostic| {
                        format!(
                            "{}: {}: {}",
                            buffer.display_name(),
                            diagnostic.severity.label(),
                            diagnostic.message
                        )
                    })
                    .unwrap_or_default();
                targets.push((index, start, message));
            }
        }
        // Stored diagnostics for files that are not open as buffers.  They sort
        // after every buffer so the walk reads "open files first, then the rest
        // of the workspace".
        let mut file_targets: Vec<(PathBuf, usize, String)> = self
            .project_diagnostics
            .iter()
            .filter(|(path, _)| self.buffer_by_path(path).is_none())
            .flat_map(|(path, entries)| {
                let mut sorted: Vec<&ProjectDiagnostic> = entries.iter().collect();
                sorted.sort_by_key(|entry| entry.offset);
                sorted
                    .into_iter()
                    .map(move |entry| (path.clone(), entry.offset, entry.diagnostic.headline()))
            })
            .collect();
        file_targets.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let base = self.buffers.len();
        for (index, (path, offset, message)) in file_targets.iter().enumerate() {
            targets.push((
                base + index,
                *offset,
                format!("{}: {}", path.display(), message),
            ));
        }
        if targets.is_empty() {
            return vec![Intent::Message(
                StatusLevel::Info,
                "no diagnostics in the project".into(),
            )];
        }
        let current = self.current_buffer_id().and_then(|id| {
            self.buffers
                .iter()
                .position(|buffer| buffer.id == id)
                .map(|index| (index, self.cursor()))
        });
        let after = |entry: &(usize, usize, String)| match current {
            Some((index, cursor)) => entry.0 > index || (entry.0 == index && entry.1 > cursor),
            None => true,
        };
        let before = |entry: &(usize, usize, String)| match current {
            Some((index, cursor)) => entry.0 < index || (entry.0 == index && entry.1 < cursor),
            None => false,
        };
        let target = if forward {
            targets
                .iter()
                .find(|entry| after(entry))
                .unwrap_or(&targets[0])
        } else {
            targets
                .iter()
                .rev()
                .find(|entry| before(entry))
                .unwrap_or(targets.last().expect("non-empty"))
        };
        let (index, offset, message) = (target.0, target.1, target.2.clone());
        // A file with no open buffer: hand it to the app layer to open and
        // register before we can focus it.
        if index >= base {
            let path = &file_targets[index - base].0;
            return vec![Intent::OpenDiagnosticFile {
                path: path.clone(),
                offset,
            }];
        }
        let id = self.buffers[index].id;
        if self.current_buffer_id() != Some(id) && !self.focus_buffer(id) {
            // Cross-buffer: focus first so the scroll lands in the right pane.
            return Vec::new();
        }
        self.set_cursor(offset);
        self.center_cursor(area);
        vec![
            Intent::EnsureVisible,
            Intent::Message(StatusLevel::Info, message),
        ]
    }

    /// Centre the caret's line in the focused pane.
    fn center_cursor(&mut self, area: Rect) {
        let height = self.text_height(area);
        let line = self.line_at(self.cursor());
        if let Some(pane) = self.layout.focused_pane_mut() {
            pane.view.scroll_line = line.saturating_sub(height / 2);
        }
    }

    fn scroll_to_edge(&mut self, top: bool, area: Rect) -> Option<Intent> {
        let height = self.text_height(area);
        let scroll = self.current_pane()?.view.scroll_line;
        let line = if top {
            scroll
        } else {
            scroll + height.saturating_sub(1)
        };
        let at = self
            .current_buffer()
            .map(|buffer| buffer.line_start(line.min(buffer.last_line())))?;
        self.set_cursor(at);
        Some(Intent::EnsureVisible)
    }

    fn scroll_lines(&mut self, direction: Direction) -> Option<Intent> {
        let count = self.pending.take_count().max(1);
        let focused = self.layout.focused()?;
        let max = self.current_buffer()?.last_line() as i32;
        let scroll = self.layout.pane(focused)?.view.scroll_line as i32;
        let next = (scroll + direction.sign() * count as i32).clamp(0, max) as usize;
        if let Some(pane) = self.layout.pane_mut(focused) {
            pane.view.scroll_line = next;
        }
        Some(Intent::EnsureVisible)
    }

    fn jump(&mut self, back: bool) -> Option<Intent> {
        let entry = if back {
            self.jumplist.back()?
        } else {
            self.jumplist.forward()?
        };
        let (buffer, offset) = entry;
        if self.current_buffer_id() != Some(buffer) && !self.focus_buffer(buffer) {
            return None;
        }
        let at = self
            .buffer(buffer)
            .map(|buffer| buffer.clamp_byte(offset))?;
        self.set_cursor(at);
        Some(Intent::EnsureVisible)
    }

    // ------------------------------------------------------------- marks

    fn set_mark(&mut self, name: char) -> Vec<Intent> {
        let Some(id) = self.current_buffer_id() else {
            return Vec::new();
        };
        let cursor = self.cursor();
        if let Some(buffer) = self.buffer_mut(id) {
            buffer.marks.insert(name, cursor);
        }
        vec![Intent::Message(
            StatusLevel::Info,
            format!("mark {name} set"),
        )]
    }

    fn goto_mark(&mut self, name: char, line: bool) -> Vec<Intent> {
        let target = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            let at = match name {
                '.' => buffer.last_change_pos,
                _ => buffer.marks.get(&name).copied(),
            };
            at.map(|at| {
                if line {
                    buffer.first_non_blank(buffer.line_of(at))
                } else {
                    buffer.clamp_byte(at)
                }
            })
        };
        let Some(target) = target else {
            return vec![Intent::Message(
                StatusLevel::Warn,
                format!("mark {name} is not set"),
            )];
        };
        let before = self.cursor();
        if let Some(id) = self.current_buffer_id() {
            self.jumplist.push((id, before));
        }
        self.set_cursor(target);
        vec![Intent::EnsureVisible]
    }

    // ------------------------------------------------------------- results

    /// Adopt a tree handed back by the parser task.
    ///
    /// Returns false when the result is stale, which is normal: it means the
    /// buffer changed while the parse was running.  Either way the job is over,
    /// so the status has to stop saying `Pending` — otherwise the buffer waits
    /// on a parse that is already finished and highlighting freezes for the rest
    /// of the session.
    pub fn adopt_tree(
        &mut self,
        id: BufferId,
        version: u64,
        slot: crate::syntax::TreeSlot,
    ) -> bool {
        let Some(buffer) = self.buffer_mut(id) else {
            return false;
        };
        if buffer.version != version {
            // The tree describes text that no longer exists, so it cannot be
            // painted onto this one.  Keep drawing with the previous tree and
            // let the idle timer re-parse.
            buffer.syntax.status = crate::syntax::ParseStatus::Stale;
            return false;
        }
        match slot.take() {
            Some(tree) => {
                let has_errors = tree.root_node().has_error();
                buffer.syntax.tree = Some(tree);
                buffer.syntax.tree_version = version;
                buffer.syntax.status = crate::syntax::ParseStatus::Current;
                buffer.syntax.has_errors = has_errors;
                true
            }
            None => {
                // The job reported back with nothing: this text does not parse.
                // Pin the version so the idle timer does not retry it every
                // tick; the next edit makes it interesting again.
                buffer.syntax.tree_version = version;
                buffer.syntax.status = crate::syntax::ParseStatus::Failed;
                false
            }
        }
    }

    /// Apply formatter output, keeping each pane's caret on the text it was on.
    pub fn apply_format(&mut self, id: BufferId, text: &str) -> Result<bool, String> {
        let current = self
            .buffer(id)
            .map(|buffer| buffer.text())
            .ok_or_else(|| "buffer is gone".to_string())?;
        if current == text {
            return Ok(false);
        }
        let changes = {
            let buffer = self.buffer(id).expect("checked above");
            crate::format::diff_changes(&buffer.rope, text)
        };
        if changes.is_empty() {
            return Ok(false);
        }
        // `Format` groups so undo walks back over a whole format run at once —
        // and so an automatic run is not a step of its own (`Buffer::undo`
        // folds it into the edit it followed).  `:format` is something the user
        // did, so that one is recorded as a normal change.
        let asked = self.format_requested == Some(id);
        self.format_requested = None;
        let group = if asked {
            ChangeGroup::Command
        } else {
            ChangeGroup::Format
        };
        self.apply_rewrite(id, changes, group);
        Ok(true)
    }

    /// Apply `changes` to `id` and put every pane's caret back where it belongs.
    ///
    /// One buffer edit, not one per pane: applying per pane would rewrite the
    /// text again for every pane showing the buffer, duplicating the
    /// replacement's tail.  Each pane then maps *its own* caret through the
    /// edits it started from — taking the caret from whichever pane happens to
    /// be first would move the focused pane's caret to where an unfocused one
    /// was, and collapsing every selection to a point would throw away visual
    /// selections and extra cursors.
    ///
    /// Shared by the external formatter and the language server's edits so the
    /// two cannot disagree about where the caret lands.  Returns the number of
    /// edits applied.
    fn apply_rewrite(&mut self, id: BufferId, changes: Vec<Change>, group: ChangeGroup) -> usize {
        let panes = self.layout.panes_showing(id);
        let before: Vec<(PaneId, Selection, (usize, usize))> = {
            let Some(buffer) = self.buffer(id) else {
                return 0;
            };
            panes
                .iter()
                .filter_map(|pane_id| self.layout.pane(*pane_id))
                .map(|pane| {
                    let selection = pane.selection.clone();
                    let position = crate::format::position_of(&buffer.rope, selection.cursor());
                    (pane.id, selection, position)
                })
                .collect()
        };
        let mut selection = panes
            .iter()
            .find(|pane_id| self.layout.is_focused(**pane_id))
            .or_else(|| panes.first())
            .and_then(|pane_id| self.layout.pane(*pane_id))
            .map(|pane| pane.selection.clone())
            .unwrap_or_else(|| Selection::point(0));
        let edits = {
            let Some(buffer) = self.buffer_mut(id) else {
                return 0;
            };
            buffer.apply(changes, group, &mut selection)
        };
        let restored: Vec<(PaneId, Selection)> = {
            let Some(buffer) = self.buffer(id) else {
                return 0;
            };
            before
                .iter()
                .map(|(pane_id, selection, position)| {
                    (
                        *pane_id,
                        crate::format::selection_after_rewrite(
                            *position,
                            &buffer.rope,
                            &edits,
                            selection,
                        ),
                    )
                })
                .collect()
        };
        for (pane_id, selection) in restored {
            if let Some(pane) = self.layout.pane_mut(pane_id) {
                pane.selection = selection;
                // The caret may be on a different line now, so a column remembered
                // from before the rewrite would walk the next `j`/`k` sideways.
                pane.view.wanted_col = None;
            }
        }
        if let Some(buffer) = self.buffer_mut(id) {
            buffer.break_undo_group();
        }
        edits.len()
    }

    /// Replace a span with `text` (the spelling picker).
    pub fn replace_span(&mut self, span: Span, text: &str) -> Vec<Intent> {
        let landing = span.start + text.len();
        self.apply(vec![Change::replace(span, text)], ChangeGroup::Command);
        self.set_cursor(landing);
        vec![Intent::EnsureVisible]
    }

    /// Apply a set of server-supplied edits to `id` as one undo step.
    ///
    /// The edits arrive as byte spans in the text the server was told about, so
    /// they are applied back-to-front: applying them in order would shift every
    /// later span by the length of the earlier replacement.  Returns how many
    /// edits landed, so the caller can say "nothing to do" when the server sent
    /// an empty list.
    pub fn apply_edits(
        &mut self,
        id: BufferId,
        edits: &[crate::lsp::TextEdit],
        group: ChangeGroup,
    ) -> usize {
        if edits.is_empty() || self.buffer(id).is_none_or(|buffer| buffer.read_only) {
            return 0;
        }
        let mut ordered: Vec<&crate::lsp::TextEdit> = edits.iter().collect();
        ordered.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        let changes: Vec<Change> = ordered
            .into_iter()
            .map(|edit| Change::replace(edit.range, edit.new_text.clone()))
            .collect();
        // Every pane showing the buffer keeps its own caret, the same way a
        // formatter run does: a server edit can move text under a pane the user
        // is not looking at, and raw edit mapping would leave that caret on the
        // wrong line.
        self.apply_rewrite(id, changes, group)
    }

    /// Store diagnostics from a background job, replacing that source's set.
    pub fn set_diagnostics(
        &mut self,
        id: BufferId,
        source: DiagnosticSource,
        diagnostics: Vec<Diagnostic>,
    ) {
        if let Some(buffer) = self.buffer_mut(id) {
            buffer
                .diagnostics
                .retain(|existing| existing.source != source);
            buffer.diagnostics.extend(diagnostics);
        }
    }

    /// Store diagnostics for a workspace file that has no open buffer, replacing
    /// that source's set.  Keyed by absolute path so the project scope can find
    /// them without holding every file as a buffer.
    pub fn set_project_diagnostics(
        &mut self,
        path: PathBuf,
        source: DiagnosticSource,
        diagnostics: Vec<ProjectDiagnostic>,
    ) {
        let entry = self.project_diagnostics.entry(path).or_default();
        entry.retain(|existing| existing.diagnostic.source != source);
        entry.extend(diagnostics);
    }

    /// Diagnostics for a workspace file that is not open as a buffer.
    pub fn project_diagnostics(&self, path: &Path) -> Option<&[ProjectDiagnostic]> {
        self.project_diagnostics.get(path).map(Vec::as_slice)
    }

    /// Drop the stored diagnostics for `path`, returning them so a caller can
    /// move them onto a buffer that just opened.
    pub fn take_project_diagnostics(&mut self, path: &Path) -> Vec<ProjectDiagnostic> {
        self.project_diagnostics.remove(path).unwrap_or_default()
    }

    /// Suggestions for the word under the cursor.
    fn spell_suggest(&mut self) -> Vec<Intent> {
        let (word, span, suggestions) = {
            let Some(buffer) = self.current_buffer() else {
                return Vec::new();
            };
            let cursor = self.cursor();
            let Some(word) = buffer.word_under_cursor(cursor) else {
                return vec![Intent::Message(
                    StatusLevel::Warn,
                    "no word under cursor".into(),
                )];
            };
            let span = buffer.word_at(cursor);
            let suggestions = buffer
                .diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.source == DiagnosticSource::Spell
                        && diagnostic.range.contains(cursor)
                })
                .map(|diagnostic| diagnostic.suggestions.clone())
                .unwrap_or_default();
            (word, span, suggestions)
        };
        if suggestions.is_empty() {
            return vec![Intent::Message(
                StatusLevel::Info,
                format!("\"{word}\" is not flagged (is [spell] enable on?)"),
            )];
        }
        vec![Intent::OpenPrompt(PromptSpec::Spell {
            word,
            span,
            suggestions,
        })]
    }

    // ------------------------------------------------------------- options

    /// Flip a `[section] key` option through the same validation `:set` uses.
    fn toggle_option(&mut self, option: ToggleOption) -> Vec<Intent> {
        let key = option.key();
        let result = match option {
            ToggleOption::LineNumbers => {
                let next = match self.config.editor.line_number {
                    LineNumber::None => "relative",
                    _ => "none",
                };
                self.config.set_value(key, next)
            }
            ToggleOption::AutoFormat => {
                let next = if self.config.format.on_idle.is_disabled() {
                    "500ms"
                } else {
                    "off"
                };
                self.config.set_value(key, next)
            }
            _ => {
                let current = self.config.value_of(key).unwrap_or_default();
                let next = if current == "on" { "false" } else { "true" };
                self.config.set_value(key, next)
            }
        };
        if let Err(error) = result {
            return vec![Intent::Message(StatusLevel::Error, error)];
        }
        let mut intents = vec![Intent::Message(
            StatusLevel::Info,
            format!("{key} = {}", self.config.value_of(key).unwrap_or_default()),
        )];
        if option == ToggleOption::SpellCheck {
            match (self.current_buffer_id(), self.config.spell.enable) {
                (Some(id), true) => intents.push(Intent::Spell(id)),
                (Some(id), false) => self.set_diagnostics(id, DiagnosticSource::Spell, Vec::new()),
                (None, _) => {}
            }
        }
        intents
    }

    /// Schedule an idle format if `[format] on-idle` is set.
    ///
    /// Held while insert mode is open in the buffer: the formatter rewrites the
    /// whole buffer and parks the caret back on its old line and column, so a
    /// run that lands mid-typing deletes the line `o` has just opened.  Leaving
    /// insert mode arms the same timer instead.
    pub fn schedule_auto_format(&mut self, id: BufferId) {
        if self.mode == Mode::Insert && self.current_buffer_id() == Some(id) {
            self.format_due = None;
            self.format_held = Some(id);
            return;
        }
        let delay = self.config.format.on_idle;
        if delay.is_disabled() {
            self.format_due = None;
            return;
        }
        let language = match self.buffer(id) {
            Some(buffer) => buffer.language.id,
            None => return,
        };
        if crate::format::formatter_for(language, &self.config).is_none() {
            return;
        }
        self.format_due = Some((
            id,
            std::time::Instant::now() + std::time::Duration::from_millis(delay.as_millis()),
        ));
    }

    /// Background job bookkeeping.
    pub fn job_started(&mut self, buffer: BufferId, kind: JobKind, job: JobId) {
        self.pending_jobs.insert((buffer, kind), job);
    }

    pub fn job_finished(&mut self, buffer: BufferId, kind: JobKind) {
        self.pending_jobs.remove(&(buffer, kind));
    }

    /// Report a background task that failed.
    pub fn report_failure(&mut self, event: AsyncEvent) -> Vec<Intent> {
        match event {
            AsyncEvent::Failed { what, error } => {
                vec![Intent::Message(
                    StatusLevel::Error,
                    format!("{what} failed: {error}"),
                )]
            }
            other => vec![Intent::Message(
                StatusLevel::Warn,
                format!("unexpected background event: {other:?}"),
            )],
        }
    }

    // ------------------------------------------------------------- status

    pub fn mode_label(&self) -> String {
        self.mode.label()
    }

    pub fn pending_echo(&self) -> String {
        self.pending.echo()
    }

    pub fn search_pattern(&self) -> Option<&str> {
        self.search.as_ref().map(|query| query.pattern.as_str())
    }

    /// The register prefix plus pending keys, for the status echo area.
    pub fn echo(&self) -> String {
        let mut echo = self.pending_echo();
        if echo.is_empty() {
            if let Some(register) = self.pending.register {
                echo = format!("\"{register}");
            }
        }
        echo
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

/// Geometry of the last rendered frame, remembered for command-time splits.
#[derive(Default, Debug)]
struct AreaCell(std::cell::Cell<Rect>);

impl Editor {
    /// Terminal area of the last frame (`Rect::ZERO` before the first draw).
    pub fn frame_area(&self) -> Rect {
        self.last_area.0.get()
    }
}

impl Editor {}

fn should_push_jump(motion: Motion, from_line: usize, to_line: usize) -> bool {
    use Motion::*;
    match motion {
        DocumentStart
        | DocumentEnd
        | SearchNext
        | SearchPrev
        | MatchBracket
        | ParagraphNext
        | ParagraphPrev
        | Column
        | FindChar(_)
        | TextObject { .. } => true,
        PageForward | PageBackward | HalfPageForward | HalfPageBackward | Up | Down => true,
        WordStart | WordEnd | WordBack | WordEndBack | BigWordStart | BigWordEnd | BigWordBack
        | BigWordEndBack => from_line.abs_diff(to_line) >= 5,
        _ => false,
    }
}

/// Normal mode cannot sit past the last character of a line.
fn clamp_cursor(buffer: &Buffer, position: usize) -> usize {
    let position = buffer.clamp_byte(position);
    let line_end = buffer.line_end(buffer.line_of(position));
    position.min(line_end)
}

fn clamped_to_buffer(buffer: Option<&Buffer>, selection: Selection) -> Selection {
    let Some(buffer) = buffer else {
        return selection;
    };
    let max = buffer.len_bytes();
    let mut mapped = Selection::new(
        selection
            .spans()
            .iter()
            .map(|span| span.clamp(max))
            .collect(),
        selection.primary_index(),
        selection.dir(),
    );
    if mapped.spans().is_empty() {
        mapped.collapse(0);
    }
    mapped
}

fn next_boundary(buffer: &Buffer, byte: usize) -> usize {
    next_char_boundary(&buffer.rope, byte)
}

fn chunk_message(verb: &str, text: &str, linewise: bool) -> String {
    if linewise || text.ends_with('\n') {
        format!("{verb} {} line(s)", text.lines().count())
    } else {
        format!("{verb} {} char(s)", text.chars().count())
    }
}

fn format_undo(verb: &str, remaining: usize, redoable: usize) -> String {
    format!("{verb} ({remaining} left, {redoable} to redo)")
}

/// The word used when an operator finds nothing to work on.
fn verb_for(operator: Operator) -> &'static str {
    match operator {
        Operator::Delete => "delete",
        Operator::Change => "change",
        Operator::Yank => "yank",
        Operator::Replace => "replace",
        Operator::Join => "join",
        Operator::Indent | Operator::Unindent | Operator::Reformat => "indent",
        Operator::SwapCase | Operator::Upper | Operator::Lower => "re-case",
        Operator::ToggleComment => "comment",
    }
}

pub fn split_mode_label(mode: SplitMode) -> &'static str {
    match mode {
        SplitMode::Equal => "equal",
        SplitMode::Golden => "golden",
    }
}

/// A key press that means "insert this text" in insert mode.
/// The text a key press types in insert mode, if it types anything.
///
/// SHIFT is normal: it is how capitals and `( { !` reach us. Only Ctrl and Alt
/// mean "this is a command, not text".
fn insertable(key: KeyEvent) -> Option<String> {
    keys::typed_char(key).map(|c| c.to_string())
}

/// The closing partner of an opening bracket or quote, when inserting one is
/// wanted at `cursor`.
///
/// A quote only pairs when it is not already inside a string or a comment: the
/// grammar knows, and the text-only fallback stays quiet rather than guessing.
fn matching_pair(buffer: Option<&Buffer>, cursor: usize, open: char) -> Option<char> {
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '"' => '"',
        '\'' => '\'',
        '`' => '`',
        _ => return None,
    };
    if matches!(open, '"' | '\'' | '`') {
        // A quote in the middle of a word is an apostrophe, not a string.
        let buffer = buffer?;
        if let Some((_, before)) = buffer.char_before(cursor)
            && (before.is_alphanumeric() || before == '_')
        {
            return None;
        }
        if crate::syntax::in_string_or_comment(buffer, cursor) {
            return None;
        }
    }
    Some(close)
}

/// Is `character` the closing half of a bracket pair?
fn is_closing(character: char) -> bool {
    matches!(character, ')' | ']' | '}')
}

// --------------------------------------------------------------------- facades
//
// Small wrappers the app layer uses so it never has to build an `Action` by hand
// or reach into private editing internals.

impl Editor {
    /// Run one command as if it had been bound to a key.
    pub fn run_cmd(&mut self, command: Cmd, area: Rect) -> Vec<Intent> {
        self.set_last_area(area);
        self.dispatch(Action::Cmd(command), &[], area)
    }

    /// `"Undo (3 left, 1 to redo)"`, counting the steps still on the stacks.
    fn undo_message(&self, verb: &str, id: BufferId) -> String {
        match self.buffer(id) {
            Some(buffer) => format_undo(verb, buffer.undo_depth(), buffer.redo_depth()),
            None => verb.to_string(),
        }
    }

    /// Flip an option (`:set number`, a bound toggle key, …).
    pub fn toggle(&mut self, option: ToggleOption) -> Vec<Intent> {
        self.toggle_option(option)
    }

    /// Write the focused buffer to disk.
    pub fn save_now(&mut self) -> Option<Intent> {
        self.save(None)
    }

    /// Re-read the focused buffer from disk (`:edit`).
    pub fn reload_from_disk(&mut self, force: bool) -> Option<Intent> {
        self.reload_file(force)
    }

    /// Insert pasted text as a single undo step.
    ///
    /// A block pasted at the start of an indented line is shifted to that
    /// line's depth, so code copied from column 0 lands inside the function it
    /// is pasted into instead of at the left margin.  Pasting mid-line is left
    /// alone: there is no block to align, and adding indentation there would
    /// corrupt the line.
    pub fn paste_text(&mut self, text: &str) -> Vec<Intent> {
        if text.is_empty() {
            return Vec::new();
        }
        if self.mode != Mode::Insert {
            self.enter_insert();
        }
        let text = self.reindent_for_paste(text);
        self.insert_text(&text);
        vec![Intent::EnsureVisible]
    }

    /// Shift `text` to the caret line's indentation, when that makes sense.
    fn reindent_for_paste(&self, text: &str) -> String {
        if !self.config.editor.auto_indent {
            return text.to_string();
        }
        let Some(buffer) = self.current_buffer() else {
            return text.to_string();
        };
        let cursor = self.cursor();
        let line = buffer.line_of(cursor);
        let line_start = buffer.line_start(line);
        // Only at the start of a line: mid-line pastes have no block to align.
        let before = buffer.slice(Span::new(line_start, cursor));
        if !before.chars().all(|c| c == ' ' || c == '\t') {
            return text.to_string();
        }
        let target = buffer.indent_str(line);
        crate::indent::reindent_paste(text, &target)
    }

    /// Every buffer, for `:buffers` and the picker.
    pub fn buffer_labels(&self) -> Vec<(BufferId, String, bool)> {
        self.buffers
            .iter()
            .map(|buffer| (buffer.id, buffer.display_name(), buffer.is_dirty()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vim-style starter configuration `vortex --write-config` ships.
    ///
    /// Vortex binds nothing itself, so a test that types `w` or `d` has to ask
    /// for those keys the way any user does — reading the shipped starter is also
    /// the check that it really does bind what the tests (and people) expect.
    fn starter() -> Config {
        let (config, warnings) = Config::parse(Config::default_config_text())
            .expect("the shipped starter configuration parses");
        assert!(warnings.is_empty(), "starter config warnings: {warnings:?}");
        config
    }

    /// A user binding that extends a starter key must not disable it.
    fn fixture_with_wd(text: &str) -> Fixture {
        let mut config = starter();
        config
            .keymap
            .entry("normal".to_string())
            .or_default()
            .insert("wd".to_string(), "close-pane".to_string());
        Fixture::with_config(text, config)
    }

    #[test]
    fn a_key_that_starts_a_longer_binding_still_runs() {
        // With `wd = close-pane` bound, `w` is a binding *and* a prefix: the
        // `x` cannot extend `w`, so `w` runs and `x` is read again.
        let mut fixture = fixture_with_wd("alpha beta\n");
        fixture.feed("w");
        fixture.feed("x");
        assert_eq!(
            fixture.editor.current_buffer().unwrap().text(),
            // `w` lands on the `b` of beta, `x` deletes that `b`.
            "alpha eta\n",
            "w then x should move then delete, warning: {:?}",
            fixture.editor.echo()
        );
    }

    #[test]
    fn a_quiet_key_sequence_runs_what_is_bound() {
        // `w` alone waits for the longer binding, then vim's `timeoutlen` lets it
        // through.
        let mut fixture = fixture_with_wd("alpha beta\n");
        fixture.feed("w");
        assert_eq!(
            fixture.editor.current_buffer().unwrap().text(),
            "alpha beta\n",
            "still waiting for a longer binding"
        );
        // Backdate the sequence instead of sleeping for the real timeout.
        fixture.editor.pending.keys_at = Some(
            std::time::Instant::now()
                - crate::input::PENDING_TIMEOUT
                - std::time::Duration::from_millis(5),
        );
        let area = fixture.area;
        fixture.editor.expire_pending(area);
        assert_eq!(
            fixture.editor.line_at(fixture.editor.cursor()),
            0,
            "the timed-out `w` should have moved the cursor"
        );
        assert_eq!(fixture.editor.cursor(), 6, "onto `b` of beta");
    }

    #[test]
    fn a_longer_binding_still_wins_and_a_lone_operator_waits() {
        // `wd` must close the pane rather than run `w` then `d`.
        let mut fixture = fixture_with_wd("alpha\nbeta\n");
        fixture.editor.run_cmd(Cmd::SplitHorizontal, fixture.area);
        assert_eq!(fixture.editor.layout.panes().len(), 2);
        fixture.feed("wd");
        assert_eq!(fixture.editor.layout.panes().len(), 1, "wd closed the pane");

        // A pending operator is not a key sequence: `d` waits for its motion
        // however long the user takes.
        fixture.feed("d");
        let area = fixture.area;
        fixture.editor.pending.keys_at = Some(
            std::time::Instant::now()
                - crate::input::PENDING_TIMEOUT
                - std::time::Duration::from_millis(5),
        );
        fixture.editor.expire_pending(area);
        assert!(
            fixture.editor.pending.operator.is_some(),
            "the operator must survive the timeout"
        );
        assert_eq!(
            fixture.editor.current_buffer().unwrap().text(),
            "alpha\nbeta\n",
            "nothing deleted yet"
        );
    }

    #[test]
    fn x_deletes_one_multibyte_character() {
        // A multi-byte character is one character: `x` must not nibble bytes.
        let mut fixture = Fixture::new("alpha … beta 漢字 gamma\n");
        fixture.feed("0wx");
        assert_eq!(
            fixture.editor.current_buffer().unwrap().text(),
            "alpha  beta 漢字 gamma\n"
        );
        fixture.feed("u");
        assert_eq!(
            fixture.editor.current_buffer().unwrap().text(),
            "alpha … beta 漢字 gamma\n"
        );
    }

    /// An editor with one in-memory buffer shown in a pane, driven by keys the
    /// way the app drives it.
    struct Fixture {
        editor: Editor,
        keymaps: Keymaps,
        area: Rect,
    }

    impl Fixture {
        fn new(text: &str) -> Self {
            Self::with_config(text, starter())
        }

        fn with_config(text: &str, config: Config) -> Self {
            let (keymaps, warnings) = Keymaps::build(&config);
            assert!(warnings.is_empty(), "broken keymap: {warnings:?}");
            let mut editor = Editor::new(config);
            let id = editor.alloc_buffer_id();
            editor.buffers.push(Buffer::new(
                id,
                None,
                text.to_string(),
                &editor.config.editor,
            ));
            editor.show_in_pane(id, None);
            Self {
                editor,
                keymaps,
                area: Rect::new(0, 0, 80, 24),
            }
        }

        /// Feed a key notation such as `gg` or `ciw`.
        fn feed(&mut self, notation: &str) -> Vec<Intent> {
            let keys = keys::parse_key_sequence(notation)
                .unwrap_or_else(|| panic!("\"{notation}\" is not a key sequence"));
            let mut intents = Vec::new();
            for key in keys {
                intents.extend(self.editor.handle_key(key, &self.keymaps, self.area));
            }
            intents
        }

        fn text(&self) -> String {
            self.editor.current_buffer().expect("a buffer").text()
        }

        fn cursor(&self) -> usize {
            self.editor
                .current_pane()
                .expect("a pane")
                .selection
                .cursor()
        }

        fn warn(messages: &[Intent]) -> String {
            messages
                .iter()
                .filter_map(|intent| match intent {
                    Intent::Message(StatusLevel::Warn, text) => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
    }

    #[test]
    fn insert_types_capitals_punctuation_and_digits() {
        let mut fixture = Fixture::new("hello\n");
        fixture.feed("iZZ!(1)");
        assert_eq!(fixture.text(), "ZZ!(1)hello\n");
        assert!(Fixture::warn(&fixture.feed("i")).is_empty());
    }

    #[test]
    fn enter_after_an_opening_brace_indents_the_new_line() {
        let mut fixture = Fixture::new("fn main() {}\n");
        fixture.feed("f{");
        fixture.feed("a");
        fixture.feed("enter");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    \n}\n",
            "the new line is indented and the brace keeps its own line"
        );
        assert_eq!(caret_at(&fixture), (1, 4), "the caret is between them");
    }

    #[test]
    fn a_closing_brace_typed_alone_dedents_to_the_block() {
        // The caret sits at the end of an over-indented blank line inside the
        // block; typing `}` should line it up with the block, not with the
        // stray indentation.
        let mut fixture = Fixture::new("fn main() {\n    let a = 1;\n        ");
        fixture.feed("jjA");
        fixture.feed("}");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    let a = 1;\n}",
            "the brace lines up with the block, not with the stray indentation"
        );
    }

    #[test]
    fn an_opening_bracket_inserts_its_partner() {
        let mut fixture = Fixture::new("\n");
        fixture.feed("i");
        fixture.feed("(");
        assert_eq!(fixture.text(), "()\n");
        assert_eq!(fixture.cursor(), 1, "the caret sits between the pair");
        // Typing the closing half steps over the partner instead of doubling it.
        fixture.feed(")");
        assert_eq!(fixture.text(), "()\n");
        assert_eq!(fixture.cursor(), 2);
    }

    #[test]
    fn a_quote_inside_a_word_is_not_paired() {
        // `don't` must not become `don''t`.
        let mut fixture = Fixture::new("\n");
        fixture.feed("idon");
        fixture.feed("'");
        assert_eq!(fixture.text(), "don'\n");
    }

    #[test]
    fn auto_pairs_can_be_turned_off() {
        let mut config = starter();
        config.editor.auto_match_pairs = false;
        let mut fixture = Fixture::with_config("\n", config);
        fixture.feed("i(");
        assert_eq!(fixture.text(), "(\n");
    }

    #[test]
    fn escape_returns_to_normal_mode() {
        let mut fixture = Fixture::new("one\n");
        fixture.feed("iabc");
        assert_eq!(fixture.editor.mode, Mode::Insert);
        fixture.feed("esc");
        assert_eq!(fixture.editor.mode, Mode::Normal);
        // The cursor sits on the last character typed, vim-style.
        assert_eq!(fixture.cursor(), 2);
    }

    #[test]
    fn operators_work_on_motions_and_text_objects() {
        let mut fixture = Fixture::new("alpha bravo\ncharlie\n");
        fixture.feed("ggdw");
        assert_eq!(fixture.text(), "bravo\ncharlie\n");

        let mut fixture = Fixture::new("alpha bravo\n");
        fixture.feed("gglciwzulu");
        assert_eq!(fixture.text(), "zulu bravo\n");

        let mut fixture = Fixture::new("alpha\nbravo\ncharlie\n");
        fixture.feed("gg2dd");
        assert_eq!(fixture.text(), "charlie\n");
    }

    #[test]
    fn counts_apply_to_operators() {
        let mut fixture = Fixture::new("abcdef\n");
        fixture.feed("gg3x");
        assert_eq!(fixture.text(), "def\n");
    }

    #[test]
    fn visual_line_deletes_a_range() {
        let mut fixture = Fixture::new("one\ntwo\nthree\n");
        fixture.feed("ggVjd");
        assert_eq!(fixture.text(), "three\n");
        assert_eq!(fixture.editor.mode, Mode::Normal);
    }

    #[test]
    fn undo_and_redo_round_trip_a_change() {
        let mut fixture = Fixture::new("keep\n");
        fixture.feed("ideleted\nesc");
        let edited = fixture.text();
        assert!(edited.starts_with("deleted"));
        fixture.feed("u");
        assert_eq!(fixture.text(), "keep\n");
        fixture.feed("ctrl-r");
        assert_eq!(fixture.text(), edited);
    }

    #[test]
    fn dollar_stays_on_the_last_character() {
        let mut fixture = Fixture::new("abc\nde\n");
        fixture.feed("g");
        fixture.feed("g");
        fixture.feed("$");
        assert_eq!(fixture.cursor(), 2);
        fixture.feed("j");
        fixture.feed("$");
        // `abc\nde`: the `e` of the second line.
        assert_eq!(fixture.cursor(), 5);
    }

    #[test]
    fn unknown_keys_warn_instead_of_panic() {
        let mut fixture = Fixture::new("x\n");
        let warnings = Fixture::warn(&fixture.feed("<F9>"));
        assert!(warnings.contains("no binding"), "got {warnings:?}");
    }

    #[test]
    fn the_leader_and_colon_open_prompts() {
        let mut fixture = Fixture::new("x\n");
        let intents = fixture.feed("space f");
        assert!(
            intents
                .iter()
                .any(|intent| matches!(intent, Intent::OpenPrompt(_))),
            "expected a prompt: {intents:?}"
        );
    }

    #[test]
    fn a_bound_ex_command_runs_instead_of_opening_the_prompt() {
        // `"gb" = "bnext"` must switch buffers on the key press, not leave a
        // pre-filled command line waiting for Enter.
        let mut config = Config::default();
        config
            .keymap
            .entry("normal".to_string())
            .or_default()
            .insert("g b".to_string(), "bnext".to_string());
        let mut fixture = Fixture::with_config("x\n", config);
        let intents = fixture.feed("g b");
        assert!(
            intents.contains(&Intent::RunCommand("bnext".to_string())),
            "expected the command to run: {intents:?}"
        );
    }

    /// Where the caret sits, the way the screen shows it.
    fn caret_at(fixture: &Fixture) -> (usize, usize) {
        let buffer = fixture.editor.current_buffer().expect("a buffer");
        let at = fixture.cursor();
        let line = buffer.line_of(at);
        (line, at - buffer.line_start(line))
    }

    /// A buffer that looks like Rust to the language detection, so the default
    /// `rustfmt` entry resolves without touching the disk or `$PATH`.
    fn as_rust(fixture: &mut Fixture) {
        fixture.editor.buffers[0].set_path(std::path::PathBuf::from("probe.rs"));
    }

    #[test]
    fn o_opens_a_line_below_and_types_into_it() {
        let mut fixture = Fixture::new("fn main() {\n    let a = 1;\n}\n");
        fixture.feed("jo");
        assert_eq!(fixture.editor.mode, Mode::Insert);
        assert_eq!(
            fixture.text(),
            "fn main() {\n    let a = 1;\n    \n}\n",
            "the new line takes the indentation; the closing brace stays put"
        );
        assert_eq!(caret_at(&fixture), (2, 4), "the caret is on the new line");
        // What is typed next belongs to that line, not to the one below it.
        fixture.feed("x");
        assert_eq!(fixture.text(), "fn main() {\n    let a = 1;\n    x\n}\n");
    }

    #[test]
    fn o_at_the_end_of_the_text_opens_the_last_line() {
        // No trailing newline: the current line has to be ended first.
        let mut fixture = Fixture::new("alpha\nbeta");
        fixture.feed("jo");
        assert_eq!(fixture.text(), "alpha\nbeta\n");
        assert_eq!(
            caret_at(&fixture),
            (2, 0),
            "on the opened line, not past it"
        );

        // A trailing newline already ends the last line.
        let mut fixture = Fixture::new("one\ntwo\n");
        fixture.feed("jo");
        assert_eq!(fixture.text(), "one\ntwo\n\n");
        assert_eq!(caret_at(&fixture), (2, 0));
    }

    #[test]
    fn capital_o_opens_a_line_above() {
        let mut fixture = Fixture::new("fn main() {\n    let a = 1;\n}\n");
        fixture.feed("jO");
        assert_eq!(fixture.text(), "fn main() {\n    \n    let a = 1;\n}\n");
        assert_eq!(caret_at(&fixture), (1, 4));
    }

    /// Give the fixture's buffer a diagnostic at each of `lines` (0-based).
    fn mark_lines(fixture: &mut Fixture, lines: &[usize]) {
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        mark_lines_in(fixture, id, lines);
    }

    /// `mark_lines` for a buffer that is not the focused one.
    fn mark_lines_in(fixture: &mut Fixture, id: BufferId, lines: &[usize]) {
        let diagnostics = {
            let buffer = fixture.editor.buffer(id).expect("a buffer");
            lines
                .iter()
                .map(|line| {
                    let at = buffer.line_start(*line);
                    Diagnostic::new(
                        crate::selection::Span::new(at, at + 1),
                        crate::diagnostic::Severity::Error,
                        format!("problem on line {line}"),
                        DiagnosticSource::Lsp,
                    )
                })
                .collect()
        };
        fixture
            .editor
            .set_diagnostics(id, DiagnosticSource::Lsp, diagnostics);
    }

    /// Add an in-memory buffer without showing it, the way the LSP client
    /// leaves buffers it has reported on but the user never opened.
    fn add_scratch(fixture: &mut Fixture, text: &str) -> BufferId {
        let id = fixture.editor.alloc_buffer_id();
        fixture.editor.buffers.push(Buffer::new(
            id,
            None,
            text.to_string(),
            &fixture.editor.config.editor,
        ));
        id
    }

    #[test]
    fn bracket_d_jumps_to_the_next_diagnostic_and_wraps() {
        let mut fixture = Fixture::new("one\ntwo\nthree\nfour\n");
        mark_lines(&mut fixture, &[1, 3]);
        fixture.feed("]d");
        assert_eq!(caret_at(&fixture).0, 1, "first jump lands on line 1");
        fixture.feed("]d");
        assert_eq!(caret_at(&fixture).0, 3, "second jump lands on line 3");
        fixture.feed("]d");
        assert_eq!(caret_at(&fixture).0, 1, "past the last one wraps around");
    }

    #[test]
    fn bracket_d_back_jumps_to_the_previous_diagnostic_and_wraps() {
        let mut fixture = Fixture::new("one\ntwo\nthree\nfour\n");
        mark_lines(&mut fixture, &[1, 3]);
        fixture.feed("G");
        // `G` lands on line 3, which is itself flagged: `[d` steps back to the
        // previous diagnostic rather than staying put.
        fixture.feed("[d");
        assert_eq!(caret_at(&fixture).0, 1, "back from the end finds line 1");
        fixture.feed("[d");
        assert_eq!(caret_at(&fixture).0, 3, "before the first one wraps");
    }

    #[test]
    fn bracket_d_with_no_diagnostics_says_so() {
        let mut fixture = Fixture::new("clean\n");
        let intents = fixture.feed("]d");
        assert!(
            intents.iter().any(|intent| matches!(
                intent,
                Intent::Message(StatusLevel::Info, text) if text.contains("no diagnostics")
            )),
            "expected a message, got {intents:?}"
        );
    }

    #[test]
    fn project_d_jumps_walk_into_the_next_buffer_and_wrap() {
        let mut fixture = Fixture::new("one\ntwo\nthree\n");
        let first = fixture.editor.current_buffer_id().expect("a buffer");
        mark_lines(&mut fixture, &[0, 2]);
        let second = add_scratch(&mut fixture, "alpha\nbeta\n");
        mark_lines_in(&mut fixture, second, &[1]);

        let jump = |fixture: &mut Fixture| {
            fixture
                .editor
                .run_cmd(Cmd::DiagnosticNext(DiagnosticScope::Project), fixture.area)
        };
        jump(&mut fixture);
        assert_eq!(
            caret_at(&fixture).0,
            2,
            "the caret sits on the line-0 target already, so the first jump moves on"
        );
        assert_eq!(fixture.editor.current_buffer_id(), Some(first));
        jump(&mut fixture);
        assert_eq!(
            fixture.editor.current_buffer_id(),
            Some(second),
            "past the last target in this buffer, the walk crosses into the next"
        );
        assert_eq!(caret_at(&fixture).0, 1);
        jump(&mut fixture);
        assert_eq!(
            fixture.editor.current_buffer_id(),
            Some(first),
            "past the last target in the project, it wraps"
        );
        assert_eq!(caret_at(&fixture).0, 0);
    }

    #[test]
    fn project_d_back_walks_into_the_previous_buffer() {
        let mut fixture = Fixture::new("one\ntwo\nthree\n");
        let first = fixture.editor.current_buffer_id().expect("a buffer");
        mark_lines(&mut fixture, &[2]);
        let second = add_scratch(&mut fixture, "alpha\nbeta\n");
        mark_lines_in(&mut fixture, second, &[0]);
        // The caret sits at the top of the first buffer: `[d` wraps to the
        // last target in the project, which lives in the other buffer.
        let intents = fixture
            .editor
            .run_cmd(Cmd::DiagnosticPrev(DiagnosticScope::Project), fixture.area);
        assert_eq!(fixture.editor.current_buffer_id(), Some(second));
        assert_eq!(caret_at(&fixture).0, 0);
        assert!(
            intents.iter().any(|intent| matches!(
                intent,
                Intent::Message(StatusLevel::Info, text) if text.contains("problem on line 0")
            )),
            "the message names the target: {intents:?}"
        );
        fixture
            .editor
            .run_cmd(Cmd::DiagnosticPrev(DiagnosticScope::Project), fixture.area);
        assert_eq!(fixture.editor.current_buffer_id(), Some(first));
        assert_eq!(caret_at(&fixture).0, 2);
    }

    #[test]
    fn buffer_d_jumps_ignore_other_buffers() {
        let mut fixture = Fixture::new("one\ntwo\n");
        let first = fixture.editor.current_buffer_id().expect("a buffer");
        mark_lines(&mut fixture, &[0]);
        let second = add_scratch(&mut fixture, "alpha\n");
        mark_lines_in(&mut fixture, second, &[0]);
        fixture.feed("]d");
        assert_eq!(
            fixture.editor.current_buffer_id(),
            Some(first),
            "buffer scope never leaves the current buffer"
        );
    }

    #[test]
    fn project_d_with_no_diagnostics_says_so() {
        let mut fixture = Fixture::new("clean\n");
        add_scratch(&mut fixture, "also clean\n");
        let intents = fixture
            .editor
            .run_cmd(Cmd::DiagnosticNext(DiagnosticScope::Project), fixture.area);
        assert!(
            intents.iter().any(|intent| matches!(
                intent,
                Intent::Message(StatusLevel::Info, text)
                    if text.contains("no diagnostics in the project")
            )),
            "expected a message, got {intents:?}"
        );
    }

    #[test]
    fn replacing_a_source_does_not_accumulate_diagnostics() {
        // `set_diagnostics` replaces one source's set.  If the items were stored
        // under a source the caller never passes back, the retain matched nothing
        // and every update appended: errors piled up as the user typed and only
        // went away when a differently-sourced set happened to wipe the buffer.
        let mut fixture = Fixture::new("one\ntwo\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        let at = fixture.editor.buffer(id).expect("a buffer").line_start(0);

        for count in 1..=5 {
            let pulled: Vec<_> = (0..count)
                .map(|line| {
                    Diagnostic::new(
                        crate::selection::Span::new(at + line, at + line + 1),
                        crate::diagnostic::Severity::Error,
                        format!("problem {line}"),
                        DiagnosticSource::LspPull,
                    )
                })
                .collect();
            fixture
                .editor
                .set_diagnostics(id, DiagnosticSource::LspPull, pulled);
            let stored = fixture
                .editor
                .buffer(id)
                .expect("a buffer")
                .diagnostics
                .iter()
                .filter(|d| d.source == DiagnosticSource::LspPull)
                .count();
            assert_eq!(
                stored, count,
                "each pull must replace the last, not stack on it"
            );
        }
    }

    #[test]
    fn project_d_jumps_into_a_stored_workspace_diagnostic() {
        // A workspace file with no open buffer: its diagnostics live in the
        // path-keyed store.  `]d` must hand them to the app layer as an
        // OpenDiagnosticFile intent rather than dropping them.
        let mut fixture = Fixture::new("one\ntwo\n");
        mark_lines(&mut fixture, &[0]);
        let path = std::path::PathBuf::from("/tmp/project/lib.rs");
        let entry = ProjectDiagnostic {
            path: path.clone(),
            offset: 4,
            line: 1,
            column: 2,
            diagnostic: Diagnostic::new(
                crate::selection::Span::point(4),
                crate::diagnostic::Severity::Error,
                "problem in lib.rs",
                DiagnosticSource::Lsp,
            ),
        };
        fixture
            .editor
            .set_project_diagnostics(path.clone(), DiagnosticSource::Lsp, vec![entry]);
        let intents = fixture
            .editor
            .run_cmd(Cmd::DiagnosticNext(DiagnosticScope::Project), fixture.area);
        assert!(
            intents.iter().any(|intent| matches!(
                intent,
                Intent::OpenDiagnosticFile { path: p, offset }
                    if *p == path && *offset == 4
            )),
            "expected an OpenDiagnosticFile intent for the stored file: {intents:?}"
        );
    }

    #[test]
    fn set_project_diagnostics_replaces_its_source() {
        // Like buffers, a repeated store of one source must replace rather than
        // accumulate; otherwise every publish would stack on the last.
        let mut fixture = Fixture::new("x\n");
        let path = std::path::PathBuf::from("/tmp/project/a.rs");
        for count in 1..=3 {
            let entries: Vec<_> = (0..count)
                .map(|i| ProjectDiagnostic {
                    path: path.clone(),
                    offset: i,
                    line: i,
                    column: 0,
                    diagnostic: Diagnostic::new(
                        crate::selection::Span::point(i),
                        crate::diagnostic::Severity::Error,
                        format!("problem {i}"),
                        DiagnosticSource::Lsp,
                    ),
                })
                .collect();
            fixture
                .editor
                .set_project_diagnostics(path.clone(), DiagnosticSource::Lsp, entries);
            let stored = fixture
                .editor
                .project_diagnostics(&path)
                .unwrap_or_default()
                .len();
            assert_eq!(stored, count, "each store must replace the last");
        }
    }

    #[test]
    fn take_project_diagnostics_moves_them_out_of_the_store() {
        // Opening a file that was only registered (never opened) must move its
        // stored diagnostics onto the buffer; rust-analyzer will not republish
        // unchanged content on a re-open, so leaving them in the store would
        // orphan them and show nothing until an edit.
        let mut fixture = Fixture::new("x\n");
        let path = std::path::PathBuf::from("/tmp/project/a.rs");
        fixture.editor.set_project_diagnostics(
            path.clone(),
            DiagnosticSource::Lsp,
            vec![ProjectDiagnostic {
                path: path.clone(),
                offset: 0,
                line: 1,
                column: 2,
                diagnostic: Diagnostic::new(
                    crate::selection::Span::point(4),
                    crate::diagnostic::Severity::Error,
                    "problem",
                    DiagnosticSource::Lsp,
                ),
            }],
        );
        let taken = fixture.editor.take_project_diagnostics(&path);
        assert_eq!(taken.len(), 1, "the stored set is returned");
        assert!(
            fixture.editor.project_diagnostics(&path).is_none(),
            "the store entry is gone once moved"
        );
    }

    #[test]
    fn o_on_a_line_that_opens_a_block_indents_inside_it() {
        // `o` on `fn main() {` must land inside the block, not at column 0.
        let mut fixture = Fixture::new("fn main() {\n}\n");
        fixture.feed("o");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    \n}\n",
            "the opened line is indented one level"
        );
        assert_eq!(caret_at(&fixture), (1, 4));
    }

    #[test]
    fn typing_on_an_opened_line_keeps_its_indentation() {
        // `o` on `if true {` opens an indented line.  Typing the first
        // character must not dedent it back to the `if`'s column: only a
        // closing bracket is allowed to move the line.
        let mut fixture = Fixture::new("if true {\n}\n");
        fixture.feed("o");
        assert_eq!(caret_at(&fixture), (1, 4), "opened inside the block");
        fixture.feed("x");
        assert_eq!(
            fixture.text(),
            "if true {\n    x\n}\n",
            "typing keeps the indentation the opened line had"
        );
        assert_eq!(caret_at(&fixture), (1, 5));
    }

    #[test]
    fn typing_on_an_opened_line_keeps_its_indentation_when_parsed() {
        // The real session has a parsed buffer, and the parse lands while the
        // user pauses after `o`.  That is the case the tree-sitter dedent path
        // sees, and the one the unparsed fixture above cannot reach.
        let mut fixture = Fixture::new("if true {\n}\n");
        as_rust(&mut fixture);
        fixture.editor.buffers[0].reparse_now();
        fixture.feed("o");
        // The async parse completes before the next keystroke.
        fixture.editor.buffers[0].reparse_now();
        assert_eq!(caret_at(&fixture), (1, 4), "opened inside the block");
        fixture.feed("x");
        assert_eq!(
            fixture.text(),
            "if true {\n    x\n}\n",
            "typing keeps the indentation the opened line had"
        );
        assert_eq!(caret_at(&fixture), (1, 5));
    }

    #[test]
    fn o_on_a_nested_block_indents_one_level_deeper() {
        let mut fixture = Fixture::new("fn main() {\n    if x {\n    }\n}\n");
        fixture.feed("jo");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    if x {\n        \n    }\n}\n"
        );
        assert_eq!(caret_at(&fixture), (2, 8));
    }

    #[test]
    fn capital_o_on_a_block_opener_stays_outside_the_block() {
        // `O` opens the line *above*, which is outside the block the line
        // opens, so it keeps the line's own indentation.
        let mut fixture = Fixture::new("fn main() {\n}\n");
        fixture.feed("O");
        assert_eq!(fixture.text(), "\nfn main() {\n}\n");
        assert_eq!(caret_at(&fixture), (0, 0));
    }

    #[test]
    fn o_on_a_plain_line_keeps_its_indentation() {
        let mut fixture = Fixture::new("fn main() {\n    let a = 1;\n}\n");
        fixture.feed("jo");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    let a = 1;\n    \n}\n",
            "a line that opens nothing does not indent further"
        );
    }

    #[test]
    fn o_in_visual_mode_swaps_ends_and_never_opens_a_line() {
        // Visual `o` is vim's swap of the cursor and the far end of the
        // selection.  The open-a-line form of `o` reaching a selection is what
        // mangled the text before, so pin the binding down.
        let mut fixture = Fixture::new("one\ntwo\nthree\n");
        fixture.feed("jjvk");
        let before = fixture.cursor();
        fixture.feed("o");
        assert_eq!(fixture.editor.mode, Mode::Visual(VisualKind::Char));
        assert_eq!(fixture.text(), "one\ntwo\nthree\n", "`o` edits nothing");
        assert_ne!(fixture.cursor(), before, "the caret swapped ends");
    }

    #[test]
    fn o_holds_the_idle_format_until_insert_mode_ends() {
        let mut fixture = Fixture::new("fn main() {\n    let a = 1;\n}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.feed("jo");
        assert!(
            fixture.editor.format_due.is_none(),
            "`o` must not queue a reformat that lands under the caret"
        );
        // What the event loop does when it notices the buffer changed.
        fixture.editor.schedule_auto_format(id);
        assert_eq!(
            fixture.editor.format_held,
            Some(id),
            "the format is waiting, not forgotten"
        );
        assert!(
            fixture.editor.format_due.is_none(),
            "still nothing armed while the caret is typing"
        );
        fixture.feed("esc");
        assert!(
            fixture.editor.format_due.is_some(),
            "hands off the keyboard: now it may run"
        );
    }

    #[test]
    fn undo_walks_past_an_idle_format_to_the_last_edit() {
        let mut fixture = Fixture::new("fn main() {\nlet a=1;\n}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.feed("jjo");
        fixture.feed("x");
        fixture.feed("esc");
        // What the background formatter does when its run finishes.
        fixture
            .editor
            .apply_format(id, "fn main() {\n    let a = 1;\n    x\n}\n")
            .expect("the formatter output applies");
        assert_eq!(fixture.editor.current_buffer().unwrap().undo_depth(), 2);

        fixture.feed("u");
        assert_eq!(
            fixture.text(),
            "fn main() {\nlet a=1;\n}\n\n",
            "one `u` undoes the typed line, not the formatting"
        );
        fixture.feed("u");
        assert_eq!(fixture.text(), "fn main() {\nlet a=1;\n}\n");

        // Two presses undid two edits, so two put them back — and the format
        // rides along with the edit it followed rather than being its own step.
        fixture.feed("ctrl-r");
        assert_eq!(fixture.text(), "fn main() {\nlet a=1;\n}\n\n");
        fixture.feed("ctrl-r");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    let a = 1;\n    x\n}\n",
            "the edit and its format come back together"
        );
    }

    #[test]
    fn an_explicit_format_is_its_own_undo_step() {
        let mut fixture = Fixture::new("let a=1;\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        // The idle timer: not a step of its own, just part of the edit it followed.
        fixture
            .editor
            .apply_format(id, "let a = 1;\n")
            .expect("the formatter output applies");
        assert_eq!(
            fixture.editor.current_buffer().unwrap().undo_depth(),
            0,
            "an automatic run is not a change the user made"
        );

        // `:format`: asked for by name, so `u` takes the formatting back and
        // leaves the text alone.
        fixture.editor.format_requested = Some(id);
        fixture
            .editor
            .apply_format(id, "let a = 1;\n\n")
            .expect("the formatter output applies");
        assert_eq!(
            fixture.editor.current_buffer().unwrap().undo_depth(),
            1,
            "a requested run is a step like any other"
        );
        fixture.feed("u");
        assert_eq!(fixture.text(), "let a = 1;\n");
    }

    #[test]
    fn a_format_lands_once_when_two_panes_show_the_buffer() {
        // The same buffer visible in a split: the formatter's change set must
        // touch the text once, not once per pane.
        let mut fixture =
            Fixture::new("fn main() {\n    if x {\n    println!(\"test\");\n    }\n}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.run_cmd(Cmd::SplitHorizontal, fixture.area);
        assert_eq!(fixture.editor.layout.panes_showing(id).len(), 2);
        fixture
            .editor
            .apply_format(
                id,
                "fn main() {\n    if x {\n        println!(\"test\");\n    }\n}\n",
            )
            .expect("the formatter output applies");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    if x {\n        println!(\"test\");\n    }\n}\n"
        );
    }

    #[test]
    fn a_format_keeps_the_caret_on_the_text_it_was_on() {
        // The formatter grew the block above the caret.  Restoring the caret by
        // the line number it happened to be on left it on whatever line moved up
        // into its place, which is the "cursor resets at random" jump.
        let mut fixture = Fixture::new("fn main() {\nlet a=1;\n}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.feed("G");
        let text = fixture.text();
        let before = fixture.cursor();
        assert_eq!(
            &text[before..before + 1],
            "}",
            "the caret starts on the closing brace"
        );

        fixture
            .editor
            .apply_format(id, "fn main() {\n    let a = 1;\n    let b = 2;\n}\n")
            .expect("the formatter output applies");

        let text = fixture.text();
        let after = fixture.cursor();
        assert_eq!(
            &text[after..after + 1],
            "}",
            "still the closing brace, not the line that slid up under it"
        );
        assert_eq!(caret_at(&fixture), (3, 0));
    }

    #[test]
    fn a_format_keeps_each_panes_own_caret() {
        // Two panes on one buffer: the caret that gets restored was the one in
        // the pane being edited.  Taking it from whichever pane the layout lists
        // first teleported the focused pane's caret to the other pane's place.
        let mut fixture = Fixture::new("fn main() {\nlet a=1;\n}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.run_cmd(Cmd::SplitHorizontal, fixture.area);
        let panes = fixture.editor.layout.panes_showing(id);
        assert_eq!(panes.len(), 2);
        let focused = fixture.editor.layout.focused().expect("a focused pane");
        let other = panes
            .iter()
            .copied()
            .find(|pane| *pane != focused)
            .expect("2 panes");

        fixture.feed("G");
        let text = fixture.text();
        let on_brace = fixture.cursor();
        assert_eq!(&text[on_brace..on_brace + 1], "}");
        if let Some(pane) = fixture.editor.layout.pane_mut(other) {
            pane.selection = Selection::point(0);
        }

        fixture
            .editor
            .apply_format(id, "fn main() {\n    let a = 1;\n    let b = 2;\n}\n")
            .expect("the formatter output applies");

        let text = fixture.text();
        assert_eq!(
            &text[fixture.cursor()..fixture.cursor() + 1],
            "}",
            "the focused pane followed its own caret, not the other one's"
        );
        assert_ne!(
            fixture.cursor(),
            0,
            "it did not land where the other pane was"
        );
        assert_ne!(fixture.cursor(), on_brace, "and it travelled with the text");
        assert_eq!(
            fixture
                .editor
                .layout
                .pane(other)
                .expect("the other pane")
                .selection
                .cursor(),
            0,
            "the unfocused pane keeps its own caret"
        );
    }

    #[test]
    fn the_idle_format_names_the_buffer_that_armed_it() {
        // The timer outlives a buffer switch: it formats the buffer that was
        // edited, not whichever one is on screen when it fires.
        let mut fixture = Fixture::new("fn main() {\nlet a=1;\n}\n");
        as_rust(&mut fixture);
        let edited = fixture.editor.current_buffer_id().expect("a buffer");
        let other = fixture.editor.new_scratch();
        fixture.editor.show_in_pane(other, None);
        assert_eq!(fixture.editor.current_buffer_id(), Some(other));

        fixture.editor.schedule_auto_format(edited);
        assert_eq!(
            fixture.editor.format_due.map(|(buffer, _)| buffer),
            Some(edited),
            "the deadline belongs to the buffer that was edited"
        );
    }

    #[test]
    fn yank_and_paste_round_trip() {
        // `yy` is linewise, so `p` drops the copy on the next line.
        let mut fixture = Fixture::new("copy me\n");
        fixture.feed("ggyyp");
        assert_eq!(fixture.text(), "copy me\ncopy me\n");

        // `l` moves over exactly one character, so `yl` yanks one, and `p`
        // splices it in after the cursor.
        let mut fixture = Fixture::new("copy me\n");
        fixture.feed("ggyllp");
        assert_eq!(fixture.text(), "cocpy me\n");
    }

    #[test]
    fn a_paste_at_the_start_of_an_indented_line_takes_that_indentation() {
        // The caret sits at the start of an indented line inside a block; a
        // block copied from column 0 lands at the block's depth.  The pasted
        // text ends with a newline, so the caret line's own newline survives as
        // a blank line — the same as pasting in vim.
        let mut fixture = Fixture::new("fn main() {\n    \n}\n");
        fixture.feed("jA");
        fixture
            .editor
            .paste_text("let a = 1;\nif a {\n    b();\n}\n");
        assert_eq!(
            fixture.text(),
            "fn main() {\n    let a = 1;\n    if a {\n        b();\n    }\n\n}\n"
        );
    }

    #[test]
    fn a_paste_mid_line_is_not_reindented() {
        // There is no block to align, and adding indentation would corrupt the
        // line the caret is in.
        let mut fixture = Fixture::new("    let a = 1;\n");
        fixture.feed("A");
        fixture.editor.paste_text(" + 2");
        assert_eq!(fixture.text(), "    let a = 1; + 2\n");
    }

    // ------------------------------------------------------------ lsp edits

    fn edit(range: std::ops::Range<usize>, text: &str) -> crate::lsp::TextEdit {
        crate::lsp::TextEdit {
            range: crate::selection::Span::new(range.start, range.end),
            new_text: text.to_string(),
        }
    }

    #[test]
    fn server_edits_apply_back_to_front() {
        // Two edits on one line: applying them in order would shift the second
        // span by the length of the first replacement.
        let mut fixture = Fixture::new("fn  main( ) {}\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        let applied = fixture.editor.apply_edits(
            id,
            &[edit(2..4, " "), edit(9..10, "")],
            ChangeGroup::Format,
        );
        assert_eq!(applied, 2);
        assert_eq!(fixture.text(), "fn main() {}\n");
    }

    #[test]
    fn an_empty_edit_list_changes_nothing() {
        let mut fixture = Fixture::new("let a = 1;\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        assert_eq!(fixture.editor.apply_edits(id, &[], ChangeGroup::Format), 0);
        assert_eq!(fixture.text(), "let a = 1;\n");
    }

    #[test]
    fn a_read_only_buffer_refuses_server_edits() {
        let mut fixture = Fixture::new("let a = 1;\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.buffer_mut(id).expect("a buffer").read_only = true;
        assert_eq!(
            fixture
                .editor
                .apply_edits(id, &[edit(0..3, "const")], ChangeGroup::Format),
            0
        );
        assert_eq!(fixture.text(), "let a = 1;\n");
    }

    #[test]
    fn a_quick_fix_is_one_undo_step() {
        // A code action is a change the user chose, so one `u` takes it back.
        let mut fixture = Fixture::new("let unused = 1;\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture
            .editor
            .apply_edits(id, &[edit(0..15, "")], ChangeGroup::Command);
        assert_eq!(fixture.text(), "\n");
        fixture.feed("u");
        assert_eq!(fixture.text(), "let unused = 1;\n");
    }

    #[test]
    fn a_server_edit_keeps_every_panes_caret() {
        // Two panes on one buffer: a server edit has to move both carets the
        // way a formatter run does.  Restoring only the focused pane left the
        // other on a raw edit mapping, which lands on the wrong line as soon as
        // the edit changes the line count above it.
        let mut fixture = Fixture::new("fn  main() {}\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.set_last_area(fixture.area);
        fixture
            .editor
            .show_in_pane(id, Some(Axis::Column))
            .expect("a split");
        let panes = fixture.editor.layout.panes_showing(id);
        assert_eq!(panes.len(), 2, "the buffer is in two panes");
        let unfocused = panes
            .iter()
            .copied()
            .find(|pane| !fixture.editor.layout.is_focused(*pane))
            .expect("an unfocused pane");
        let end = fixture.editor.buffer(id).expect("a buffer").line_end(0);
        fixture
            .editor
            .layout
            .pane_mut(unfocused)
            .expect("a pane")
            .selection = Selection::point(end);

        // Collapse the double space; the caret was at the end of the line and
        // must still be at the end of the line.
        fixture
            .editor
            .apply_edits(id, &[edit(2..4, " ")], ChangeGroup::Format);
        assert_eq!(fixture.text(), "fn main() {}\n");
        let caret = fixture
            .editor
            .layout
            .pane(unfocused)
            .expect("a pane")
            .selection
            .cursor();
        let buffer = fixture.editor.buffer(id).expect("a buffer");
        assert_eq!(buffer.line_of(caret), 0, "still on the only line");
        assert_eq!(caret, buffer.line_end(0), "still at the end of the line");
    }

    #[test]
    fn a_buffer_is_found_by_its_absolute_path_too() {
        // A buffer opened from a relative command-line argument stores a
        // relative path, but a language server echoes back an absolute
        // `file://` URI.  Comparing the two directly never matched, so every
        // diagnostic for such a file was dropped as "no such buffer".
        let mut fixture = Fixture::new("fn main() {}\n");
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.buffers[0].set_path(std::path::PathBuf::from("src/probe.rs"));

        // The stored (relative) path still matches exactly.
        assert_eq!(
            fixture.editor.buffer_by_path(Path::new("src/probe.rs")),
            Some(id)
        );
        // And the absolute form the server reports resolves to the same buffer.
        let absolute = std::path::absolute("src/probe.rs").expect("absolute");
        assert_eq!(fixture.editor.buffer_by_path(&absolute), Some(id));
        // A different file is still not found.
        assert_eq!(
            fixture.editor.buffer_by_path(Path::new("src/other.rs")),
            None
        );
    }

    // ------------------------------------------------------------- highlighting

    /// A slot holding a tree for `text`, the way the blocking parse task hands
    /// one over.
    fn slot_for(text: &str) -> crate::syntax::TreeSlot {
        let language = crate::lang::by_id("rust")
            .and_then(|lang| lang.grammar())
            .expect("the Rust grammar is bundled");
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).expect("Rust grammar loads");
        let slot = crate::syntax::TreeSlot::new();
        slot.store(
            parser
                .parse(text, None)
                .expect("the Rust grammar parses this text"),
        );
        slot
    }

    /// Type a literal string, one key at a time, in whatever mode is current.
    fn type_text(fixture: &mut Fixture, text: &str) {
        for ch in text.chars() {
            let mut buf = [0u8; 4];
            let token = match ch {
                ' ' => "space".to_string(),
                other => other.encode_utf8(&mut buf).to_string(),
            };
            fixture.feed(&token);
        }
    }

    /// Say a parse job is in flight for `id`, the way `App::spawn_parse` does
    /// before it hands the text to the blocking pool.
    fn start_parse_job(editor: &mut Editor, id: BufferId) {
        editor.buffer_mut(id).unwrap().syntax.status = crate::syntax::ParseStatus::Pending;
    }

    #[test]
    fn a_stale_parse_result_leaves_the_buffer_parseable() {
        // Typing while a parse runs makes its result stale.  Dropping that
        // result used to leave the buffer still saying a parse was in flight,
        // so nothing ever parsed it again: highlighting froze from that
        // keystroke on for the rest of the session.
        let mut fixture = Fixture::new("fn a() {}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.buffer_mut(id).unwrap().reparse_now();

        // The job starts on the text as it stands, then the user types.
        start_parse_job(&mut fixture.editor, id);
        let stale_version = fixture.editor.buffer(id).unwrap().version;
        let slot = slot_for("fn a() {}\n");
        fixture.feed("o");
        type_text(&mut fixture, "// a note");
        fixture.feed("esc");
        let version = fixture.editor.buffer(id).unwrap().version;
        assert_ne!(version, stale_version, "typing bumped the version");

        assert!(
            !fixture.editor.adopt_tree(id, stale_version, slot),
            "the result for the older text is dropped"
        );
        let buffer = fixture.editor.buffer(id).unwrap();
        assert!(
            buffer.syntax.parse_due(version),
            "the job is over, so the idle timer has to owe this buffer a parse: {:?}",
            buffer.syntax
        );

        // The next parse lands, and the text typed since the dropped result is
        // highlighted with it.
        let typed = fixture.editor.buffer(id).unwrap().text();
        let slot = slot_for(&typed);
        assert!(
            fixture.editor.adopt_tree(id, version, slot),
            "a result for the current version is adopted"
        );
        let buffer = fixture.editor.buffer(id).unwrap();
        assert!(
            buffer.syntax.is_current(version),
            "{:?}",
            buffer.syntax.status
        );
        assert!(!buffer.syntax.parse_due(version), "nothing left to do");
        let groups = crate::syntax::highlights(
            buffer,
            crate::selection::Span::new(0, buffer.len_bytes()),
            buffer.language.id,
        );
        assert!(
            groups
                .iter()
                .any(|group| group.hl == crate::syntax::Hl::Comment
                    && typed[group.span.start..group.span.end].contains("a note")),
            "the typed comment should be highlighted: {groups:?}"
        );
    }

    #[test]
    fn a_failed_parse_waits_for_an_edit_instead_of_retrying_forever() {
        // A job that comes back with no tree has still finished: the buffer
        // must not stay `Pending`, and it must not be re-parsed on every idle
        // tick either — only the next edit makes the text interesting again.
        let mut fixture = Fixture::new("fn a() {}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.buffer_mut(id).unwrap().reparse_now();
        start_parse_job(&mut fixture.editor, id);
        let version = fixture.editor.buffer(id).unwrap().version;

        assert!(
            !fixture
                .editor
                .adopt_tree(id, version, crate::syntax::TreeSlot::new()),
            "an empty slot is not a tree"
        );
        let buffer = fixture.editor.buffer(id).unwrap();
        assert_eq!(buffer.syntax.status, crate::syntax::ParseStatus::Failed);
        assert!(
            !buffer.syntax.parse_due(version),
            "this version is known not to parse; retrying it every tick buys nothing"
        );

        fixture.feed("ix");
        let buffer = fixture.editor.buffer(id).unwrap();
        assert!(
            buffer.syntax.parse_due(buffer.version),
            "the edit makes the buffer worth another parse"
        );
    }

    #[test]
    fn invalidating_a_tree_asks_for_a_parse_without_waiting_on_one() {
        // `:edit` and `:source` invalidate the tree and then ask for a parse.
        // A status that read "a job is already running" turned that request
        // into a no-op, and the buffer kept its stale colours for good.
        let mut fixture = Fixture::new("fn a() {}\n");
        as_rust(&mut fixture);
        let id = fixture.editor.current_buffer_id().expect("a buffer");
        fixture.editor.buffer_mut(id).unwrap().reparse_now();
        fixture.editor.buffer_mut(id).unwrap().syntax.invalidate();
        let buffer = fixture.editor.buffer(id).unwrap();
        assert!(
            buffer.syntax.parse_due(buffer.version),
            "invalidating is a request for a parse, not a job in flight: {:?}",
            buffer.syntax.status
        );
    }
}
