//! Key bindings, stored as a trie of key sequences.
//!
//! `crossterm::event::KeyEvent` is neither `Eq` nor `Hash`, so keys are
//! normalised into [`Key`] first.  Lookups try the several spellings terminals
//! disagree about (shift on/off, shifted symbols).

use std::collections::HashMap;

use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

use super::keys::lookup_variants;
use super::keys::{format_key, normalize};
use crate::action::{Action, Cmd, Motion, Operator};

/// A normalised key press: code plus the three modifiers we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: crossterm::event::KeyCode,
    pub modifiers: crossterm::event::KeyModifiers,
}

impl Key {
    pub fn new(event: KeyEvent) -> Self {
        let event = normalize(event);
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
    }

    pub fn to_event(self) -> KeyEvent {
        KeyEvent {
            code: self.code,
            modifiers: self.modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Spellings of this key that terminals might deliver.
    fn variants(self) -> Vec<Key> {
        lookup_variants(self.to_event())
            .into_iter()
            .map(Key::new)
            .collect()
    }

    pub fn display(self) -> String {
        format_key(self.to_event())
    }
}

/// Result of resolving a key sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A binding matched exactly.
    Action(Action),
    /// The keys so far are a prefix of longer bindings.
    Prefix,
    /// Nothing starts with these keys.
    Unknown,
}

#[derive(Debug, Clone, Default)]
struct Branch {
    children: HashMap<Key, Branch>,
    action: Option<Action>,
}

/// A keymap for one context.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    root: Branch,
    size: usize,
}

impl Keymap {
    /// Bind `keys` to `action`.
    pub fn insert(&mut self, keys: &[KeyEvent], action: Action) {
        if keys.is_empty() {
            return;
        }
        let mut branch = &mut self.root;
        for event in keys {
            let key = Key::new(*event);
            branch = branch.children.entry(key).or_default();
        }
        if branch.action.is_none() {
            self.size += 1;
        }
        branch.action = Some(action);
    }

    /// Remove a binding (used by `"none"` in the config), pruning dead branches
    /// so the keys stop reporting `Prefix` afterwards.
    pub fn remove(&mut self, keys: &[KeyEvent]) -> bool {
        let path: Vec<Key> = keys.iter().map(|event| Key::new(*event)).collect();
        if path.is_empty() {
            return false;
        }
        let removed = prune(&mut self.root, &path);
        if removed {
            self.size = self.size.saturating_sub(1);
        }
        removed
    }

    /// Resolve a pressed sequence.
    /// The action these keys run *on their own*, even when longer bindings also
    /// start with them: `w` while the config binds `wd`.
    ///
    /// [`Lookup`] reports `Prefix` for such a node, which is right for waiting
    /// on the next key and wrong on its own — without this, binding `wd` would
    /// quietly make `w` unusable.
    pub fn bound_at(&self, pressed: &[KeyEvent]) -> Option<Action> {
        let mut branch = &self.root;
        for event in pressed {
            let candidates = Key::new(*event).variants();
            branch = candidates.iter().find_map(|key| branch.children.get(key))?;
        }
        branch.action.clone()
    }

    pub fn lookup(&self, pressed: &[KeyEvent]) -> Lookup {
        if pressed.is_empty() {
            return Lookup::Unknown;
        }
        let mut branch = &self.root;
        for event in pressed {
            let candidates = Key::new(*event).variants();
            let next = candidates.iter().find_map(|key| branch.children.get(key));
            match next {
                Some(child) => branch = child,
                None => return Lookup::Unknown,
            }
        }
        match &branch.action {
            // Exact match with nothing longer possible.
            Some(action) if branch.children.is_empty() => Lookup::Action(action.clone()),
            // Either a pure prefix, or a binding that could still grow into a
            // longer one — wait for the next key (and time out eventually).
            _ => Lookup::Prefix,
        }
    }

    /// Bindings that could follow `pressed`, for which-key style hints.
    pub fn completions(&self, pressed: &[KeyEvent]) -> Vec<(String, &'static str)> {
        let Some(branch) = self.branch(pressed) else {
            return Vec::new();
        };
        let mut out: Vec<(String, &'static str)> = branch
            .children
            .iter()
            .map(|(key, child)| {
                let help = child.action.as_ref().map(action_help).unwrap_or("…");
                (key.display(), help)
            })
            .collect();
        out.sort();
        out
    }

    fn branch(&self, pressed: &[KeyEvent]) -> Option<&Branch> {
        let mut branch = &self.root;
        for event in pressed {
            let candidates = Key::new(*event).variants();
            branch = candidates.iter().find_map(|key| branch.children.get(key))?;
        }
        Some(branch)
    }

    /// Every binding: `(keys, action)`.
    pub fn entries(&self) -> Vec<(Vec<KeyEvent>, Action)> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        collect(&self.root, &mut path, &mut out);
        out
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// A `key  action` listing for `:help keys`.
    pub fn documentation(&self) -> Vec<(String, String, &'static str)> {
        self.entries()
            .into_iter()
            .map(|(keys, action)| {
                let rendered = keys
                    .iter()
                    .map(|key| format_key(*key))
                    .collect::<Vec<_>>()
                    .join(" ");
                (rendered, action_name(&action), action_help(&action))
            })
            .collect()
    }
}

/// Drop the binding at `path`, then tidy up branches that lead nowhere.
fn prune(branch: &mut Branch, path: &[Key]) -> bool {
    let Some((key, rest)) = path.split_first() else {
        return false;
    };
    let Some(child) = branch.children.get_mut(key) else {
        return false;
    };
    let removed = if rest.is_empty() {
        child.action.take().is_some()
    } else {
        prune(child, rest)
    };
    if removed {
        let dead = child.action.is_none() && child.children.is_empty();
        if dead {
            branch.children.remove(key);
        }
    }
    removed
}

fn collect(branch: &Branch, path: &mut Vec<KeyEvent>, out: &mut Vec<(Vec<KeyEvent>, Action)>) {
    if let Some(action) = &branch.action {
        out.push((path.clone(), action.clone()));
    }
    let mut children: Vec<_> = branch.children.iter().collect();
    children.sort_by_key(|(key, _)| key.display());
    for (key, child) in children {
        path.push(key.to_event());
        collect(child, path, out);
        path.pop();
    }
}

fn action_help(action: &Action) -> &'static str {
    let name = action_name(action);
    crate::action::ACTIONS
        .iter()
        .find(|(action_name, _, _)| *action_name == name)
        .map(|(_, _, help)| *help)
        .unwrap_or("")
}

/// The registry name of an action, used for help output.
pub fn action_name(action: &Action) -> String {
    use crate::action::Action as A;
    // The registry is the single source of truth for bindable names; the
    // composed fallbacks below only cover actions that are not registered.
    if let Some(name) = crate::action::name_for(action) {
        return name.to_string();
    }
    match action {
        A::Motion(motion) | A::Extend(motion) => motion_name(*motion),
        A::Operator(operator) => operator_name(*operator).to_string(),
        A::OperatorMotion(operator, motion) => {
            format!("{}-{}", operator_name(*operator), motion_name(*motion))
        }
        A::OperatorLine(operator) => format!("{}-line", operator_name(*operator)),
        A::Cmd(command) => cmd_name(command),
    }
}

fn operator_name(operator: Operator) -> &'static str {
    match operator {
        Operator::Delete => "delete",
        Operator::Change => "change",
        Operator::Yank => "yank",
        Operator::Replace => "replace",
        Operator::Join => "join",
        Operator::Indent => "indent",
        Operator::Unindent => "unindent",
        Operator::Reformat => "reindent",
        Operator::SwapCase => "swap-case",
        Operator::Upper => "uppercase",
        Operator::Lower => "lowercase",
        Operator::ToggleComment => "toggle-comment",
    }
}

fn motion_name(motion: Motion) -> String {
    let named = |name: &'static str| name.to_string();
    match motion {
        Motion::Left => named("move-left"),
        Motion::Right => named("move-right"),
        Motion::Up => named("move-up"),
        Motion::Down => named("move-down"),
        Motion::LineStart => named("line-start"),
        Motion::FirstNonBlank => named("first-non-blank"),
        Motion::LineEnd => named("line-end"),
        Motion::LineEndFirstNonBlank => named("line-end-non-blank"),
        Motion::Column => named("column"),
        Motion::WordStart => named("word-forward"),
        Motion::WordEnd => named("word-end"),
        Motion::WordBack => named("word-back"),
        Motion::WordEndBack => named("word-end-back"),
        Motion::BigWordStart => named("big-word-forward"),
        Motion::BigWordEnd => named("big-word-end"),
        Motion::BigWordBack => named("big-word-back"),
        Motion::BigWordEndBack => named("big-word-end-back"),
        Motion::FindChar(dir) => match dir {
            crate::action::FindDir::Forward => named("find-char"),
            crate::action::FindDir::Backward => named("find-char-back"),
            crate::action::FindDir::TillForward => named("till-char"),
            crate::action::FindDir::TillBackward => named("till-char-back"),
        },
        Motion::RepeatFind => named("repeat-find"),
        Motion::RepeatFindReverse => named("repeat-find-reverse"),
        Motion::DocumentStart => named("document-start"),
        Motion::DocumentEnd => named("document-end"),
        Motion::ViewTop => named("view-top"),
        Motion::ViewMiddle => named("view-middle"),
        Motion::ViewBottom => named("view-bottom"),
        Motion::PageForward => named("page-down"),
        Motion::PageBackward => named("page-up"),
        Motion::HalfPageForward => named("half-page-down"),
        Motion::HalfPageBackward => named("half-page-up"),
        Motion::ParagraphNext => named("paragraph-next"),
        Motion::ParagraphPrev => named("paragraph-prev"),
        Motion::SentenceNext => named("sentence-next"),
        Motion::SentencePrev => named("sentence-prev"),
        Motion::MatchBracket => named("match-bracket"),
        Motion::CharAhead => named("current-char"),
        Motion::TillLineEnd => named("till-line-end"),
        Motion::SearchNext => named("search-next"),
        Motion::SearchPrev => named("search-prev"),
        Motion::TextObject { inside: true } => named("inside"),
        Motion::TextObject { inside: false } => named("around"),
    }
}

/// `-project` for the project scope, empty for the buffer scope, so the
/// diagnostic action names round-trip through `parse_action`.
fn diagnostic_suffix(scope: crate::diagnostic::DiagnosticScope) -> &'static str {
    match scope {
        crate::diagnostic::DiagnosticScope::Buffer => "",
        crate::diagnostic::DiagnosticScope::Project => "-project",
    }
}

fn cmd_name(command: &Cmd) -> String {
    use crate::action::Cmd;
    (match command {
        Cmd::Insert => "insert",
        Cmd::InsertAtWordEnd => "insert-word-end",
        Cmd::Append => "append",
        Cmd::AppendLineEnd => "append-line-end",
        Cmd::InsertLineStart => "insert-line-start",
        Cmd::AppendLineStart => "append-line-start",
        Cmd::OpenBelow => "open-below",
        Cmd::OpenAbove => "open-above",
        Cmd::Normal => "normal",
        Cmd::Visual(kind) => match kind {
            crate::action::VisualKind::Char => "visual",
            crate::action::VisualKind::Line => "visual-line",
            crate::action::VisualKind::Block => "visual-block",
        },
        Cmd::Cancel => "cancel",
        Cmd::ExitInsert => "exit-insert",
        Cmd::DeleteCharNext => "delete-char",
        Cmd::DeleteCharPrev => "delete-char-back",
        Cmd::DeleteSelection => "delete-selection",
        Cmd::PutAfter => "paste-after",
        Cmd::PutBefore => "paste-before",
        Cmd::PutAfterLine => "paste-line-after",
        Cmd::PutBeforeLine => "paste-line-before",
        Cmd::ReplaceChar => "replace-char",
        Cmd::Undo => "undo",
        Cmd::Redo => "redo",
        Cmd::Repeat => "repeat",
        Cmd::SelectRegister => "select-register",
        Cmd::SetMark => "set-mark",
        Cmd::GotoMarkChar => "goto-mark-char",
        Cmd::GotoMarkLine => "goto-mark-line",
        Cmd::DuplicateLine => "duplicate-line",
        Cmd::RemoveEmptyLines => "remove-empty-lines",
        Cmd::SwapAnchor => "swap-anchor",
        Cmd::InsertPrevious => "insert-previous",
        Cmd::Save => "save",
        Cmd::SaveAs => "save-as",
        Cmd::SaveAll => "save-all",
        Cmd::Quit => "quit",
        Cmd::ForceQuit => "force-quit",
        Cmd::QuitAll => "quit-all",
        Cmd::ReloadFile => "reload-file",
        Cmd::CommandMode => "command-mode",
        Cmd::SearchForward => "search-forward",
        Cmd::SearchBackward => "search-backward",
        Cmd::FindWordForward => "find-word-forward",
        Cmd::FindWordBackward => "find-word-backward",
        Cmd::ClearSearchHighlight => "clear-search-highlight",
        Cmd::FindFiles => "find-files",
        Cmd::GrepProject => "grep-project",
        Cmd::GotoLine => "goto-line",
        Cmd::SwitchBuffer => "switch-buffer",
        Cmd::CommandPalette => "command-palette",
        Cmd::SplitVertical => "split-vertical",
        Cmd::SplitHorizontal => "split-horizontal",
        Cmd::SplitAuto => "split-auto",
        Cmd::OnlyPane => "only-pane",
        Cmd::ZoomPane => "zoom-pane",
        Cmd::EqualizePanes => "equalize-panes",
        Cmd::CycleLayout(mode) => return format!("layout-{}", mode.name()),
        Cmd::FocusNext => "focus-next",
        Cmd::FocusPrev => "focus-prev",
        // Directional commands must print the exact name that re-binds them.
        Cmd::FocusDir(direction) => return format!("focus-{}", direction.label()),
        Cmd::MoveDir(direction) => return format!("buf-move-{}", direction.label()),
        Cmd::ResizeDir(direction) => return format!("resize-{}", direction.label()),
        Cmd::ClosePane => "close-pane",
        Cmd::Recenter => "recenter",
        Cmd::ScrollToTop => "scroll-to-top",
        Cmd::ScrollToBottom => "scroll-to-bottom",
        Cmd::ScrollLine(direction) => return format!("scroll-line-{}", direction.label()),
        Cmd::JumpBack => "jump-back",
        Cmd::JumpForward => "jump-forward",
        Cmd::InsertNewline => "insert-newline",
        Cmd::InsertTab => "insert-tab",
        Cmd::InsertBacktab => "insert-backtab",
        Cmd::InsertBackspace => "insert-backspace",
        Cmd::InsertDelete => "insert-delete",
        Cmd::InsertDeleteWord => "insert-delete-word",
        Cmd::InsertDeleteLineStart => "insert-delete-line-start",
        Cmd::InsertIndentLine => "insert-indent-line",
        Cmd::NormalOnce => "normal-once",
        Cmd::ToggleOption(option) => return format!("toggle-{}", option.key()),
        Cmd::SpellSuggest => "spell-suggest",
        Cmd::ShowDiagnostics(scope) => {
            return format!("show-diagnostics{}", diagnostic_suffix(*scope));
        }
        Cmd::DiagnosticNext(scope) => {
            return format!("diagnostic-next{}", diagnostic_suffix(*scope));
        }
        Cmd::DiagnosticPrev(scope) => {
            return format!("diagnostic-prev{}", diagnostic_suffix(*scope));
        }
        Cmd::Hover => "hover",
        Cmd::CodeAction => "code-action",
        Cmd::FormatBuffer => "format-buffer",
        Cmd::Ex(command) => return command.clone(),
        Cmd::ForceQuitAll => "force-quit-all",
        Cmd::Noop => "noop",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::parse_key_sequence;
    use crate::input::typed_char;

    fn bind(map: &mut Keymap, keys: &str, name: &str) {
        let keys = parse_key_sequence(keys).unwrap();
        let action = crate::action::parse_action(name).unwrap();
        map.insert(&keys, action);
    }

    fn press(text: &str) -> Vec<KeyEvent> {
        parse_key_sequence(text).unwrap()
    }

    #[test]
    fn single_key_lookup() {
        let mut map = Keymap::default();
        bind(&mut map, "w", "word-forward");
        assert!(matches!(
            map.lookup(&press("w")),
            Lookup::Action(Action::Motion(_))
        ));
        assert_eq!(map.lookup(&press("q")), Lookup::Unknown);
    }

    #[test]
    fn prefix_wins_over_shorter_action() {
        let mut map = Keymap::default();
        bind(&mut map, "g", "noop");
        bind(&mut map, "g g", "document-start");
        // `g` alone could still grow into `g g`, so it stays pending.
        assert_eq!(map.lookup(&press("g")), Lookup::Prefix);
        assert!(matches!(
            map.lookup(&press("g g")),
            Lookup::Action(Action::Motion(Motion::DocumentStart))
        ));
    }

    #[test]
    fn unknown_extension_of_unknown_prefix() {
        let mut map = Keymap::default();
        bind(&mut map, "g g", "document-start");
        assert_eq!(map.lookup(&press("x")), Lookup::Unknown);
        assert_eq!(map.lookup(&press("g x")), Lookup::Unknown);
    }

    #[test]
    fn remove_unbinds() {
        let mut map = Keymap::default();
        bind(&mut map, "ctrl-s", "save");
        assert!(map.remove(&press("ctrl-s")));
        assert_eq!(map.lookup(&press("ctrl-s")), Lookup::Unknown);
        assert!(!map.remove(&press("ctrl-s")));
    }

    #[test]
    fn no_default_binding_is_shadowed() {
        // Two entries for the same keys mean the first one is unreachable.
        for (context, table) in crate::input::DEFAULT_BINDINGS {
            let mut seen = std::collections::HashSet::new();
            for (keys, action) in *table {
                assert!(
                    seen.insert((*keys, *action)),
                    "duplicate binding for {keys:?} in {context} mode"
                );
            }
        }
    }

    #[test]
    fn the_default_keymap_is_sane() {
        let (_maps, warnings) = crate::input::Keymaps::build(&crate::config::Config::default());
        assert!(warnings.is_empty(), "broken default bindings: {warnings:?}");
    }

    #[test]
    fn diagnostic_scope_names_round_trip() {
        // `:map` prints `action_name`; the scoped diagnostic names it shows
        // for `]d` and its project-scope siblings must re-bind identically.
        for name in [
            "show-diagnostics",
            "show-diagnostics-project",
            "diagnostic-next",
            "diagnostic-next-project",
            "diagnostic-prev",
            "diagnostic-prev-project",
        ] {
            let action = crate::action::parse_action(name)
                .unwrap_or_else(|| panic!("{name:?} is not an action"));
            let rendered = action_name(&action);
            assert_eq!(rendered, name, "{name:?} renders as {rendered:?}");
            assert_eq!(
                crate::action::parse_action(&rendered),
                Some(action),
                "{rendered:?} does not parse back"
            );
        }
    }

    #[test]
    fn every_bound_action_name_can_be_rebound() {
        // `:map` prints `action_name`; a name shown there must be accepted by
        // `parse_action` and mean the same thing, so directional commands
        // (`focus-left`, `resize-up`, …) survive the round trip.
        for (context, table) in crate::input::DEFAULT_BINDINGS {
            for (keys, name) in *table {
                let action = crate::action::parse_action(name)
                    .unwrap_or_else(|| panic!("default binding {context} {keys:?} -> {name:?}"));
                let rendered = action_name(&action);
                assert_eq!(
                    rendered, *name,
                    "{context} {keys:?} renders as {rendered:?}, which would not rebind"
                );
                assert_eq!(
                    crate::action::parse_action(&rendered),
                    Some(action),
                    "{rendered:?} does not parse back"
                );
            }
        }
    }

    #[test]
    fn shifted_and_unshifted_both_match() {
        let mut map = Keymap::default();
        bind(&mut map, "<", "unindent");
        // A terminal may report `<` as shift-comma or plain comma.
        let as_shifted = press("<");
        let as_unshifted = parse_key_sequence("shift-comma").unwrap();
        assert!(matches!(map.lookup(&as_shifted), Lookup::Action(_)));
        assert!(matches!(map.lookup(&as_unshifted), Lookup::Action(_)));
    }

    #[test]
    fn completions_list_candidates() {
        let mut map = Keymap::default();
        bind(&mut map, "space f", "find-files");
        bind(&mut map, "space s", "grep-project");
        let completions = map.completions(&press("space"));
        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].0, "f");
    }

    #[test]
    fn uppercase_letters_are_distinct() {
        let mut map = Keymap::default();
        bind(&mut map, "w", "word-forward");
        bind(&mut map, "W", "big-word-forward");
        assert!(matches!(
            map.lookup(&press("w")),
            Lookup::Action(Action::Motion(Motion::WordStart))
        ));
        assert!(matches!(
            map.lookup(&press("W")),
            Lookup::Action(Action::Motion(Motion::BigWordStart))
        ));
    }

    #[test]
    fn typed_char_ignores_modifier_combinations() {
        assert_eq!(typed_char(press("x")[0]), Some('x'));
        assert_eq!(typed_char(press("ctrl-x")[0]), None);
    }
}
