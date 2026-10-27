//! The prompt: command line, search entry and fuzzy pickers.
//!
//! A [`Prompt`] is an editable line plus an optional candidate list.  The same
//! machinery serves `:` (with command and path completion), `/` (with search
//! history) and the `ctrl-p` style pickers, so key handling stays consistent
//! everywhere text is typed.

use std::any::Any;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::fuzzy;
use crate::input::keys;

/// One candidate in a picker.
pub struct Item {
    pub label: String,
    /// Secondary text shown dimmed after the label (path, language, …).
    pub detail: Option<String>,
    /// Multi-line preview shown under the list.
    pub preview: Option<String>,
    /// Whatever the app needs to act on the selection.
    pub payload: Option<Box<dyn Any>>,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
            preview: None,
            payload: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }

    pub fn payload<T: Any>(mut self, payload: T) -> Self {
        self.payload = Some(Box::new(payload));
        self
    }

    pub fn payload_as<T: Any>(&self) -> Option<&T> {
        self.payload.as_ref().and_then(|box_| box_.downcast_ref())
    }

    /// Text used for fuzzy matching.
    pub fn match_key(&self) -> &str {
        &self.label
    }
}

/// What the tab key completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Nothing — a plain text prompt.
    None,
    /// `:command` names, then paths once a command is chosen.
    Command,
    /// File system paths.
    Path,
}

/// What the app should do after a key press in a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    /// Keep editing.
    Continue,
    /// Enter was pressed: use the typed text.
    Submit,
    /// Enter was pressed on a candidate: `index` is an index into [`Prompt::items`].
    SubmitItem(usize),
    /// Esc or `ctrl-c`: close the prompt.
    Cancel,
}

/// An editable line with an optional filtered candidate list.
pub struct Prompt {
    pub label: String,
    pub text: String,
    /// Caret position, as a character index into `text`.
    caret: usize,
    pub items: Vec<Item>,
    filtered: Vec<usize>,
    pub selected: usize,
    /// Whether a candidate list is shown and navigable.
    pub select_mode: bool,
    pub completion: Completion,
    /// Command history, oldest first.
    history: Vec<String>,
    history_index: Option<usize>,
    /// Text set aside while walking history.
    stash: String,
    /// Ghost text after the caret, from tab completion.
    pub completion_suffix: String,
    /// Maximum candidates shown.
    pub limit: usize,
    /// Search-style prompts wrap the typed text in the status line.
    pub smart_case: bool,
    /// Whether the picker filters candidates by the prompt text.  The grep
    /// picker turns this off: the prompt text *is* the engine's pattern, and
    /// hit labels (`path:line`) would never match it, hiding every result.
    pub filter_items: bool,
}

impl Prompt {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            text: String::new(),
            caret: 0,
            items: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            select_mode: false,
            completion: Completion::None,
            history: Vec::new(),
            history_index: None,
            stash: String::new(),
            completion_suffix: String::new(),
            limit: 200,
            smart_case: true,
            filter_items: true,
        }
    }

    /// A command-line prompt with command completion.
    pub fn command(initial: impl Into<String>) -> Self {
        let mut prompt = Self::new(":");
        prompt.completion = Completion::Command;
        prompt.set_text(initial);
        prompt
    }

    /// A search prompt.
    pub fn search(forward: bool, initial: impl Into<String>) -> Self {
        let mut prompt = Self::new(if forward { "/" } else { "?" });
        prompt.set_text(initial);
        prompt
    }

    /// A picker over `items`, filtering as the user types.
    pub fn picker(label: impl Into<String>, items: Vec<Item>) -> Self {
        let mut prompt = Self::new(label);
        prompt.select_mode = true;
        prompt.set_items(items);
        prompt
    }

    pub fn with_completion(mut self, completion: Completion) -> Self {
        self.completion = completion;
        self
    }

    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.history = history;
        self
    }

    // ------------------------------------------------------------ candidates

    pub fn set_items(&mut self, items: Vec<Item>) {
        self.items = items;
        self.refilter();
    }

    /// Append candidates (search results stream in).
    pub fn extend_items(&mut self, items: Vec<Item>) {
        self.items.extend(items);
        self.refilter();
    }

    /// Drop every candidate.  A new search starts from an empty list: hits that
    /// streamed in for the previous, usually broader, pattern must not stay on
    /// screen next to the new ones, or the list stops being the result of the
    /// pattern that is in the prompt.
    pub fn clear_items(&mut self) {
        self.items.clear();
        self.filtered.clear();
        self.selected = 0;
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.caret = text.chars().count();
        self.text = text;
        self.completion_suffix.clear();
        if self.select_mode {
            self.refilter();
        }
    }

    pub fn refilter(&mut self) {
        if !self.select_mode {
            self.filtered.clear();
            return;
        }
        if !self.filter_items {
            // Engine-filtered results (grep): show them all, in engine order.
            self.filtered = (0..self.items.len()).collect();
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
            return;
        }
        let scored = fuzzy::filter_and_sort(
            &self.items,
            &self.text,
            self.smart_case,
            self.limit,
            Item::match_key,
        );
        self.filtered = scored.into_iter().map(|(index, _)| index).collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// Indices of the visible candidates.
    pub fn visible(&self) -> &[usize] {
        &self.filtered
    }

    pub fn item(&self, index: usize) -> Option<&Item> {
        self.items.get(index)
    }

    /// The currently highlighted candidate.
    pub fn selected_item(&self) -> Option<&Item> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    pub fn selected_visible_index(&self) -> Option<usize> {
        if self.filtered.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    fn select(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    // ------------------------------------------------------------ editing

    pub fn caret(&self) -> usize {
        self.caret
    }

    fn insert(&mut self, character: char) {
        self.completion_suffix.clear();
        let position = self.text.char_indices().nth(self.caret).map(|(at, _)| at);
        match position {
            Some(at) => self.text.insert(at, character),
            None => self.text.push(character),
        }
        self.caret += 1;
        if self.select_mode {
            self.refilter();
        }
    }

    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        self.completion_suffix.clear();
        let position = self
            .text
            .char_indices()
            .nth(self.caret - 1)
            .map(|(at, _)| at);
        if let Some(at) = position {
            let end = self
                .text
                .char_indices()
                .nth(self.caret)
                .map(|(at, _)| at)
                .unwrap_or(self.text.len());
            self.text.replace_range(at..end, "");
        }
        self.caret -= 1;
        if self.select_mode {
            self.refilter();
        }
    }

    fn delete_forward(&mut self) {
        let count = self.text.chars().count();
        if self.caret >= count {
            return;
        }
        self.completion_suffix.clear();
        let start = self.text.char_indices().nth(self.caret).map(|(at, _)| at);
        let end = self
            .text
            .char_indices()
            .nth(self.caret + 1)
            .map(|(at, _)| at)
            .unwrap_or(self.text.len());
        if let (Some(start), Some(end)) = (start, Some(end)) {
            self.text.replace_range(start..end, "");
        }
        if self.select_mode {
            self.refilter();
        }
    }

    fn delete_word(&mut self) {
        if self.caret == 0 {
            return;
        }
        let characters: Vec<char> = self.text.chars().collect();
        let mut index = self.caret;
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !characters[index - 1].is_whitespace() {
            index -= 1;
        }
        let start = characters[..index].iter().collect::<String>().len();
        let end = characters[..self.caret].iter().collect::<String>().len();
        self.text.replace_range(start..end, "");
        self.caret = index;
        self.completion_suffix.clear();
        if self.select_mode {
            self.refilter();
        }
    }

    fn move_caret(&mut self, delta: isize) {
        let count = self.text.chars().count() as isize;
        self.caret = (self.caret as isize + delta).clamp(0, count) as usize;
    }

    /// Recall a history entry; `None` restores the text typed before browsing.
    fn recall_history(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        let len = self.history.len() as isize;
        let index = match self.history_index {
            // Nothing recalled yet: the first Up shows the most recent entry.
            None => {
                if delta >= 0 || len == 0 {
                    return;
                }
                self.stash = self.text.clone();
                len - 1
            }
            Some(index) => index as isize + delta,
        };
        if index < 0 || index >= len {
            // Past either end: the text the user was typing comes back.
            self.history_index = None;
            let text = std::mem::take(&mut self.stash);
            self.set_text(text);
            return;
        }
        let index = index as usize;
        self.history_index = Some(index);
        let entry = self.history[index].clone();
        self.caret = entry.chars().count();
        self.text = entry;
    }

    /// Record a submitted line in history.
    pub fn remember(&mut self, entry: &str) {
        if entry.trim().is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == entry) {
            return;
        }
        self.history.push(entry.to_string());
        if self.history.len() > 200 {
            let excess = self.history.len() - 200;
            self.history.drain(0..excess);
        }
        self.history_index = None;
        self.stash.clear();
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Everything the user has typed, including the completion ghost text.
    pub fn display_text(&self) -> String {
        if self.completion_suffix.is_empty() {
            return self.text.clone();
        }
        format!("{}{}", self.text, self.completion_suffix)
    }

    // ------------------------------------------------------------ keys

    /// Handle a key press while the prompt is open.
    pub fn handle_key(&mut self, key: KeyEvent) -> PromptAction {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // `Esc` cancels, but `Alt-…` chords arrive as Alt held with a code.
        if key.code == KeyCode::Esc {
            return PromptAction::Cancel;
        }

        match key.code {
            KeyCode::Enter => {
                if self.select_mode && !self.filtered.is_empty() {
                    return PromptAction::SubmitItem(self.filtered[self.selected]);
                }
                return PromptAction::Submit;
            }
            // Ctrl-C cancels, like vim's `[Command-line]` default.
            KeyCode::Char('c') if control => return PromptAction::Cancel,
            KeyCode::Backspace | KeyCode::Char('h')
                if control || key.code == KeyCode::Backspace =>
            {
                self.backspace();
            }
            KeyCode::Delete | KeyCode::Char('d') if control || key.code == KeyCode::Delete => {
                self.delete_forward();
            }
            KeyCode::Char('u') if control => {
                self.set_text(String::new());
            }
            KeyCode::Char('w') if control || alt => self.delete_word(),
            KeyCode::Char('b') | KeyCode::Left if control || key.code == KeyCode::Left => {
                self.move_caret(-1)
            }
            KeyCode::Char('f') | KeyCode::Right if control || key.code == KeyCode::Right => {
                self.move_caret(1)
            }
            KeyCode::Char('a') | KeyCode::Home if control || key.code == KeyCode::Home => {
                self.caret = 0
            }
            KeyCode::Char('e') | KeyCode::End if control || key.code == KeyCode::End => {
                self.caret = self.text.chars().count()
            }
            KeyCode::Char('n') | KeyCode::Down if control || key.code == KeyCode::Down => {
                if self.select_mode {
                    self.select(1);
                } else {
                    self.recall_history(1);
                }
            }
            KeyCode::Char('p') | KeyCode::Up if control || key.code == KeyCode::Up => {
                if self.select_mode {
                    self.select(-1);
                } else {
                    self.recall_history(-1);
                }
            }
            KeyCode::Char('g') if control => return PromptAction::Cancel,
            KeyCode::Char('l') if control => {
                // Clear the filter but keep the prompt open (like `ctrl-l` in fzf).
                self.set_text(String::new());
            }
            KeyCode::Tab => self.complete(),
            KeyCode::BackTab => self.complete_previous(),
            KeyCode::Char(character) if !control && !alt => self.insert(character),
            KeyCode::Down | KeyCode::Up if self.select_mode => {
                self.select(if key.code == KeyCode::Down { 1 } else { -1 })
            }
            _ => {
                if let Some(character) = keys::typed_char(key) {
                    self.insert(character);
                }
            }
        }
        PromptAction::Continue
    }

    /// Tab: extend the text with the single completion, or the common prefix.
    fn complete(&mut self) {
        let candidates = self.completion_candidates();
        self.apply_completion(&candidates, 1);
    }

    fn complete_previous(&mut self) {
        let candidates = self.completion_candidates();
        self.apply_completion(&candidates, -1);
    }

    fn completion_candidates(&self) -> Vec<String> {
        match self.completion {
            Completion::None => Vec::new(),
            Completion::Command => {
                if self.text.contains(char::is_whitespace) {
                    let fragment = self.text.split_whitespace().next_back().unwrap_or("");
                    crate::command::complete_path(fragment)
                        .into_iter()
                        .map(|path| path.display().to_string())
                        .collect()
                } else {
                    crate::command::complete_command(&self.text)
                }
            }
            Completion::Path => crate::command::complete_path(&self.text)
                .into_iter()
                .map(|path| path.display().to_string())
                .collect(),
        }
    }

    fn apply_completion(&mut self, candidates: &[String], direction: isize) {
        if candidates.is_empty() {
            return;
        }
        // Cycling completions: remember what we completed to, then step.
        let base = self.text.clone();
        let matching: Vec<&String> = candidates
            .iter()
            .filter(|candidate| {
                candidate.starts_with(&base)
                    || strip_to_fragment(&base).starts_with(strip_to_fragment(candidate))
            })
            .collect();
        if matching.is_empty() {
            return;
        }
        static CYCLE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
        let index = if matching.len() == 1 {
            0
        } else {
            let current = CYCLE.load(std::sync::atomic::Ordering::Relaxed);
            let next = (current + direction).rem_euclid(matching.len() as isize);
            CYCLE.store(next, std::sync::atomic::Ordering::Relaxed);
            next as usize
        };
        let completed = matching[index].as_str();
        let fragment_start = base.len() - strip_to_fragment(&base).len();
        self.text = format!("{}{}", &base[..fragment_start], completed);
        self.caret = self.text.chars().count();
        if self.select_mode {
            self.refilter();
        }
    }
}

/// The part of a line after the last space or slash — the thing being completed.
fn strip_to_fragment(line: &str) -> &str {
    let split_at = line.rfind([' ', '\t']).map(|at| at + 1).unwrap_or(0);
    &line[split_at..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(character),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn typing_edits_the_line() {
        let mut prompt = Prompt::command("");
        for character in "write".chars() {
            prompt.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(prompt.text, "write");
        assert_eq!(prompt.caret(), 5);
        // One step left, so the new character lands inside the word.
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Char('!')));
        assert_eq!(prompt.text, "writ!e");
        assert_eq!(prompt.caret(), 5);
        prompt.handle_key(key(KeyCode::Backspace));
        assert_eq!(prompt.text, "write");
        assert_eq!(prompt.caret(), 4);
    }

    #[test]
    fn emacs_style_bindings_edit() {
        let mut prompt = Prompt::command("hello world");
        prompt.handle_key(ctrl('h'));
        assert_eq!(prompt.text, "hello worl");
        prompt.handle_key(ctrl('w'));
        assert_eq!(prompt.text, "hello ");
        prompt.handle_key(ctrl('a'));
        assert_eq!(prompt.caret(), 0);
        prompt.handle_key(ctrl('e'));
        assert_eq!(prompt.caret(), prompt.text.chars().count());
        prompt.handle_key(ctrl('u'));
        assert_eq!(prompt.text, "");
    }

    #[test]
    fn delete_forward_works() {
        let mut prompt = Prompt::command("abc");
        prompt.handle_key(ctrl('a'));
        prompt.handle_key(ctrl('d'));
        assert_eq!(prompt.text, "bc");
    }

    #[test]
    fn history_walks_back_and_forth() {
        let mut prompt = Prompt::command("").with_history(vec!["first".into(), "second".into()]);
        prompt.set_text("typed");
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.text, "second");
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.text, "first");
        prompt.handle_key(key(KeyCode::Down));
        assert_eq!(prompt.text, "second");
        prompt.handle_key(key(KeyCode::Down));
        assert_eq!(
            prompt.text, "typed",
            "walking past the end restores the text"
        );
    }

    #[test]
    fn remember_skips_blanks_and_duplicates() {
        let mut prompt = Prompt::command("");
        prompt.remember("one");
        prompt.remember("one");
        prompt.remember("  ");
        prompt.remember("two");
        assert_eq!(prompt.history(), ["one", "two"]);
    }

    #[test]
    fn picker_filters_and_moves_selection() {
        let items = vec![
            Item::new("src/main.rs"),
            Item::new("src/editor.rs"),
            Item::new("README.md"),
        ];
        let mut prompt = Prompt::picker("find", items);
        assert_eq!(prompt.visible().len(), 3);
        for character in "edrs".chars() {
            prompt.handle_key(key(KeyCode::Char(character)));
        }
        let visible = prompt.visible();
        assert_eq!(visible.len(), 1, "fuzzy filter should narrow to one file");
        assert_eq!(prompt.items[visible[0]].label, "src/editor.rs");
        match prompt.handle_key(key(KeyCode::Enter)) {
            PromptAction::SubmitItem(index) => {
                assert_eq!(prompt.items[index].label, "src/editor.rs")
            }
            other => panic!("expected a submission, got {other:?}"),
        }
    }

    #[test]
    fn picker_with_empty_text_keeps_everything() {
        let items = vec![Item::new("a"), Item::new("b")];
        let prompt = Prompt::picker("buffers", items);
        assert_eq!(prompt.visible().len(), 2);
    }

    #[test]
    fn engine_filtered_picker_shows_hits_whose_labels_do_not_match() {
        // The grep picker: the text is the search pattern, the hits are
        // `path:line` labels the pattern will not fuzzy-match.  Everything
        // streamed in must stay visible, in engine order.
        let mut prompt = Prompt::picker("grep", Vec::new());
        prompt.filter_items = false;
        prompt.set_text("Lookup");
        prompt.extend_items(vec![
            Item::new("src/app.rs:12").detail("let lookup = table;"),
            Item::new("src/prompt.rs:56").detail("fn match_key // Lookup"),
        ]);
        assert_eq!(prompt.visible().len(), 2);
        assert_eq!(prompt.items[prompt.visible()[0]].label, "src/app.rs:12");
        // Typing more of the pattern must not hide the already-shown hits.
        prompt.handle_key(key(KeyCode::Char('s')));
        assert_eq!(prompt.visible().len(), 2);
    }

    #[test]
    fn a_new_search_starts_from_an_empty_list() {
        // The grep picker shows exactly the hits for the pattern in the prompt.
        // When the pattern changes the old list goes first, otherwise every
        // broader prefix's hits stay on screen and the list is no longer a
        // result of the pattern that was typed.
        let mut prompt = Prompt::picker("grep", Vec::new());
        prompt.filter_items = false;
        prompt.set_text("loo");
        prompt.extend_items(vec![Item::new("src/app.rs:12"), Item::new("src/loop.rs:3")]);
        prompt.selected = 1;
        prompt.clear_items();
        assert!(prompt.visible().is_empty(), "the old hits must go");
        assert_eq!(prompt.selected, 0);
        prompt.extend_items(vec![Item::new("src/app.rs:12")]);
        assert_eq!(prompt.visible().len(), 1);
        assert_eq!(prompt.selected, 0);
    }

    #[test]
    fn escape_and_ctrl_c_cancel() {
        let mut prompt = Prompt::command("x");
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), PromptAction::Cancel);
        assert_eq!(prompt.handle_key(ctrl('c')), PromptAction::Cancel);
        assert_eq!(prompt.handle_key(ctrl('g')), PromptAction::Cancel);
    }

    #[test]
    fn plain_enter_submits_the_text() {
        let mut prompt = Prompt::command("w");
        assert_eq!(prompt.handle_key(key(KeyCode::Enter)), PromptAction::Submit);
    }

    #[test]
    fn items_carry_payloads() {
        let item = Item::new("note.rs").payload((7usize, "note.rs".to_string()));
        let payload = item.payload_as::<(usize, String)>().expect("payload");
        assert_eq!(payload.0, 7);
        assert!(item.payload_as::<u32>().is_none());
    }
}
