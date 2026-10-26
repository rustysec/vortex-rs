//! Modal input: modes, pending key state and the keymap.

use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;

use crate::action::{Action, Cmd, Motion, Operator};

pub mod keymap;
pub mod keys;
pub mod motions;
pub mod operators;
pub mod textobject;

pub use keys::{normalize, parse_key_sequence, typed_char};

/// Which keymap a key press is resolved against.
///
/// [`KeyContext::Operator`] is not a mode the user sits in — it is the state
/// between an operator (`d`) and its motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyContext {
    Normal,
    Insert,
    Visual,
    Command,
    Operator,
}

impl KeyContext {
    pub fn name(self) -> &'static str {
        match self {
            KeyContext::Normal => "normal",
            KeyContext::Insert => "insert",
            KeyContext::Visual => "visual",
            KeyContext::Command => "command",
            KeyContext::Operator => "operator",
        }
    }

    pub fn from_name(name: &str) -> Option<KeyContext> {
        match name {
            "normal" | "n" => Some(KeyContext::Normal),
            "insert" | "i" => Some(KeyContext::Insert),
            "visual" | "v" => Some(KeyContext::Visual),
            "command" | "prompt" => Some(KeyContext::Command),
            "operator" | "op" => Some(KeyContext::Operator),
            _ => None,
        }
    }
}

/// Editor modes.  `Command` covers every prompt (`:`, `/`, pickers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual(crate::action::VisualKind),
    Command,
}

impl Mode {
    /// Name used in `[keymap.<mode>]` sections.
    pub fn key(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Visual(_) => "visual",
            Mode::Command => "command",
        }
    }

    /// Label shown in the status area (lower case, as in the spec).
    pub fn label(self) -> String {
        match self {
            Mode::Normal => "normal".into(),
            Mode::Insert => "insert".into(),
            Mode::Visual(kind) => match kind {
                crate::action::VisualKind::Char => "visual".into(),
                crate::action::VisualKind::Line => "visual line".into(),
                crate::action::VisualKind::Block => "visual block".into(),
            },
            Mode::Command => "command".into(),
        }
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Mode::Visual(_))
    }

    pub fn visual_kind(self) -> Option<crate::action::VisualKind> {
        match self {
            Mode::Visual(kind) => Some(kind),
            _ => None,
        }
    }
}

pub fn mode_from_name(name: &str) -> Option<Mode> {
    match name {
        "normal" | "n" => Some(Mode::Normal),
        "insert" | "i" => Some(Mode::Insert),
        "visual" | "v" => Some(Mode::Visual(crate::action::VisualKind::Char)),
        "command" | "c" | "prompt" => Some(Mode::Command),
        _ => None,
    }
}

pub fn mode_name(mode: Mode) -> &'static str {
    mode.key()
}

/// Something that needs one more raw key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Awaiting {
    /// `f`, `F`, `t`, `T` — waiting for the character to find.
    Find(crate::action::FindDir),
    /// `i` / `a` — waiting for the text object key.
    TextObject { inside: bool },
    /// `"` — waiting for a register name.
    Register,
    /// `m` — waiting for the name of the mark being set.
    SetMark,
    /// `` ` ``, `'` — waiting for the name of the mark to jump to.
    Mark { line: bool },
    /// `r` — waiting for the replacement character.
    Replace,
}

/// An operator that is waiting for its motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorPending {
    pub operator: Operator,
    /// The keys that produced it, so a repeated sequence means "whole line".
    pub keys: Vec<KeyEvent>,
}

impl OperatorPending {
    pub fn matches(&self, pressed: &[KeyEvent]) -> bool {
        self.keys == pressed
    }
}

/// Keys pressed but not yet resolved, plus counts, registers and operators.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pending {
    /// Prefix of an incomplete key sequence.
    pub keys: Vec<KeyEvent>,
    pub keys_at: Option<Instant>,
    pub count: Option<usize>,
    pub operator: Option<OperatorPending>,
    pub register: Option<char>,
    pub awaiting: Option<Awaiting>,
    /// What the keys so far would run if the sequence stopped here, kept so a
    /// timeout or a non-matching next key can fall back to it (`w` when `wd` is
    /// also bound).
    pub fallback: Option<crate::action::Action>,
}

/// How long a partial key sequence waits for its next key.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(1);

impl Pending {
    pub fn is_active(&self) -> bool {
        !self.keys.is_empty()
            || self.count.is_some()
            || self.operator.is_some()
            || self.register.is_some()
            || self.awaiting.is_some()
    }

    /// Consume the count prefix (`3w` → 3), defaulting to 1.
    pub fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    /// Read the count without consuming it (`10j` needs it twice).
    pub fn peek_count(&self) -> usize {
        self.count.unwrap_or(1)
    }

    pub fn push_digit(&mut self, digit: char) {
        let value = digit as usize - '0' as usize;
        self.count = Some(match self.count {
            None if value == 0 => return, // leading zero is a motion, not a count
            None => value,
            Some(current) => current.saturating_mul(10).saturating_add(value),
        });
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.count = None;
        self.operator = None;
        self.awaiting = None;
        self.register = None;
        self.keys_at = None;
        self.fallback = None;
    }

    /// Clear the key prefix but keep operator/register state.
    pub fn clear_keys(&mut self) {
        self.keys.clear();
        self.keys_at = None;
        self.fallback = None;
    }

    pub fn note_key(&mut self, key: KeyEvent) {
        self.keys.push(key);
        self.keys_at = Some(Instant::now());
    }

    pub fn prefix_expired(&self) -> bool {
        match self.keys_at {
            Some(at) => !self.keys.is_empty() && at.elapsed() > PENDING_TIMEOUT,
            None => false,
        }
    }

    /// What the user has typed so far, echoed in the status area.
    pub fn echo(&self) -> String {
        let mut out = String::new();
        if let Some(count) = self.count {
            out.push_str(&count.to_string());
        }
        if let Some(register) = self.register {
            out.push('"');
            out.push(register);
        }
        if let Some(operator) = &self.operator {
            out.push_str(operator_status(operator.operator));
        }
        if let Some(awaiting) = self.awaiting {
            out.push_str(match awaiting {
                Awaiting::Find(_) => "f",
                Awaiting::TextObject { inside: true } => "i",
                Awaiting::TextObject { inside: false } => "a",
                Awaiting::Register => "\"",
                Awaiting::SetMark => "m",
                Awaiting::Mark { line: false } => "`",
                Awaiting::Mark { line: true } => "'",
                Awaiting::Replace => "r",
            });
        }
        for key in &self.keys {
            out.push_str(&keys::describe(*key));
        }
        out
    }
}

pub(crate) fn operator_status(operator: Operator) -> &'static str {
    match operator {
        Operator::Delete => "d",
        Operator::Change => "c",
        Operator::Yank => "y",
        Operator::Replace => "r",
        Operator::Join => "J",
        Operator::Indent => ">",
        Operator::Unindent => "<",
        Operator::Reformat => "=",
        Operator::SwapCase => "~",
        Operator::Upper => "gU",
        Operator::Lower => "gu",
        Operator::ToggleComment => "gc",
    }
}

/// Default bindings: vim motions with a few modern conveniences.
///
/// Values are action names from [`crate::action::ACTIONS`], so `:help keys`
/// documents exactly what the editor understands.
pub const DEFAULT_BINDINGS: &[(&str, &[(&str, &str)])] = &[
    (
        "normal",
        &[
            // movement
            ("h", "move-left"),
            ("j", "move-down"),
            ("k", "move-up"),
            ("l", "move-right"),
            ("0", "line-start"),
            ("^", "first-non-blank"),
            ("$", "line-end"),
            ("_", "line-end-non-blank"),
            ("|", "column"),
            ("w", "word-forward"),
            ("b", "word-back"),
            ("e", "word-end"),
            ("ge", "word-end-back"),
            ("W", "big-word-forward"),
            ("B", "big-word-back"),
            ("E", "big-word-end"),
            ("gE", "big-word-end-back"),
            ("f", "find-char"),
            ("F", "find-char-back"),
            ("t", "till-char"),
            ("T", "till-char-back"),
            (";", "repeat-find"),
            (",", "repeat-find-reverse"),
            ("gg", "document-start"),
            ("G", "document-end"),
            ("H", "view-top"),
            ("M", "view-middle"),
            ("L", "view-bottom"),
            ("}", "paragraph-next"),
            ("{", "paragraph-prev"),
            (")", "sentence-next"),
            ("(", "sentence-prev"),
            ("%", "match-bracket"),
            ("n", "search-next"),
            ("N", "search-prev"),
            ("pageup", "page-up"),
            ("pagedown", "page-down"),
            ("ctrl-b", "page-up"),
            ("ctrl-f", "page-down"),
            ("ctrl-u", "half-page-up"),
            ("ctrl-d", "half-page-down"),
            ("ctrl-y", "scroll-line-up"),
            ("ctrl-e", "scroll-line-down"),
            ("ctrl-o", "jump-back"),
            ("ctrl-i", "jump-forward"),
            ("z z", "recenter"),
            ("z t", "scroll-to-top"),
            ("z b", "scroll-to-bottom"),
            // text objects
            // insert / mode
            ("i", "insert"),
            ("gi", "insert-previous"),
            ("a", "append"),
            ("A", "append-line-end"),
            ("I", "insert-line-start"),
            ("o", "open-below"),
            ("O", "open-above"),
            ("v", "visual"),
            ("V", "visual-line"),
            ("ctrl-v", "visual-block"),
            ("esc", "cancel"),
            // operators
            ("d", "delete"),
            ("c", "change"),
            ("y", "yank"),
            ("r", "replace"),
            ("J", "join"),
            (">", "indent"),
            ("<", "unindent"),
            ("=", "reindent"),
            ("~", "swap-case-char"),
            ("g~", "swap-case"),
            ("gU", "uppercase"),
            ("gu", "lowercase"),
            ("gc", "toggle-comment"),
            ("x", "delete-char"),
            ("X", "delete-char-back"),
            ("s", "change-char"),
            ("S", "change-line"),
            ("D", "delete-to-line-end"),
            ("C", "change-to-line-end"),
            ("Y", "yank-line"),
            ("p", "paste-after"),
            ("P", "paste-before"),
            ("u", "undo"),
            ("ctrl-r", "redo"),
            (".", "repeat"),
            ("\"", "select-register"),
            ("m", "set-mark"),
            ("`", "goto-mark-char"),
            ("'", "goto-mark-line"),
            ("gp", "paste-after"),
            ("gs", "duplicate-line"),
            // search & prompts
            ("/", "search-forward"),
            ("?", "search-backward"),
            ("*", "find-word-forward"),
            ("#", "find-word-backward"),
            (":", "command-mode"),
            // files & panes (space is the leader)
            ("ctrl-s", "save"),
            ("space p", "command-palette"),
            // The message line advertises ctrl-p, so it has to work.
            ("ctrl-p", "command-palette"),
            ("space f", "find-files"),
            ("space s", "grep-project"),
            ("space b", "switch-buffer"),
            ("space w", "save"),
            ("space q", "quit"),
            ("space d", "show-diagnostics"),
            ("space e", "spell-suggest"),
            // vim's spelling suggestion key.
            ("z=", "spell-suggest"),
            ("space x", "clear-search-highlight"),
            ("space F", "format-buffer"),
            // layout
            ("space o", "only-pane"),
            ("space z", "zoom-pane"),
            ("space v", "split-vertical"),
            ("space h", "split-horizontal"),
            ("space n", "split-auto"),
            ("space c", "close-pane"),
            ("space =", "equalize-panes"),
            // `ctrl-w` then a direction moves between panes, as in vim.
            ("ctrl-w h", "focus-left"),
            ("ctrl-w j", "focus-down"),
            ("ctrl-w k", "focus-up"),
            ("ctrl-w l", "focus-right"),
            ("space left", "focus-left"),
            ("space right", "focus-right"),
            ("space up", "focus-up"),
            ("space down", "focus-down"),
            ("space tab", "focus-next"),
            ("shift-tab", "focus-prev"),
            ("space H", "move-left"),
            ("space J", "move-down"),
            ("space K", "move-up"),
            ("space L", "move-right"),
            ("space ctrl-h", "resize-left"),
            ("space ctrl-j", "resize-down"),
            ("space ctrl-k", "resize-up"),
            ("space ctrl-l", "resize-right"),
            ("space plus", "resize-right"),
            ("space minus", "resize-left"),
            ("space g e", "layout-explicit"),
            ("space g a", "layout-auto"),
            ("space g g", "layout-golden"),
            // toggles
            ("space t n", "toggle-line-numbers"),
            ("space t c", "toggle-cursorline"),
            ("space t w", "toggle-wrap"),
            ("space t s", "toggle-spell"),
            ("space t f", "toggle-auto-format"),
            ("space t m", "toggle-mouse"),
        ],
    ),
    (
        "insert",
        &[
            ("esc", "exit-insert"),
            ("ctrl-c", "exit-insert"),
            ("ctrl-[", "exit-insert"),
            ("enter", "insert-newline"),
            ("ctrl-j", "insert-newline"),
            ("tab", "insert-tab"),
            ("shift-tab", "insert-backtab"),
            ("backspace", "insert-backspace"),
            ("ctrl-h", "insert-backspace"),
            ("delete", "insert-delete"),
            ("ctrl-w", "insert-delete-word"),
            ("ctrl-u", "insert-delete-line-start"),
            ("ctrl-t", "insert-indent-line"),
            ("ctrl-o", "normal-once"),
            ("left", "move-left"),
            ("right", "move-right"),
            ("up", "move-up"),
            ("down", "move-down"),
            ("home", "line-start"),
            ("end", "line-end"),
        ],
    ),
    (
        "visual",
        &[
            ("v", "visual"),
            ("V", "visual-line"),
            ("ctrl-v", "visual-block"),
            ("o", "swap-anchor"),
            ("esc", "normal"),
            ("i", "inside"),
            ("a", "around"),
        ],
    ),
    (
        "operator",
        &[
            // `i` / `a` introduce text objects here rather than insert mode.
            ("i", "inside"),
            ("a", "around"),
            ("t", "till-char"),
            ("T", "till-char-back"),
        ],
    ),
];

/// Every keymap, one per resolution context.
#[derive(Debug, Clone, Default)]
pub struct Keymaps {
    pub normal: keymap::Keymap,
    pub insert: keymap::Keymap,
    pub visual: keymap::Keymap,
    /// Active while an operator waits for its motion.
    pub operator: keymap::Keymap,
}

impl Keymaps {
    /// Built-in bindings with `[keymap.*]` overrides applied.
    pub fn build(config: &crate::config::Config) -> (Keymaps, Vec<String>) {
        let mut warnings = Vec::new();

        let mut normal = keymap::Keymap::default();
        for (keys, action) in bindings_for(KeyContext::Normal) {
            bind(&mut normal, keys, action, KeyContext::Normal, &mut warnings);
        }
        apply_config_overrides(&mut normal, config, KeyContext::Normal, &mut warnings);

        // Visual mode reuses the normal bindings; motions extend the selection.
        let mut visual = normal.derive_visual();
        for (keys, action) in bindings_for(KeyContext::Visual) {
            bind(&mut visual, keys, action, KeyContext::Visual, &mut warnings);
        }
        apply_config_overrides(&mut visual, config, KeyContext::Visual, &mut warnings);

        let mut insert = keymap::Keymap::default();
        for (keys, action) in bindings_for(KeyContext::Insert) {
            bind(&mut insert, keys, action, KeyContext::Insert, &mut warnings);
        }
        apply_config_overrides(&mut insert, config, KeyContext::Insert, &mut warnings);

        // Operator pending: every motion works, plus `i`/`a` text objects.
        let mut operator = normal.clone();
        for (keys, action) in bindings_for(KeyContext::Operator) {
            bind(
                &mut operator,
                keys,
                action,
                KeyContext::Operator,
                &mut warnings,
            );
        }
        apply_config_overrides(&mut operator, config, KeyContext::Operator, &mut warnings);

        // Unknown sections are reported rather than silently ignored.
        for section in config.keymap.keys() {
            if KeyContext::from_name(section).is_none() {
                warnings.push(format!(
                    "unknown keymap section [keymap.{section}] (expected normal, insert, visual, operator or command)"
                ));
            }
        }

        (
            Keymaps {
                normal,
                insert,
                visual,
                operator,
            },
            warnings,
        )
    }

    /// Every binding as `(context, keys, action)`, for `:map`.
    pub fn describe(&self) -> Vec<(&'static str, String, String)> {
        let mut entries = Vec::new();
        for (context, map) in [
            ("normal", &self.normal),
            ("visual", &self.visual),
            ("operator", &self.operator),
            ("insert", &self.insert),
        ] {
            for (keys, action) in map.entries() {
                let rendered = keys
                    .iter()
                    .map(|key| keys::describe(*key))
                    .collect::<Vec<_>>()
                    .join(" ");
                entries.push((context, rendered, keymap::action_name(&action)));
            }
        }
        entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        entries
    }

    pub fn for_context(&self, context: KeyContext) -> &keymap::Keymap {
        match context {
            KeyContext::Normal => &self.normal,
            KeyContext::Insert => &self.insert,
            KeyContext::Visual => &self.visual,
            KeyContext::Operator => &self.operator,
            // Command mode has its own prompt bindings.
            KeyContext::Command => &self.normal,
        }
    }

    pub fn for_mode(&self, mode: Mode, operator_pending: bool) -> &keymap::Keymap {
        match mode {
            Mode::Insert => &self.insert,
            Mode::Visual(_) => &self.visual,
            Mode::Normal | Mode::Command => {
                if operator_pending {
                    &self.operator
                } else {
                    &self.normal
                }
            }
        }
    }
}

fn apply_config_overrides(
    map: &mut keymap::Keymap,
    config: &crate::config::Config,
    context: KeyContext,
    warnings: &mut Vec<String>,
) {
    for (section, table) in &config.keymap {
        if KeyContext::from_name(section) != Some(context) {
            continue;
        }
        for (keys, action) in table {
            apply_override(map, keys, action, context, warnings);
        }
    }
}

fn bindings_for(context: KeyContext) -> &'static [(&'static str, &'static str)] {
    DEFAULT_BINDINGS
        .iter()
        .find(|(name, _)| *name == context.name())
        .map(|(_, table)| *table)
        .unwrap_or(&[])
}

fn bind(
    map: &mut keymap::Keymap,
    keys: &str,
    action: &str,
    context: KeyContext,
    warnings: &mut Vec<String>,
) {
    let Some(parsed) = parse_key_sequence(keys) else {
        warnings.push(format!(
            "default binding \"{keys}\" in {} mode does not parse",
            context.name()
        ));
        return;
    };
    let Some(action) = crate::action::parse_action(action) else {
        warnings.push(format!(
            "default binding \"{keys}\" refers to unknown action \"{action}\""
        ));
        return;
    };
    map.insert(&parsed, action);
}

fn apply_override(
    map: &mut keymap::Keymap,
    keys: &str,
    action: &str,
    context: KeyContext,
    warnings: &mut Vec<String>,
) {
    let Some(parsed) = parse_key_sequence(keys) else {
        warnings.push(format!(
            "[keymap.{}] cannot parse \"{keys}\"",
            context.name()
        ));
        return;
    };
    let Some(action) = crate::action::parse_action(action) else {
        warnings.push(format!(
            "[keymap.{}] \"{keys}\" refers to unknown action \"{action}\"",
            context.name()
        ));
        return;
    };
    if action == Action::Cmd(Cmd::Noop) {
        map.remove(&parsed);
    } else {
        map.insert(&parsed, action);
    }
}

/// Motions become selection-extending actions in visual mode.
fn extend_action(action: Action) -> Action {
    match action {
        Action::Motion(motion) => Action::Extend(motion),
        other => other,
    }
}

impl keymap::Keymap {
    fn derive_visual(&self) -> keymap::Keymap {
        let mut map = keymap::Keymap::default();
        for (keys, action) in self.entries() {
            map.insert(&keys, extend_action(action));
        }
        map
    }
}

/// A digit that starts a count (`1`-`9`, plus `0` once a count is running).
pub fn count_digit(event: KeyEvent, count_active: bool) -> Option<char> {
    match keys::typed_char(event) {
        Some(digit @ '1'..='9') => Some(digit),
        Some('0') if count_active => Some('0'),
        _ => None,
    }
}

/// Motions that are valid while an operator is pending.
pub fn motion_is_operable(motion: Motion) -> bool {
    !matches!(
        motion,
        Motion::SearchNext | Motion::SearchPrev | Motion::PageForward | Motion::PageBackward
    )
}
