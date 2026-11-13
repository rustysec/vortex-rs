//! The action vocabulary: motions, operators and commands.
//!
//! Every key binding and every `:` command resolves to an [`Action`].  Action
//! names are also the vocabulary used by the TOML keymap, so this module is the
//! single source of truth for binding names (see [`action_names`] / [`parse_action`]).

use std::str::FromStr;

use crate::diagnostic::DiagnosticScope;

/// How a motion walks the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Motion {
    // --- basic movement ---
    Left,
    Right,
    Up,
    Down,
    /// `0`
    LineStart,
    /// `^`
    FirstNonBlank,
    /// `$`
    LineEnd,
    /// `_`
    LineEndFirstNonBlank,
    /// `|` — column given by the count
    Column,
    // --- word motions ---
    WordStart,
    WordEnd,
    WordBack,
    WordEndBack,
    BigWordStart,
    BigWordEnd,
    BigWordBack,
    BigWordEndBack,
    // --- character find (the character is taken from pending input) ---
    FindChar(FindDir),
    RepeatFind,
    RepeatFindReverse,
    // --- structure ---
    DocumentStart,
    DocumentEnd,
    ViewTop,
    ViewMiddle,
    ViewBottom,
    PageForward,
    PageBackward,
    HalfPageForward,
    HalfPageBackward,
    ParagraphNext,
    ParagraphPrev,
    SentenceNext,
    SentencePrev,
    MatchBracket,
    /// The character under the cursor (used by `x`, `s`, `~`).
    CharAhead,
    /// From the cursor to the end of the line (used by `D`, `C`).
    TillLineEnd,
    /// `/` / `?` — open the search prompt.
    SearchNext,
    SearchPrev,
    /// `i` / `a` — the text-object key follows.
    TextObject {
        inside: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindDir {
    Forward,
    TillForward,
    Backward,
    TillBackward,
}

/// Operators combine with a motion or text object (`d` + `w`, `c` + `iw`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operator {
    Delete,
    Change,
    Yank,
    /// `r` — the replacement character is taken from pending input.
    Replace,
    /// `J`
    Join,
    /// `>`
    Indent,
    /// `<`
    Unindent,
    /// `=` — reindent using the formatter when one is configured.
    Reformat,
    /// `~`
    SwapCase,
    /// `U`
    Upper,
    /// `u`
    Lower,
    /// `gc` — toggle line comments.
    ToggleComment,
}

/// Commands are self contained actions that do not need a motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    // --- mode switching ---
    Insert,
    InsertAtWordEnd,
    Append,
    AppendLineEnd,
    InsertLineStart,
    AppendLineStart,
    OpenBelow,
    OpenAbove,
    Normal,
    Visual(VisualKind),
    Cancel,
    ExitInsert,

    // --- editing ---
    DeleteCharNext,
    DeleteCharPrev,
    DeleteSelection,
    PutAfter,
    PutBefore,
    PutAfterLine,
    PutBeforeLine,
    ReplaceChar,
    Undo,
    Redo,
    Repeat,
    SelectRegister,
    SetMark,
    GotoMarkChar,
    GotoMarkLine,
    /// `g;` — go back to the last change, in insert mode.
    InsertPrevious,
    /// `gv` — swap the visual selection's anchor and cursor.
    SwapAnchor,
    DuplicateLine,
    RemoveEmptyLines,

    // --- files ---
    Save,
    SaveAs,
    SaveAll,
    Quit,
    ForceQuit,
    QuitAll,
    ForceQuitAll,
    ReloadFile,

    // --- prompts / search ---
    CommandMode,
    SearchForward,
    SearchBackward,
    FindWordForward,
    FindWordBackward,
    ClearSearchHighlight,
    FindFiles,
    GrepProject,
    GotoLine,
    SwitchBuffer,
    CommandPalette,

    // --- layout ---
    SplitVertical,
    SplitHorizontal,
    SplitAuto,
    OnlyPane,
    ZoomPane,
    EqualizePanes,
    CycleLayout(SplitMode),
    FocusNext,
    FocusPrev,
    FocusDir(Direction),
    MoveDir(Direction),
    ResizeDir(Direction),
    ClosePane,

    // --- viewport ---
    Recenter,
    ScrollToTop,
    ScrollToBottom,
    ScrollLine(Direction),
    JumpBack,
    JumpForward,

    // --- insert mode ---
    InsertNewline,
    InsertTab,
    InsertBacktab,
    InsertBackspace,
    InsertDelete,
    InsertDeleteWord,
    InsertDeleteLineStart,
    InsertIndentLine,
    /// One normal-mode action, then straight back to insert (`ctrl-o`).
    NormalOnce,

    // --- misc ---
    ToggleOption(ToggleOption),
    SpellSuggest,
    /// `:diagnostic list` — the diagnostics picker over the given scope.
    ShowDiagnostics(DiagnosticScope),
    /// Jump to the next (`]d`) or previous (`[d`) diagnostic in the scope.
    DiagnosticNext(DiagnosticScope),
    DiagnosticPrev(DiagnosticScope),
    /// Ask the language server what is at the caret (`K`).
    Hover,
    /// Ask the language server for quick fixes at the caret.
    CodeAction,
    FormatBuffer,
    /// Run an ex command, used by keybindings such as `"bn" = "bnext"`.
    Ex(String),
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

impl VisualKind {
    pub fn label(self) -> &'static str {
        match self {
            VisualKind::Char => "VISUAL",
            VisualKind::Line => "V-LINE",
            VisualKind::Block => "V-BLOCK",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn is_horizontal(self) -> bool {
        matches!(self, Direction::Left | Direction::Right)
    }

    /// Kebab-case suffix used by the bindable action names
    /// (`focus-left`, `resize-up`, …).
    pub fn label(self) -> &'static str {
        match self {
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }

    pub fn sign(self) -> i32 {
        match self {
            Direction::Left | Direction::Up => -1,
            Direction::Right | Direction::Down => 1,
        }
    }
}

/// How panes are sized after splits. Mirrors the `[layout] split-mode`
/// setting.  The split *direction* is chosen per command (`split-horizontal`,
/// `split-vertical`, `split-auto`), not by the mode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum SplitMode {
    /// Splits keep even shares (`split-ratio`); nothing re-balances on focus.
    #[default]
    Equal,
    /// Resize every split so the focused pane gets the golden ratio.
    Golden,
}

impl std::str::FromStr for SplitMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "equal" => Ok(SplitMode::Equal),
            "golden" => Ok(SplitMode::Golden),
            "explicit" | "auto" => Err(format!(
                "split mode \"{value}\" was removed; use \"equal\" (direction is a \
                 per-split choice now) or \"golden\""
            )),
            other => Err(format!("unknown split mode \"{other}\" (equal, golden)")),
        }
    }
}

impl SplitMode {
    pub fn name(self) -> &'static str {
        match self {
            SplitMode::Equal => "equal",
            SplitMode::Golden => "golden",
        }
    }
}

/// Options that can be flipped with a key or `:set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToggleOption {
    LineNumbers,
    Mouse,
    AutoFormat,
    Wrap,
    SpellCheck,
    CursorLine,
}

impl ToggleOption {
    pub fn key(self) -> &'static str {
        match self {
            ToggleOption::LineNumbers => "editor.line-number",
            ToggleOption::Mouse => "editor.mouse",
            ToggleOption::AutoFormat => "format.on-idle",
            ToggleOption::Wrap => "editor.wrap",
            ToggleOption::SpellCheck => "spell.enable",
            ToggleOption::CursorLine => "editor.cursorline",
        }
    }
}

/// A resolved key binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Move the cursor.
    Motion(Motion),
    /// Move the cursor and grow the selection (visual mode).
    Extend(Motion),
    /// Start an operator; a motion or text object must follow.
    Operator(Operator),
    /// An operator applied to a fixed motion (`x`, `D`, `~`).
    OperatorMotion(Operator, Motion),
    /// An operator applied to whole lines (`dd`, `S`, `>>`).
    OperatorLine(Operator),
    Cmd(Cmd),
}

impl Action {
    pub const fn motion(m: Motion) -> Self {
        Action::Motion(m)
    }
}

impl From<Motion> for Action {
    fn from(m: Motion) -> Self {
        Action::Motion(m)
    }
}

impl From<Operator> for Action {
    fn from(o: Operator) -> Self {
        Action::Operator(o)
    }
}

impl From<Cmd> for Action {
    fn from(c: Cmd) -> Self {
        Action::Cmd(c)
    }
}

/// `name, action, help` — the registry used by the keymap, `:help keys` and tests.
pub const ACTIONS: &[(&str, Action, &str)] = &[
    // ---- motions ----
    ("move-left", Action::motion(Motion::Left), "move left"),
    ("move-right", Action::motion(Motion::Right), "move right"),
    ("move-up", Action::motion(Motion::Up), "move up"),
    ("move-down", Action::motion(Motion::Down), "move down"),
    (
        "line-start",
        Action::motion(Motion::LineStart),
        "first column",
    ),
    (
        "first-non-blank",
        Action::motion(Motion::FirstNonBlank),
        "first non-blank of line",
    ),
    ("line-end", Action::motion(Motion::LineEnd), "end of line"),
    (
        "line-end-non-blank",
        Action::motion(Motion::LineEndFirstNonBlank),
        "last non-blank of line",
    ),
    ("column", Action::motion(Motion::Column), "go to column N"),
    (
        "word-forward",
        Action::motion(Motion::WordStart),
        "next word start",
    ),
    ("word-end", Action::motion(Motion::WordEnd), "next word end"),
    (
        "word-back",
        Action::motion(Motion::WordBack),
        "previous word start",
    ),
    (
        "word-end-back",
        Action::motion(Motion::WordEndBack),
        "previous word end",
    ),
    (
        "big-word-forward",
        Action::motion(Motion::BigWordStart),
        "next WORD start",
    ),
    (
        "big-word-end",
        Action::motion(Motion::BigWordEnd),
        "next WORD end",
    ),
    (
        "big-word-back",
        Action::motion(Motion::BigWordBack),
        "previous WORD start",
    ),
    (
        "big-word-end-back",
        Action::motion(Motion::BigWordEndBack),
        "previous WORD end",
    ),
    (
        "find-char",
        Action::motion(Motion::FindChar(FindDir::Forward)),
        "find char forward",
    ),
    (
        "find-char-back",
        Action::motion(Motion::FindChar(FindDir::Backward)),
        "find char backward",
    ),
    (
        "till-char",
        Action::motion(Motion::FindChar(FindDir::TillForward)),
        "move before char",
    ),
    (
        "till-char-back",
        Action::motion(Motion::FindChar(FindDir::TillBackward)),
        "move after char",
    ),
    (
        "repeat-find",
        Action::motion(Motion::RepeatFind),
        "repeat f/F",
    ),
    (
        "repeat-find-reverse",
        Action::motion(Motion::RepeatFindReverse),
        "repeat f/F reversed",
    ),
    (
        "document-start",
        Action::motion(Motion::DocumentStart),
        "start of file",
    ),
    (
        "document-end",
        Action::motion(Motion::DocumentEnd),
        "end of file",
    ),
    (
        "view-top",
        Action::motion(Motion::ViewTop),
        "top of viewport",
    ),
    (
        "view-middle",
        Action::motion(Motion::ViewMiddle),
        "middle of viewport",
    ),
    (
        "view-bottom",
        Action::motion(Motion::ViewBottom),
        "bottom of viewport",
    ),
    (
        "page-down",
        Action::motion(Motion::PageForward),
        "page down",
    ),
    ("page-up", Action::motion(Motion::PageBackward), "page up"),
    (
        "half-page-down",
        Action::motion(Motion::HalfPageForward),
        "half page down",
    ),
    (
        "half-page-up",
        Action::motion(Motion::HalfPageBackward),
        "half page up",
    ),
    (
        "paragraph-next",
        Action::motion(Motion::ParagraphNext),
        "next paragraph",
    ),
    (
        "paragraph-prev",
        Action::motion(Motion::ParagraphPrev),
        "previous paragraph",
    ),
    (
        "sentence-next",
        Action::motion(Motion::SentenceNext),
        "next sentence",
    ),
    (
        "sentence-prev",
        Action::motion(Motion::SentencePrev),
        "previous sentence",
    ),
    (
        "match-bracket",
        Action::motion(Motion::MatchBracket),
        "matching bracket",
    ),
    (
        "search-next",
        Action::motion(Motion::SearchNext),
        "next search match",
    ),
    (
        "search-prev",
        Action::motion(Motion::SearchPrev),
        "previous search match",
    ),
    (
        "current-char",
        Action::motion(Motion::CharAhead),
        "the character under the cursor",
    ),
    (
        "till-line-end",
        Action::motion(Motion::TillLineEnd),
        "from the cursor to the end of the line",
    ),
    (
        "inside",
        Action::motion(Motion::TextObject { inside: true }),
        "inside text object",
    ),
    (
        "around",
        Action::motion(Motion::TextObject { inside: false }),
        "around text object",
    ),
    // ---- operators ----
    ("delete", Action::Operator(Operator::Delete), "delete"),
    ("change", Action::Operator(Operator::Change), "change"),
    ("yank", Action::Operator(Operator::Yank), "yank"),
    (
        "replace",
        Action::Operator(Operator::Replace),
        "replace char",
    ),
    ("join", Action::Operator(Operator::Join), "join lines"),
    ("indent", Action::Operator(Operator::Indent), "indent"),
    ("unindent", Action::Operator(Operator::Unindent), "unindent"),
    (
        "reindent",
        Action::Operator(Operator::Reformat),
        "reindent / format",
    ),
    (
        "swap-case",
        Action::Operator(Operator::SwapCase),
        "swap case",
    ),
    ("uppercase", Action::Operator(Operator::Upper), "uppercase"),
    ("lowercase", Action::Operator(Operator::Lower), "lowercase"),
    (
        "toggle-comment",
        Action::Operator(Operator::ToggleComment),
        "toggle comment",
    ),
    // ---- insert / modes ----
    ("insert", Action::Cmd(Cmd::Insert), "insert before cursor"),
    (
        "insert-word-end",
        Action::Cmd(Cmd::InsertAtWordEnd),
        "insert at end of word",
    ),
    ("append", Action::Cmd(Cmd::Append), "append after cursor"),
    (
        "append-line-end",
        Action::Cmd(Cmd::AppendLineEnd),
        "append at end of line",
    ),
    (
        "insert-line-start",
        Action::Cmd(Cmd::InsertLineStart),
        "insert at first non-blank",
    ),
    (
        "append-line-start",
        Action::Cmd(Cmd::AppendLineStart),
        "append at first non-blank",
    ),
    ("open-below", Action::Cmd(Cmd::OpenBelow), "open line below"),
    ("open-above", Action::Cmd(Cmd::OpenAbove), "open line above"),
    ("normal", Action::Cmd(Cmd::Normal), "switch to normal mode"),
    (
        "exit-insert",
        Action::Cmd(Cmd::ExitInsert),
        "leave insert mode",
    ),
    (
        "visual",
        Action::Cmd(Cmd::Visual(VisualKind::Char)),
        "visual mode",
    ),
    (
        "visual-line",
        Action::Cmd(Cmd::Visual(VisualKind::Line)),
        "visual line mode",
    ),
    (
        "visual-block",
        Action::Cmd(Cmd::Visual(VisualKind::Block)),
        "visual block mode",
    ),
    (
        "cancel",
        Action::Cmd(Cmd::Cancel),
        "cancel pending keys / close prompt",
    ),
    // ---- editing ----
    (
        "delete-char",
        Action::OperatorMotion(Operator::Delete, Motion::CharAhead),
        "delete char under cursor",
    ),
    (
        "delete-to-line-end",
        Action::OperatorMotion(Operator::Delete, Motion::TillLineEnd),
        "delete to end of line",
    ),
    (
        "delete-line",
        Action::OperatorLine(Operator::Delete),
        "delete whole line",
    ),
    (
        "change-char",
        Action::OperatorMotion(Operator::Change, Motion::CharAhead),
        "replace char under cursor",
    ),
    (
        "change-to-line-end",
        Action::OperatorMotion(Operator::Change, Motion::TillLineEnd),
        "change to end of line",
    ),
    (
        "change-line",
        Action::OperatorLine(Operator::Change),
        "change whole line",
    ),
    (
        "yank-line",
        Action::OperatorLine(Operator::Yank),
        "yank whole line",
    ),
    (
        "swap-case-char",
        Action::OperatorMotion(Operator::SwapCase, Motion::CharAhead),
        "swap case of one char",
    ),
    (
        "delete-char-back",
        Action::Cmd(Cmd::DeleteCharPrev),
        "delete char before cursor",
    ),
    (
        "delete-selection",
        Action::Cmd(Cmd::DeleteSelection),
        "delete visual selection",
    ),
    (
        "paste-after",
        Action::Cmd(Cmd::PutAfter),
        "paste after cursor",
    ),
    (
        "paste-before",
        Action::Cmd(Cmd::PutBefore),
        "paste before cursor",
    ),
    (
        "paste-line-after",
        Action::Cmd(Cmd::PutAfterLine),
        "paste as lines below",
    ),
    (
        "paste-line-before",
        Action::Cmd(Cmd::PutBeforeLine),
        "paste as lines above",
    ),
    ("undo", Action::Cmd(Cmd::Undo), "undo"),
    ("redo", Action::Cmd(Cmd::Redo), "redo"),
    ("repeat", Action::Cmd(Cmd::Repeat), "repeat last change"),
    (
        "select-register",
        Action::Cmd(Cmd::SelectRegister),
        "select register",
    ),
    ("set-mark", Action::Cmd(Cmd::SetMark), "set mark"),
    (
        "goto-mark-char",
        Action::Cmd(Cmd::GotoMarkChar),
        "jump to mark (exact column)",
    ),
    (
        "goto-mark-line",
        Action::Cmd(Cmd::GotoMarkLine),
        "jump to mark (first non-blank)",
    ),
    (
        "duplicate-line",
        Action::Cmd(Cmd::DuplicateLine),
        "duplicate line(s)",
    ),
    (
        "swap-anchor",
        Action::Cmd(Cmd::SwapAnchor),
        "swap cursor and selection anchor",
    ),
    (
        "insert-previous",
        Action::Cmd(Cmd::InsertPrevious),
        "re-enter insert where you left insert",
    ),
    (
        "remove-empty-lines",
        Action::Cmd(Cmd::RemoveEmptyLines),
        "delete empty lines in selection",
    ),
    // ---- files ----
    ("save", Action::Cmd(Cmd::Save), "write file"),
    ("save-as", Action::Cmd(Cmd::SaveAs), "write file as…"),
    ("save-all", Action::Cmd(Cmd::SaveAll), "write all buffers"),
    ("quit", Action::Cmd(Cmd::Quit), "close pane / quit"),
    (
        "force-quit",
        Action::Cmd(Cmd::ForceQuit),
        "quit without saving",
    ),
    (
        "quit-all",
        Action::Cmd(Cmd::QuitAll),
        "quit, refusing unsaved changes",
    ),
    (
        "force-quit-all",
        Action::Cmd(Cmd::ForceQuitAll),
        "quit, discarding unsaved changes",
    ),
    (
        "reload-file",
        Action::Cmd(Cmd::ReloadFile),
        "re-read file from disk",
    ),
    // ---- prompts ----
    (
        "command-mode",
        Action::Cmd(Cmd::CommandMode),
        "command mode",
    ),
    (
        "search-forward",
        Action::Cmd(Cmd::SearchForward),
        "search forward prompt",
    ),
    (
        "search-backward",
        Action::Cmd(Cmd::SearchBackward),
        "search backward prompt",
    ),
    (
        "find-word-forward",
        Action::Cmd(Cmd::FindWordForward),
        "search word under cursor",
    ),
    (
        "find-word-backward",
        Action::Cmd(Cmd::FindWordBackward),
        "search word under cursor backwards",
    ),
    (
        "clear-search-highlight",
        Action::Cmd(Cmd::ClearSearchHighlight),
        "hide search matches",
    ),
    ("find-files", Action::Cmd(Cmd::FindFiles), "find files"),
    (
        "grep-project",
        Action::Cmd(Cmd::GrepProject),
        "search project",
    ),
    ("goto-line", Action::Cmd(Cmd::GotoLine), "goto line"),
    (
        "switch-buffer",
        Action::Cmd(Cmd::SwitchBuffer),
        "switch buffer",
    ),
    (
        "command-palette",
        Action::Cmd(Cmd::CommandPalette),
        "command palette",
    ),
    // ---- layout ----
    (
        "split-vertical",
        Action::Cmd(Cmd::SplitVertical),
        "split side by side",
    ),
    (
        "split-horizontal",
        Action::Cmd(Cmd::SplitHorizontal),
        "split top / bottom",
    ),
    (
        "split-auto",
        Action::Cmd(Cmd::SplitAuto),
        "split along longest axis",
    ),
    ("only-pane", Action::Cmd(Cmd::OnlyPane), "zoom focused pane"),
    ("zoom-pane", Action::Cmd(Cmd::ZoomPane), "toggle pane zoom"),
    (
        "equalize-panes",
        Action::Cmd(Cmd::EqualizePanes),
        "make panes equal size",
    ),
    (
        "layout-equal",
        Action::Cmd(Cmd::CycleLayout(SplitMode::Equal)),
        "use even split sizing",
    ),
    (
        "layout-golden",
        Action::Cmd(Cmd::CycleLayout(SplitMode::Golden)),
        "use golden-ratio splits",
    ),
    ("focus-next", Action::Cmd(Cmd::FocusNext), "focus next pane"),
    (
        "focus-prev",
        Action::Cmd(Cmd::FocusPrev),
        "focus previous pane",
    ),
    (
        "focus-left",
        Action::Cmd(Cmd::FocusDir(Direction::Left)),
        "focus pane left",
    ),
    (
        "focus-right",
        Action::Cmd(Cmd::FocusDir(Direction::Right)),
        "focus pane right",
    ),
    (
        "focus-up",
        Action::Cmd(Cmd::FocusDir(Direction::Up)),
        "focus pane above",
    ),
    (
        "focus-down",
        Action::Cmd(Cmd::FocusDir(Direction::Down)),
        "focus pane below",
    ),
    (
        "buf-move-left",
        Action::Cmd(Cmd::MoveDir(Direction::Left)),
        "move pane left",
    ),
    (
        "buf-move-right",
        Action::Cmd(Cmd::MoveDir(Direction::Right)),
        "move pane right",
    ),
    (
        "buf-move-up",
        Action::Cmd(Cmd::MoveDir(Direction::Up)),
        "move pane up",
    ),
    (
        "buf-move-down",
        Action::Cmd(Cmd::MoveDir(Direction::Down)),
        "move pane down",
    ),
    (
        "resize-left",
        Action::Cmd(Cmd::ResizeDir(Direction::Left)),
        "shrink pane width",
    ),
    (
        "resize-right",
        Action::Cmd(Cmd::ResizeDir(Direction::Right)),
        "grow pane width",
    ),
    (
        "resize-up",
        Action::Cmd(Cmd::ResizeDir(Direction::Up)),
        "shrink pane height",
    ),
    (
        "resize-down",
        Action::Cmd(Cmd::ResizeDir(Direction::Down)),
        "grow pane height",
    ),
    (
        "close-pane",
        Action::Cmd(Cmd::ClosePane),
        "close focused pane",
    ),
    // ---- misc ----
    (
        "toggle-line-numbers",
        Action::Cmd(Cmd::ToggleOption(ToggleOption::LineNumbers)),
        "toggle line numbers",
    ),
    (
        "toggle-mouse",
        Action::Cmd(Cmd::ToggleOption(ToggleOption::Mouse)),
        "toggle mouse support",
    ),
    (
        "toggle-auto-format",
        Action::Cmd(Cmd::ToggleOption(ToggleOption::AutoFormat)),
        "toggle automatic formatting",
    ),
    (
        "toggle-wrap",
        Action::Cmd(Cmd::ToggleOption(ToggleOption::Wrap)),
        "toggle soft wrap",
    ),
    (
        "toggle-spell",
        Action::Cmd(Cmd::ToggleOption(ToggleOption::SpellCheck)),
        "toggle spell checking",
    ),
    (
        "toggle-cursorline",
        Action::Cmd(Cmd::ToggleOption(ToggleOption::CursorLine)),
        "toggle current line highlight",
    ),
    (
        "spell-suggest",
        Action::Cmd(Cmd::SpellSuggest),
        "spelling suggestions",
    ),
    (
        "show-diagnostics",
        Action::Cmd(Cmd::ShowDiagnostics(DiagnosticScope::Buffer)),
        "list diagnostics in this buffer",
    ),
    (
        "show-diagnostics-project",
        Action::Cmd(Cmd::ShowDiagnostics(DiagnosticScope::Project)),
        "list diagnostics in the project",
    ),
    (
        "diagnostic-next",
        Action::Cmd(Cmd::DiagnosticNext(DiagnosticScope::Buffer)),
        "jump to next diagnostic in this buffer",
    ),
    (
        "diagnostic-next-project",
        Action::Cmd(Cmd::DiagnosticNext(DiagnosticScope::Project)),
        "jump to next diagnostic in the project",
    ),
    (
        "diagnostic-prev",
        Action::Cmd(Cmd::DiagnosticPrev(DiagnosticScope::Buffer)),
        "jump to previous diagnostic in this buffer",
    ),
    (
        "diagnostic-prev-project",
        Action::Cmd(Cmd::DiagnosticPrev(DiagnosticScope::Project)),
        "jump to previous diagnostic in the project",
    ),
    (
        "hover",
        Action::Cmd(Cmd::Hover),
        "show what is under the cursor",
    ),
    (
        "code-action",
        Action::Cmd(Cmd::CodeAction),
        "quick fixes at the cursor",
    ),
    (
        "format-buffer",
        Action::Cmd(Cmd::FormatBuffer),
        "run the formatter now",
    ),
    // ---- viewport ----
    ("recenter", Action::Cmd(Cmd::Recenter), "center the cursor"),
    (
        "scroll-to-top",
        Action::Cmd(Cmd::ScrollToTop),
        "move cursor to top of viewport",
    ),
    (
        "scroll-to-bottom",
        Action::Cmd(Cmd::ScrollToBottom),
        "move cursor to bottom of viewport",
    ),
    (
        "scroll-line-up",
        Action::Cmd(Cmd::ScrollLine(Direction::Up)),
        "scroll up without moving the cursor",
    ),
    (
        "scroll-line-down",
        Action::Cmd(Cmd::ScrollLine(Direction::Down)),
        "scroll down without moving the cursor",
    ),
    ("jump-back", Action::Cmd(Cmd::JumpBack), "older jump"),
    ("jump-forward", Action::Cmd(Cmd::JumpForward), "newer jump"),
    // ---- insert mode ----
    ("insert-newline", Action::Cmd(Cmd::InsertNewline), "newline"),
    ("insert-tab", Action::Cmd(Cmd::InsertTab), "indent"),
    (
        "insert-backtab",
        Action::Cmd(Cmd::InsertBacktab),
        "unindent",
    ),
    (
        "insert-backspace",
        Action::Cmd(Cmd::InsertBackspace),
        "delete char before",
    ),
    (
        "insert-delete",
        Action::Cmd(Cmd::InsertDelete),
        "delete char under cursor",
    ),
    (
        "insert-delete-word",
        Action::Cmd(Cmd::InsertDeleteWord),
        "delete word before",
    ),
    (
        "insert-delete-line-start",
        Action::Cmd(Cmd::InsertDeleteLineStart),
        "delete to start of line",
    ),
    (
        "insert-indent-line",
        Action::Cmd(Cmd::InsertIndentLine),
        "indent the current line",
    ),
    (
        "normal-once",
        Action::Cmd(Cmd::NormalOnce),
        "one normal-mode action, then insert",
    ),
    ("noop", Action::Cmd(Cmd::Noop), "do nothing"),
];

/// Parse an action by its kebab-case name (`"word-forward"`, `"save"`, …).
///
/// Also accepts `":w"`-style strings and bare `:` command names (`"bnext"`,
/// `"bn"`), which run the command as an ex action.
pub fn parse_action(name: &str) -> Option<Action> {
    let name = name.trim();
    if name.is_empty() {
        return Some(Action::Cmd(Cmd::Noop));
    }
    if name == "none" || name == "noop" {
        return Some(Action::Cmd(Cmd::Noop));
    }
    if let Some(rest) = name.strip_prefix(':') {
        return Some(Action::Cmd(Cmd::Ex(rest.to_string())));
    }
    // `+` and `<` need names of their own because they are awkward in TOML keys.
    let alias = match name {
        "plus" => Some("indent"),
        "minus" => Some("unindent"),
        "equal" => Some("reindent"),
        "tilde" => Some("swap-case"),
        "gt" => Some("indent"),
        "lt" => Some("unindent"),
        _ => None,
    };
    let lookup = alias.unwrap_or(name);
    if let Some((_, action, _)) = ACTIONS.iter().find(|(n, _, _)| *n == lookup) {
        return Some(action.clone());
    }
    // `:` command names double as action names, so a keymap can bind `"bnext"`
    // exactly as it binds `":bnext"`.  Action names win on a clash (`quit`).
    if crate::command::is_command(lookup) {
        return Some(Action::Cmd(Cmd::Ex(lookup.to_string())));
    }
    None
}

/// All action names, for completion and documentation.
pub fn action_names() -> Vec<&'static str> {
    ACTIONS.iter().map(|(n, _, _)| *n).collect()
}

/// The kebab-case name of an action, for the `:map` listing.
pub fn name_for(action: &Action) -> Option<&'static str> {
    ACTIONS
        .iter()
        .find(|(_, candidate, _)| candidate == action)
        .map(|(name, _, _)| *name)
}

/// `name<TAB>description` lines for `:help keys`.
pub fn help_lines() -> Vec<String> {
    ACTIONS
        .iter()
        .map(|(_n, _, h)| *h)
        .zip(ACTIONS.iter().map(|(n, _, _)| *n))
        .map(|(help, name)| {
            let mut line = String::with_capacity(name.len() + help.len() + 2);
            line.push_str(name);
            line.push('\t');
            line.push_str(help);
            line
        })
        .collect()
}

impl FromStr for Action {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_action(s).ok_or_else(|| format!("unknown action '{s}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mode_parses_the_two_modes() {
        assert_eq!("equal".parse::<SplitMode>().unwrap(), SplitMode::Equal);
        assert_eq!("Golden".parse::<SplitMode>().unwrap(), SplitMode::Golden);
        assert_eq!(SplitMode::default(), SplitMode::Equal);
        assert_eq!(SplitMode::Equal.name(), "equal");
    }

    #[test]
    fn retired_split_modes_error_with_guidance() {
        // "explicit" and "auto" described split *direction*, which is now a
        // per-command choice; the mode only governs sizing.
        for retired in ["explicit", "auto"] {
            let error = retired.parse::<SplitMode>().unwrap_err();
            assert!(error.contains("removed"), "{retired}: {error}");
            assert!(error.contains("equal"), "{retired}: {error}");
        }
        let error = "fibonacci".parse::<SplitMode>().unwrap_err();
        assert!(error.contains("equal, golden"), "{error}");
    }
}
