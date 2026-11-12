//! The application shell: terminal setup, the async event loop, intents and
//! background jobs.
//!
//! `App` owns the terminal, the [`Editor`] and the prompt.  Keys go to the editor,
//! the editor returns [`Intent`]s, and this module is the only place that touches
//! the file system, spawns processes or draws.  Background work reports through
//! the same event channel as keystrokes, so a slow formatter can never block a key
//! press, and a stale result is dropped by comparing buffer versions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use ratatui::layout::Rect;

use crate::action::Cmd;
use crate::buffer::BufferId;
use crate::command::{self, Command, ConfigAction, SetOutcome, SplitDirection};
use crate::config::Background;
use crate::config::Config;
use crate::diagnostic::DiagnosticSource;
use crate::editor::{Editor, Intent, PromptSpec, StatusLevel};
use crate::event::{AsyncEvent, Event, Events};
use crate::format::{self, Formatter};
use crate::input::Keymaps;
use crate::input::Mode;
use crate::layout::Axis;
use crate::lsp::{self, Clients, LspEvent, ServerSpec};
use crate::prompt::{self, Prompt, PromptAction};
use crate::search::{self, SearchEvent, SearchHandle, SearchKind};
use crate::spell::{self, SpellResult};
use crate::status::Snapshot;
use crate::syntax::{self, TreeSlot};
use crate::term::{self, Terminal, TerminalGuard};
use crate::theme::{Observed, Theme};
use crate::ui::{self, CommandLine, Picker, Screen};

/// Idle time before a buffer is re-parsed.
const PARSE_IDLE: Duration = Duration::from_millis(120);
/// Idle time before a buffer is spell checked.
const SPELL_IDLE: Duration = Duration::from_millis(1_500);
/// Idle time before an auto-format runs.
const FORMAT_FALLBACK_IDLE: Duration = Duration::from_millis(500);
/// Idle time before a completion request goes out, so typing a word sends one
/// request rather than one per character.
const COMPLETION_IDLE: Duration = Duration::from_millis(90);
/// How long the caret must rest on a diagnostic before its message is shown.
const DIAGNOSTIC_HOLD: Duration = Duration::from_millis(500);

/// What the open prompt collects; drives how submission is interpreted.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptKind {
    Command,
    Search { forward: bool },
    Path,
    GotoLine,
    FindFiles,
    Grep,
    SwitchBuffer,
    Palette,
    Spell { span: crate::selection::Span },
    Diagnostics,
    /// Quick fixes offered by the language server.
    CodeActions,
}

/// A hit picked from a search list.
#[derive(Debug, Clone)]
pub struct HitPayload {
    pub path: PathBuf,
    pub line: usize,
}

/// A diagnostic row picked from `:error`-style list.
#[derive(Debug, Clone)]
pub struct DiagnosticPayload {
    pub buffer: BufferId,
    pub offset: usize,
}

/// Payload of the buffer picker.
#[derive(Debug, Clone, Copy)]
pub struct BufferPayload(pub BufferId);

/// Payload of a palette entry: the command line to run.
#[derive(Debug, Clone)]
pub struct CommandPayload(pub String);

/// Payload of a spelling suggestion.
#[derive(Debug, Clone)]
pub struct WordPayload(pub String);

/// Payload of a quick-fix row: which action in the pending list it is.
#[derive(Debug, Clone, Copy)]
pub struct CodeActionPayload {
    pub buffer: BufferId,
    pub index: usize,
}

pub struct App {
    pub editor: Editor,
    pub theme: Theme,
    /// The terminal's own colours, measured once at startup.  Kept so that
    /// `:set theme.background=…` can rebuild the palette on the spot.
    observed: Observed,
    pub keymaps: Keymaps,
    pub events: Events,
    pub terminal: Terminal,
    pub _terminal_guard: TerminalGuard,
    pub prompt: Option<(PromptKind, Prompt)>,
    pub message: Option<(StatusLevel, String)>,
    pub message_expiry: Option<Instant>,
    pub cwd: PathBuf,
    pub config_path: Option<PathBuf>,
    pub command_history: Vec<String>,
    pub search_history: Vec<String>,
    pub search_job: Option<SearchHandle>,
    pub search_root: PathBuf,
    pub clients: Clients,
    pub quit: bool,
    needs_redraw: bool,
    last_key_at: Option<Instant>,
    versions: HashMap<BufferId, u64>,
    last_change: HashMap<BufferId, Instant>,
    spell_checked: HashMap<BufferId, u64>,
    /// The open completion popup, if any.
    pub completion: Option<crate::completion::Completion>,
    /// The next request id, shared by every kind of LSP request so a late
    /// answer can be matched to the request that asked for it.
    next_request: i64,
    /// The request we are waiting for: `(id, prefix start, buffer)`.
    completion_pending: Option<(i64, usize, BufferId)>,
    /// When the next completion request is due, for debouncing keystrokes.
    completion_due: Option<Instant>,
    /// The hover text to show, and the request it answers.
    pub hover: Option<String>,
    /// The hover request in flight: `(id, buffer)`.
    hover_pending: Option<(i64, BufferId)>,
    /// The LSP format request in flight: `(id, buffer)`.
    format_pending: Option<(i64, BufferId)>,
    /// The code-action request in flight: `(id, buffer)`.
    code_action_pending: Option<(i64, BufferId)>,
    /// The quick fixes the last code-action answer offered, indexed by the
    /// picker's payload.
    code_actions: Vec<lsp::CodeAction>,
    /// When the caret came to rest on the diagnostic it is currently on, so the
    /// float only appears after a deliberate pause.
    diagnostic_hold: Option<(usize, Instant)>,
    /// Whether the float is currently on screen, so the tick that makes it due
    /// can trigger exactly one redraw.
    diagnostic_float_shown: bool,
    last_frame: Rect,
}

impl App {
    /// Set up the terminal and open the files named on the command line.
    pub fn start(
        config: Config,
        config_path: Option<PathBuf>,
        files: &[PathBuf],
        line_hint: Option<usize>,
    ) -> crate::error::Result<Self> {
        let mouse = config.editor.mouse;
        let guard = term::enter_terminal(mouse)?;
        let terminal = term::terminal()?;
        // Ask the terminal how dark it is while we still own the keyboard.
        let observed = if config.theme.background == Background::Auto {
            term::query_colors(term::COLOR_QUERY_TIMEOUT)
        } else {
            Observed::UNKNOWN
        };
        let theme = Theme::with_observed(&config.theme, observed);
        let (keymaps, keymap_warnings) = Keymaps::build(&config);
        let events = Events::start();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let search_root = search::project_root(files.first().map(Path::new));

        let mut editor = Editor::new(config);
        for path in files {
            editor.open_file(path);
        }
        if editor.buffers.is_empty() {
            editor.new_scratch();
        }
        if let Some(line) = line_hint {
            editor.goto_line(line);
        }

        let mut app = Self {
            editor,
            theme,
            observed,
            keymaps,
            events,
            terminal,
            _terminal_guard: guard,
            prompt: None,
            message: None,
            message_expiry: None,
            cwd,
            config_path,
            command_history: Vec::new(),
            search_history: Vec::new(),
            search_job: None,
            search_root,
            clients: Clients::new(),
            quit: false,
            needs_redraw: true,
            last_key_at: None,
            versions: HashMap::new(),
            last_change: HashMap::new(),
            spell_checked: HashMap::new(),
            completion: None,
            next_request: 1,
            completion_pending: None,
            completion_due: None,
            hover: None,
            hover_pending: None,
            format_pending: None,
            code_action_pending: None,
            code_actions: Vec::new(),
            diagnostic_hold: None,
            diagnostic_float_shown: false,
            last_frame: Rect::new(0, 0, 80, 24),
        };
        // Initial background work: parse, and check spelling if enabled.
        let ids: Vec<BufferId> = app
            .editor
            .buffers()
            .iter()
            .map(|buffer| buffer.id)
            .collect();
        for id in ids {
            app.spawn_parse(id);
            if app.editor.config.spell.enable {
                app.spawn_spell(id);
            }
            app.notify_lsp_open(id);
        }
        for warning in keymap_warnings.iter().take(3) {
            app.set_message(StatusLevel::Warn, warning.clone());
        }
        Ok(app)
    }

    /// Tear the terminal back down.
    pub fn shutdown(mut self) {
        self.clients.shutdown_all();
        if let Some(handle) = self.search_job.take() {
            handle.cancel();
        }
        self.events.shutdown();
        term::leave_terminal();
    }

    // -------------------------------------------------------------- the loop

    /// Run until quit.
    pub async fn run(&mut self) -> crate::error::Result<()> {
        let mut ticker = tokio::time::interval(Duration::from_millis(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut receiver = self
            .events
            .take_receiver()
            .ok_or_else(|| crate::error::Error::Terminal("events already taken".into()))?;
        loop {
            if self.quit {
                break;
            }
            tokio::select! {
                event = receiver.recv() => {
                    match event {
                        Some(event) => self.handle_event(event),
                        // The reader thread stopped: shut down rather than spin.
                        None => break,
                    }
                }
                _ = ticker.tick() => self.on_tick(),
            }
            if self.needs_redraw {
                self.draw()?;
                self.needs_redraw = false;
            }
        }
        self.events.put_receiver(receiver);
        Ok(())
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Paste(text) => self.on_paste(&text),
            Event::Resize { .. } => {
                self.needs_redraw = true;
            }
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Tick => self.on_tick(),
            Event::Async(event) => self.on_async(event),
            Event::Quit => self.quit = true,
        }
    }

    fn on_tick(&mut self) {
        let mut wake = false;
        let now = Instant::now();

        // An idle key sequence resolves on its own: a bound key runs even while
        // a longer binding starts with it (`w` with `wd` bound), and a prefix
        // that binds nothing — a stray `g` — is dropped so it cannot swallow
        // the next key.
        if self.editor.pending.prefix_expired() {
            let area = self.last_frame;
            let intents = self.editor.expire_pending(area);
            self.apply_intents(intents);
            wake = true;
        }
        // Messages fade out.
        if self.message_expiry.is_some_and(|expiry| now >= expiry) {
            self.message = None;
            self.message_expiry = None;
            wake = true;
        }

        self.sync_versions();

        // Background work, one buffer per tick so the loop stays responsive.
        if let Some(id) = self.needs_parse() {
            self.spawn_parse(id);
        }
        if let Some(id) = self.needs_spell() {
            self.spawn_spell(id);
        }
        if let Some(id) = self.needs_format() {
            self.editor.format_due = None;
            self.spawn_format(id);
        }

        // A format request that was waiting for the idle timer.  It runs on the
        // buffer that was edited, not on whichever buffer is on screen when the
        // timer expires: formatting the wrong buffer rewrote text the user never
        // touched and parked its caret somewhere else.
        if let Some((due_buffer, due)) = self.editor.format_due {
            if now >= due {
                self.editor.format_due = None;
                self.spawn_format(due_buffer);
            }
        }

        // A completion request that was waiting for the typing to settle.
        if self.completion_due.is_some() {
            self.flush_completion();
        }

        // The diagnostic float appears only once the caret has rested on the
        // same diagnostic for a moment, so moving through a file does not flash
        // a card on every line.
        if self.update_diagnostic_hold(now) {
            wake = true;
        }

        self.needs_redraw |= wake;
    }

    /// Track how long the caret has rested on the diagnostic it is on.
    ///
    /// Returns true when the float should appear or disappear, so the caller
    /// knows to redraw.  The hold is keyed by the diagnostic's start offset, so
    /// moving to a different diagnostic restarts the clock.
    fn update_diagnostic_hold(&mut self, now: Instant) -> bool {
        let wanted = if self.editor.config.editor.diagnostic_float
            && self.prompt.is_none()
            && self.completion.is_none()
        {
            self.diagnostic_under_cursor()
                .map(|diagnostic| diagnostic.range.start)
        } else {
            None
        };
        let Some(at) = wanted else {
            let was_shown = self.diagnostic_float_shown;
            self.diagnostic_hold = None;
            self.diagnostic_float_shown = false;
            return was_shown;
        };
        match self.diagnostic_hold {
            Some((held, since)) if held == at => {
                // Same diagnostic: show the float once the hold has elapsed.
                // Without this the card would only appear on the next unrelated
                // redraw, which may never come while the caret sits still.
                if !self.diagnostic_float_shown && since.elapsed() >= DIAGNOSTIC_HOLD {
                    self.diagnostic_float_shown = true;
                    return true;
                }
                false
            }
            _ => {
                // A different diagnostic (or the first one): restart the clock
                // and take down any card that was showing.
                let was_shown = self.diagnostic_float_shown;
                self.diagnostic_hold = Some((at, now));
                self.diagnostic_float_shown = false;
                was_shown
            }
        }
    }

    /// The diagnostic the caret sits on, if any.
    fn diagnostic_under_cursor(&self) -> Option<&crate::diagnostic::Diagnostic> {
        let buffer = self.editor.current_buffer()?;
        let cursor = self.editor.cursor();
        buffer
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.range.contains(cursor))
    }

    /// Notice buffer edits, so parsing, spell checking and LSP sync follow them.
    fn sync_versions(&mut self) {
        let now = Instant::now();
        let buffers: Vec<(BufferId, u64)> = self
            .editor
            .buffers()
            .iter()
            .map(|buffer| (buffer.id, buffer.version))
            .collect();
        for (id, version) in buffers {
            let previous = self.versions.insert(id, version);
            if previous.is_some_and(|previous| previous != version) {
                self.last_change.insert(id, now);
                self.editor.schedule_auto_format(id);
                // The server has to see the edit before it can answer about it.
                self.notify_lsp_change(id);
            } else if previous.is_none() {
                self.versions.insert(id, version);
                self.last_change.insert(id, now);
            }
        }
        self.versions
            .retain(|id, _| self.editor.buffers().iter().any(|buffer| buffer.id == *id));
    }

    fn idle_long_enough(&self, id: BufferId, idle: Duration) -> bool {
        self.last_change
            .get(&id)
            .is_none_or(|at| at.elapsed() >= idle)
    }

    fn needs_parse(&self) -> Option<BufferId> {
        self.editor.buffers().iter().find_map(|buffer| {
            (buffer.language.has_tree_sitter()
                && buffer.syntax.parse_due(buffer.version)
                && self.idle_long_enough(buffer.id, PARSE_IDLE))
            .then_some(buffer.id)
        })
    }

    fn needs_spell(&self) -> Option<BufferId> {
        if !self.editor.config.spell.enable {
            return None;
        }
        self.editor.buffers().iter().find_map(|buffer| {
            let checked = self.spell_checked.get(&buffer.id).copied().unwrap_or(0);
            (checked != buffer.version && self.idle_long_enough(buffer.id, SPELL_IDLE))
                .then_some(buffer.id)
        })
    }

    fn needs_format(&self) -> Option<BufferId> {
        // Only `[format] on-save` and `:format` are automatic: idle formatting is
        // driven by `editor.format_due`, set when a change settles down.
        let _ = FORMAT_FALLBACK_IDLE;
        None
    }

    // ------------------------------------------------------------------ keys

    fn on_key(&mut self, key: KeyEvent) {
        // Only press/release matter; repeats arrive as press events anyway.
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.last_key_at = Some(Instant::now());
        // Any key clears a shown message, like vim.
        if self.message.is_some() {
            self.message = None;
            self.message_expiry = None;
        }
        // A hover card is dismissed by the next key, the way a tooltip is: it
        // is an answer to one question, not a mode.
        if self.hover.take().is_some() {
            self.needs_redraw = true;
        }

        if self.prompt.is_some() {
            let kind = self
                .prompt
                .as_ref()
                .map(|(kind, _)| kind.clone())
                .expect("prompt present");
            let before = self
                .prompt
                .as_ref()
                .map(|(_, prompt)| prompt.text.clone())
                .expect("prompt present");
            let outcome = {
                let (_, prompt) = self.prompt.as_mut().expect("prompt present");
                prompt.handle_key(key)
            };
            // Live reactions (re-search, highlight) belong to text edits only.
            // Navigation keys also return `Continue`, and re-running the grep
            // on every Down press would clear the list and the selection the
            // user is trying to move through.
            let edited = self
                .prompt
                .as_ref()
                .is_some_and(|(_, prompt)| prompt.text != before);
            match outcome {
                PromptAction::Continue if edited => {
                    self.on_prompt_edited(kind);
                }
                PromptAction::Continue => {}
                PromptAction::Submit => self.submit_prompt(None),
                PromptAction::SubmitItem(index) => self.submit_prompt(Some(index)),
                PromptAction::Cancel => {
                    self.cancel_prompt();
                }
            }
            self.needs_redraw = true;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.editor.pending.clear();
            self.editor.leave_insert();
            self.editor.leave_visual();
            self.completion = None;
            self.needs_redraw = true;
            return;
        }

        // The completion popup gets first refusal on Tab/Enter/Esc/arrows; any
        // other key falls through to the editor so typing narrows the list.
        if self.completion.is_some() && self.completion_key(key) {
            return;
        }

        let area = self.last_frame;
        let intents = self.editor.handle_key(key, &self.keymaps, area);
        self.apply_intents(intents);
        self.refresh_completion();
        self.schedule_completion();
        self.needs_redraw = true;
    }

    fn on_paste(&mut self, text: &str) {
        if let Some((_, prompt)) = self.prompt.as_mut() {
            prompt.set_text(format!("{}{text}", prompt.text));
            self.needs_redraw = true;
            return;
        }
        if self.editor.mode == Mode::Insert {
            for intent in self.editor.paste_text(text) {
                self.apply_intents(vec![intent]);
            }
        } else {
            self.set_message(
                StatusLevel::Info,
                format!(
                    "{} bytes pasted (paste inserts only in insert mode)",
                    text.len()
                ),
            );
        }
        self.needs_redraw = true;
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.prompt.is_some() {
            if let Some((_, prompt)) = self.prompt.as_mut() {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        prompt.handle_key(key(KeyCode::Up));
                    }
                    MouseEventKind::ScrollDown => {
                        prompt.handle_key(key(KeyCode::Down));
                    }
                    _ => {}
                }
            }
            self.needs_redraw = true;
            return;
        }
        let area = self.last_frame;
        let pane_area = self.pane_area(area);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_lines(mouse.column, mouse.row, -3);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_lines(mouse.column, mouse.row, 3);
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.click(mouse.column, mouse.row, pane_area, false);
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                self.click(mouse.column, mouse.row, pane_area, true);
            }
            _ => {}
        }
        self.needs_redraw = true;
    }

    fn scroll_lines(&mut self, column: u16, row: u16, delta: i32) {
        let area = self.last_frame;
        let pane_area = self.pane_area(area);
        if let Some(pane_id) = self.pane_at(column, row, pane_area) {
            self.editor.layout.focus(pane_id);
            self.editor
                .layout
                .on_focus_changed(&self.editor.config.layout);
        }
        let focused = match self.editor.layout.focused() {
            Some(id) => id,
            None => return,
        };
        let max = self
            .editor
            .current_buffer()
            .map(|buffer| buffer.last_line() as i32)
            .unwrap_or(0);
        let current = self
            .editor
            .layout
            .pane(focused)
            .map(|pane| pane.view.scroll_line as i32)
            .unwrap_or(0);
        if let Some(pane) = self.editor.layout.pane_mut(focused) {
            pane.view.scroll_line = (current + delta).clamp(0, max) as usize;
        }
    }

    /// Focus a pane and move the cursor (or extend the selection on a drag).
    fn click(&mut self, column: u16, row: u16, pane_area: Rect, drag: bool) {
        let Some(pane_id) = self.pane_at(column, row, pane_area) else {
            return;
        };
        let focused_now = self.editor.layout.focused() == Some(pane_id);
        self.editor.layout.focus(pane_id);
        self.editor
            .layout
            .on_focus_changed(&self.editor.config.layout);
        let Some(pane_rect) = self.pane_rect(pane_id, pane_area) else {
            return;
        };
        let (buffer, pane) = match (
            self.editor.current_buffer(),
            self.editor.layout.pane(pane_id),
        ) {
            (Some(buffer), Some(pane)) => (buffer, pane),
            _ => return,
        };
        if let Some(offset) = ui::byte_at(
            buffer,
            pane_rect,
            &pane.view,
            &pane.selection,
            &self.editor.config,
            column,
            row,
        ) {
            let selection = if drag && self.editor.mode.is_visual() {
                let mut selection = self.editor.selection();
                selection.extend_to(offset);
                selection
            } else {
                crate::selection::Selection::point(offset)
            };
            let _ = focused_now;
            self.editor.set_selection(selection);
        }
    }

    fn pane_at(&self, column: u16, row: u16, pane_area: Rect) -> Option<crate::layout::PaneId> {
        self.editor
            .layout
            .rects(pane_area)
            .into_iter()
            .find(|(_, rect)| rect.contains(Position { x: column, y: row }))
            .map(|(id, _)| id)
    }

    fn pane_rect(&self, pane_id: crate::layout::PaneId, pane_area: Rect) -> Option<Rect> {
        self.editor
            .layout
            .rects(pane_area)
            .into_iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, rect)| rect)
    }

    // --------------------------------------------------------------- intents

    fn apply_intents(&mut self, intents: Vec<Intent>) {
        for intent in intents {
            match intent {
                Intent::Message(level, text) => self.set_message(level, text),
                Intent::OpenPrompt(spec) => self.open_prompt(spec),
                Intent::RunCommand(line) => {
                    let parsed = command::parse(line.trim_start_matches(':'));
                    self.run_command(parsed);
                }
                Intent::Parse(id) => self.spawn_parse(id),
                Intent::Format(id) => self.spawn_format(id),
                Intent::RangeFormat(id, span) => self.spawn_range_format(id, span),
                Intent::Hover => self.request_hover(),
                Intent::CodeAction => self.request_code_actions(),
                Intent::Spell(id) => self.spawn_spell(id),
                Intent::FindFiles(query) => {
                    let kind = PromptKind::FindFiles;
                    self.open_picker(kind, "find", &query, Vec::new());
                    self.start_search(SearchKind::FileNames, query);
                }
                Intent::Grep(query) => {
                    let kind = PromptKind::Grep;
                    self.open_picker(kind.clone(), "grep", &query.pattern, Vec::new());
                    self.disable_local_filter();
                    self.start_search(SearchKind::Contents, query.pattern);
                }
                Intent::Clipboard(text) => {
                    term::set_clipboard(&text);
                }
                Intent::EnsureVisible => {
                    // The editor resolves the focused pane itself; handing it a
                    // pre-cut pane area would subtract the chrome twice.
                    let area = self.last_frame;
                    self.editor.ensure_visible(area);
                }
                Intent::ReloadConfig => self.reload_config(),
                Intent::Quit => self.quit = true,
            }
        }
    }

    /// Surface a configuration warning from startup without hiding the fact
    /// that the editor is usable.
    pub fn set_startup_warning(&mut self, text: String) {
        self.set_message(StatusLevel::Warn, text);
    }

    fn set_message(&mut self, level: StatusLevel, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            self.message = None;
            self.message_expiry = None;
            return;
        }
        let timeout = self.editor.config.status.message_timeout;
        self.message = Some((level, text));
        self.message_expiry = if timeout.is_disabled() {
            None
        } else {
            Some(Instant::now() + timeout.to_std())
        };
    }

    // --------------------------------------------------------------- prompts

    fn open_prompt(&mut self, spec: PromptSpec) {
        match spec {
            PromptSpec::Command(initial) => {
                let mut prompt =
                    Prompt::command(initial).with_history(self.command_history.clone());
                prompt.remember("");
                self.prompt = Some((PromptKind::Command, prompt));
            }
            PromptSpec::Search { forward, initial } => {
                let mut prompt = Prompt::search(forward, initial);
                let history = self.search_history.clone();
                prompt = prompt.with_history(history);
                self.prompt = Some((PromptKind::Search { forward }, prompt));
            }
            PromptSpec::Path { initial } => {
                let prompt = Prompt::new("write ")
                    .with_completion(prompt::Completion::Path)
                    .with_history(self.command_history.clone());
                let mut prompt = prompt;
                prompt.set_text(initial);
                self.prompt = Some((PromptKind::Path, prompt));
            }
            PromptSpec::GotoLine => {
                self.prompt = Some((PromptKind::GotoLine, Prompt::new("line ")));
            }
            PromptSpec::FindFiles(query) => {
                self.open_picker(PromptKind::FindFiles, "find", &query, Vec::new());
                self.start_search(SearchKind::FileNames, query);
            }
            PromptSpec::Grep(query) => {
                self.open_picker(PromptKind::Grep, "grep", &query, Vec::new());
                self.disable_local_filter();
                self.start_search(SearchKind::Contents, query);
            }
            PromptSpec::SwitchBuffer => {
                let items = self.buffer_items();
                self.open_picker(PromptKind::SwitchBuffer, "buffers", "", items);
            }
            PromptSpec::Palette(query) => {
                let items = self.palette_items();
                self.open_picker(PromptKind::Palette, "commands", &query, items);
            }
            PromptSpec::Spell {
                word,
                span,
                suggestions,
            } => {
                let items: Vec<prompt::Item> = suggestions
                    .into_iter()
                    .map(|suggestion| {
                        prompt::Item::new(suggestion.clone()).payload(WordPayload(suggestion))
                    })
                    .collect();
                let mut picker = Prompt::picker(format!("fix \"{word}\""), items);
                picker.smart_case = false;
                self.prompt = Some((PromptKind::Spell { span }, picker));
            }
            PromptSpec::Diagnostics => {
                let items = self.diagnostic_items();
                self.open_picker(PromptKind::Diagnostics, "diagnostics", "", items);
            }
            PromptSpec::Substitute(_) => {
                self.set_message(
                    StatusLevel::Warn,
                    ":substitute is not implemented; use :grep to find and edit by hand",
                );
            }
        }
        self.needs_redraw = true;
    }

    fn open_picker(
        &mut self,
        kind: PromptKind,
        title: &str,
        query: &str,
        items: Vec<prompt::Item>,
    ) {
        let mut picker = Prompt::picker(title, items);
        picker.set_text(query);
        self.prompt = Some((kind, picker));
    }

    /// The grep picker's text is the engine's pattern, not a local filter:
    /// hit labels are `path:line` and would all be fuzzy-filtered away.
    fn disable_local_filter(&mut self) {
        if let Some((_, prompt)) = self.prompt.as_mut() {
            prompt.filter_items = false;
            prompt.refilter();
        }
    }

    fn cancel_prompt(&mut self) {
        if let Some((kind, _)) = self.prompt.take() {
            if matches!(kind, PromptKind::FindFiles | PromptKind::Grep)
                && let Some(handle) = self.search_job.take()
            {
                handle.cancel();
            }
            if let PromptKind::Search { .. } = kind {
                // `/` cancelled: vim keeps the previous term but drops the highlight.
                self.editor.refresh_search_matches();
            }
        }
        self.needs_redraw = true;
    }

    /// Live reactions to typing in a prompt: search highlighting, new searches.
    fn on_prompt_edited(&mut self, kind: PromptKind) {
        let text = self
            .prompt
            .as_ref()
            .map(|(_, prompt)| prompt.text.clone())
            .unwrap_or_default();
        match kind {
            PromptKind::Search { .. } => {
                self.editor.set_search(&text);
                self.editor.refresh_search_matches();
            }
            PromptKind::Grep => self.start_search(SearchKind::Contents, text),
            PromptKind::FindFiles => {
                // The file list is fetched once; the picker filters it locally.
            }
            PromptKind::Command
            | PromptKind::Path
            | PromptKind::GotoLine
            | PromptKind::SwitchBuffer
            | PromptKind::Palette
            | PromptKind::Spell { .. }
            | PromptKind::Diagnostics
            | PromptKind::CodeActions => {}
        }
        self.needs_redraw = true;
    }

    /// Enter was pressed.  `item` is the index into the picker's items, if any.
    fn submit_prompt(&mut self, item: Option<usize>) {
        let Some((kind, prompt)) = self.prompt.take() else {
            return;
        };
        let text = prompt.display_text();
        match kind {
            PromptKind::Command => {
                let line = text.trim_start().to_string();
                if !line.is_empty() {
                    push_history(&mut self.command_history, line.clone());
                }
                let parsed = command::parse(&line);
                self.run_command(parsed);
            }
            PromptKind::Search { forward } => {
                let term = text.clone();
                if !term.is_empty() {
                    push_history(&mut self.search_history, term.clone());
                }
                self.editor.set_search(&term);
                if !term.is_empty() {
                    let area = self.last_frame;
                    let _ = area;
                    let intents = self.editor.search_next(!forward, 1);
                    self.apply_intents(intents);
                }
            }
            PromptKind::Path => {
                let path = expand(&text);
                if path.as_os_str().is_empty() {
                    self.set_message(StatusLevel::Warn, "no path given");
                } else {
                    let intent = self.editor.write_to(&path);
                    self.apply_intents(vec![intent]);
                }
            }
            PromptKind::GotoLine => match text.trim().parse::<usize>() {
                Ok(line) => {
                    let intents = self.editor.goto_line(line);
                    self.apply_intents(intents);
                }
                Err(_) => {
                    self.set_message(StatusLevel::Error, format!("not a line number: {text}"))
                }
            },
            PromptKind::FindFiles | PromptKind::Grep => {
                let index = item.or_else(|| self.prompt_index(&prompt));
                let payload = index
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<HitPayload>())
                    .cloned();
                if let Some(payload) = payload {
                    self.open_hit(&payload);
                } else if text.trim().is_empty() {
                    self.set_message(StatusLevel::Warn, "nothing selected");
                } else {
                    // Enter with text and no candidate: search the project's
                    // contents for it.  This opens the grep picker rather than
                    // only starting a job — with the prompt closed here, the
                    // results of a bare `start_search` would be dropped as
                    // belonging to no open picker.
                    self.open_prompt(PromptSpec::Grep(text));
                }
            }
            PromptKind::SwitchBuffer => {
                let id = item
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<BufferPayload>())
                    .map(|payload| payload.0);
                match id {
                    Some(id) => {
                        self.editor.focus_buffer(id);
                    }
                    None => self.set_message(StatusLevel::Warn, "no buffer selected"),
                }
            }
            PromptKind::Palette => {
                let line = item
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<CommandPayload>())
                    .map(|payload| payload.0.clone());
                match line {
                    Some(line) => {
                        let parsed = command::parse(line.trim_start_matches(':'));
                        self.run_command(parsed);
                    }
                    None => self.set_message(StatusLevel::Warn, "no command selected"),
                }
            }
            PromptKind::Spell { span } => {
                let word = item
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<WordPayload>())
                    .map(|payload| payload.0.clone());
                match word {
                    Some(word) => {
                        let intents = self.editor.replace_span(span, &word);
                        self.apply_intents(intents);
                    }
                    None => self.set_message(StatusLevel::Warn, "no suggestion selected"),
                }
            }
            PromptKind::Diagnostics => {
                let payload = item
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<DiagnosticPayload>())
                    .cloned();
                match payload {
                    Some(payload) => {
                        self.editor.focus_buffer(payload.buffer);
                        self.editor.set_cursor(payload.offset);
                        self.apply_intents(vec![Intent::EnsureVisible]);
                    }
                    None => self.set_message(StatusLevel::Warn, "no diagnostic selected"),
                }
            }
            PromptKind::CodeActions => {
                let payload = item
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<CodeActionPayload>())
                    .copied();
                match payload {
                    Some(payload) => self.apply_code_action(payload),
                    None => self.set_message(StatusLevel::Warn, "no quick fix selected"),
                }
            }
        }
        self.needs_redraw = true;
    }

    /// Apply the quick fix a picker row refers to.
    fn apply_code_action(&mut self, payload: CodeActionPayload) {
        let Some(action) = self.code_actions.get(payload.index) else {
            self.set_message(StatusLevel::Warn, "that quick fix is gone");
            return;
        };
        let title = action.title.clone();
        let edits = action.edits.clone();
        // A quick fix is a change the user chose, so it gets its own undo step
        // rather than folding into whatever they typed before.
        let applied = self
            .editor
            .apply_edits(payload.buffer, &edits, crate::buffer::ChangeGroup::Command);
        if applied == 0 {
            self.set_message(StatusLevel::Warn, "the quick fix changed nothing");
            return;
        }
        self.set_message(StatusLevel::Info, title);
        self.apply_intents(vec![Intent::EnsureVisible]);
    }

    fn prompt_index(&self, prompt: &Prompt) -> Option<usize> {
        if !prompt.select_mode || prompt.visible().is_empty() {
            return None;
        }
        prompt.visible().get(prompt.selected).copied()
    }

    // ------------------------------------------------------------- commands

    fn run_command(&mut self, command: Command) {
        match command {
            Command::Editor(cmd) => {
                let area = self.last_frame;
                let intents = self.editor.run_cmd(cmd, area);
                self.apply_intents(intents);
            }
            Command::Open { path, split } => {
                if path.is_empty() {
                    self.open_prompt(PromptSpec::Path {
                        initial: String::new(),
                    });
                    return;
                }
                let path = expand(&path);
                let id = match split {
                    None => self.editor.open_file(&path),
                    Some(SplitDirection::Row) => self.editor.open_file_split(&path, Axis::Row),
                    Some(SplitDirection::Column) => {
                        self.editor.open_file_split(&path, Axis::Column)
                    }
                };
                self.spawn_parse(id);
                if self.editor.config.spell.enable {
                    self.spawn_spell(id);
                }
                self.notify_lsp_open(id);
                if let Some(buffer) = self.editor.buffer(id) {
                    self.set_message(
                        StatusLevel::Info,
                        format!("{} lines", buffer.display_line_count()),
                    );
                }
            }
            Command::NewBuffer => {
                self.editor.new_scratch();
                if let Some(id) = self.editor.current_buffer_id() {
                    self.editor.show_in_pane(id, None);
                }
            }
            Command::Write { path, force } => {
                let target = path.as_deref().map(expand);
                if target.is_none() && !force && self.file_changed_on_disk() {
                    self.set_message(
                        StatusLevel::Error,
                        "the file changed on disk; :write! to overwrite anyway",
                    );
                    return;
                }
                // A save under a new name adopts that path: the language server
                // has never seen the new URI, so it will need an open rather
                // than only the save below.  Compute this before the write,
                // which is what rewrites the buffer's path.
                let renamed = match (&target, self.editor.current_buffer_id()) {
                    (Some(path), Some(id)) => {
                        self.editor.buffer(id).and_then(|b| b.path()) != Some(path.as_path())
                    }
                    _ => false,
                };
                if let Some(id) = self.editor.current_buffer_id() {
                    self.notify_lsp_save(id);
                }
                let intent = match &target {
                    Some(path) => Some(self.editor.write_to(path)),
                    None => self.editor.save_now(),
                };
                if let Some(intent) = intent {
                    self.apply_intents(vec![intent]);
                }
                if renamed {
                    if let Some(id) = self.editor.current_buffer_id() {
                        self.notify_lsp_open(id);
                    }
                }
                // `[format] on-save` formats before the write settles.
                if self.editor.config.format.on_save && !force {
                    if let Some(id) = self.editor.current_buffer_id() {
                        self.spawn_format(id);
                    }
                }
            }
            Command::WriteAll | Command::WriteQuit => {
                let area = self.last_frame;
                let intents = self.editor.run_cmd(Cmd::SaveAll, area);
                self.apply_intents(intents);
                if matches!(command, Command::WriteQuit) {
                    self.quit = true;
                }
            }
            Command::Buffer { target, next } => self.select_buffer(&target, next),
            Command::BufferDelete { target, force } => self.delete_buffer(&target, force),
            Command::BufferList => {
                let items = self.buffer_items();
                self.open_picker(PromptKind::SwitchBuffer, "buffers", "", items);
            }
            Command::Set { arguments } => self.run_set(&arguments),
            Command::ListMaps { modes } => {
                let items: Vec<prompt::Item> = self
                    .keymaps
                    .describe()
                    .into_iter()
                    .filter(|(context, _, _)| {
                        modes.is_empty() || modes.iter().any(|m| m == context)
                    })
                    .map(|(context, keys, action)| {
                        prompt::Item::new(format!("{context}: {keys}")).detail(action)
                    })
                    .collect();
                self.open_picker(PromptKind::Palette, "key bindings", "", items);
            }
            Command::Unmap { lhs, mode } => {
                let context = context_name(mode);
                let removed = self.editor.config.remove_binding(context, &lhs);
                // Unmapping a built-in means binding it to nothing.
                self.editor
                    .config
                    .keymap
                    .entry(context.to_string())
                    .or_default()
                    .insert(lhs.clone(), "none".to_string());
                let (keymaps, warnings) = Keymaps::build(&self.editor.config);
                self.keymaps = keymaps;
                for warning in warnings {
                    self.set_message(StatusLevel::Warn, warning);
                }
                // Like vim, `:unmap` changes this session only; `:config write`
                // is how you keep it.
                self.set_message(
                    StatusLevel::Info,
                    if removed {
                        format!("unmapped {lhs} (was a user binding)")
                    } else {
                        format!("{lhs} is now unbound")
                    },
                );
            }
            Command::Config(action) => self.run_config(action),
            Command::FindFiles(query) => {
                self.open_prompt(PromptSpec::FindFiles(query.unwrap_or_default()));
            }
            Command::Grep(query) => self.open_prompt(PromptSpec::Grep(query)),
            Command::Format => {
                if let Some(id) = self.editor.current_buffer_id() {
                    self.spawn_format(id);
                }
            }
            Command::ClearSearchHighlight => {
                self.editor.search_matches.clear();
                self.set_message(StatusLevel::Info, "search highlight cleared");
            }
            Command::GotoLine(line) => {
                let intents = self.editor.goto_line(line);
                self.apply_intents(intents);
            }
            Command::ChangeDir(directory) => self.change_dir(directory),
            Command::Message(level, text) => self.set_message(level, text),
            Command::Reload { force } => self.reload_buffer(force),
            Command::ReloadConfig => self.reload_config(),
            Command::Noop => {}
        }
        self.needs_redraw = true;
    }

    fn run_set(&mut self, arguments: &[String]) {
        for outcome in command::parse_set(arguments, &self.editor) {
            match outcome {
                SetOutcome::Toggle(option) => {
                    let intents = self.editor.toggle(option);
                    self.apply_intents(intents);
                }
                SetOutcome::Changed { key, value } => {
                    if let Err(error) = self.editor.config.set_value(&key, &value) {
                        self.set_message(StatusLevel::Error, error);
                        continue;
                    }
                    let (keymaps, _) = Keymaps::build(&self.editor.config);
                    self.keymaps = keymaps;
                    self.theme = Theme::with_observed(&self.editor.config.theme, self.observed);
                    term::set_mouse_capture(self.editor.config.editor.mouse);
                    self.set_message(
                        StatusLevel::Info,
                        format!(
                            "{key} = {}",
                            self.editor.config.value_of(&key).unwrap_or_default()
                        ),
                    );
                    // `:set` is session-scoped, exactly like vim: writing the
                    // config file is an explicit `:config write`.
                }
                SetOutcome::Show(text) => self.set_message(StatusLevel::Info, text),
                SetOutcome::Summary(summary) => {
                    let items: Vec<prompt::Item> = summary
                        .split("  ")
                        .filter(|entry| !entry.is_empty())
                        .map(|entry| prompt::Item::new(entry.to_string()))
                        .collect();
                    self.open_picker(PromptKind::Palette, "options", "", items);
                }
                SetOutcome::Bad(error) => self.set_message(StatusLevel::Error, error),
            }
        }
    }

    fn run_config(&mut self, action: ConfigAction) {
        match action {
            ConfigAction::Path => {
                let loaded = self
                    .config_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none: defaults>".to_string());
                let user = Config::default_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string());
                self.set_message(
                    StatusLevel::Info,
                    format!("config: {loaded}  default: {user}"),
                );
            }
            ConfigAction::Source => self.reload_config(),
            ConfigAction::Write { path, merge } => {
                let path = expand(&path);
                let text = if merge {
                    self.editor.config.to_toml_string()
                } else {
                    Config::default_config_text().to_string()
                };
                match std::fs::write(&path, text) {
                    Ok(()) => {
                        self.set_message(StatusLevel::Info, format!("wrote {}", path.display()))
                    }
                    Err(error) => {
                        self.set_message(StatusLevel::Error, format!("{}: {error}", path.display()))
                    }
                }
            }
        }
    }

    fn select_buffer(&mut self, target: &str, next: Option<i32>) {
        let ids: Vec<BufferId> = self
            .editor
            .buffers()
            .iter()
            .map(|buffer| buffer.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let current = self
            .editor
            .current_buffer_id()
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        if let Some(delta) = next {
            let index = (current as i32 + delta).rem_euclid(ids.len() as i32) as usize;
            self.editor.focus_buffer(ids[index]);
            return;
        }
        if target.trim().is_empty() {
            let items = self.buffer_items();
            self.open_picker(PromptKind::SwitchBuffer, "buffers", "", items);
            return;
        }
        // Accept a number or a substring of the name.
        if let Ok(number) = target.trim().parse::<u32>() {
            if let Some(id) = ids.iter().find(|id| id.0 == number).copied() {
                self.editor.focus_buffer(id);
                return;
            }
        }
        let needle = target.trim().to_lowercase();
        let found = ids.iter().copied().find(|id| {
            self.editor
                .buffer(*id)
                .is_some_and(|buffer| buffer.display_name().to_lowercase().contains(&needle))
        });
        match found {
            Some(id) => {
                self.editor.focus_buffer(id);
            }
            None => self.set_message(
                StatusLevel::Error,
                format!("no buffer matching \"{target}\""),
            ),
        }
    }

    fn delete_buffer(&mut self, target: &str, force: bool) {
        let ids: Vec<BufferId> = self
            .editor
            .buffers()
            .iter()
            .map(|buffer| buffer.id)
            .collect();
        let Some(id) = ids.iter().copied().find(|id| {
            target.trim().is_empty() && self.editor.current_buffer_id() == Some(*id)
                || !target.trim().is_empty()
                    && self
                        .editor
                        .buffer(*id)
                        .is_some_and(|buffer| buffer.display_name().contains(target.trim()))
        }) else {
            self.set_message(
                StatusLevel::Error,
                format!("no buffer matching \"{target}\""),
            );
            return;
        };
        match self.editor.close_buffer(id, force) {
            Ok(panes) => {
                self.versions.remove(&id);
                self.spell_checked.remove(&id);
                self.set_message(
                    StatusLevel::Info,
                    format!("closed buffer {} ({panes} pane(s))", id.0),
                );
                if self.editor.buffers().is_empty() {
                    self.editor.new_scratch();
                    if let Some(id) = self.editor.current_buffer_id() {
                        self.editor.show_in_pane(id, None);
                    }
                }
            }
            Err(error) => self.set_message(StatusLevel::Error, error),
        }
    }

    fn change_dir(&mut self, directory: Option<String>) {
        match directory {
            None => {
                self.set_message(StatusLevel::Info, format!("cwd {}", self.cwd.display()));
            }
            Some(path) => {
                let path = expand(&path);
                match std::fs::canonicalize(&path) {
                    Ok(directory) => {
                        if let Err(error) = std::env::set_current_dir(&directory) {
                            self.set_message(StatusLevel::Error, error.to_string());
                            return;
                        }
                        self.cwd = directory.clone();
                        self.search_root = search::project_root(Some(&directory));
                        self.set_message(StatusLevel::Info, format!("cwd {}", directory.display()));
                    }
                    Err(error) => {
                        self.set_message(StatusLevel::Error, format!("{}: {error}", path.display()))
                    }
                }
            }
        }
    }

    fn file_changed_on_disk(&self) -> bool {
        self.editor
            .current_buffer()
            .is_some_and(|buffer| buffer.file_modified_on_disk())
    }

    // ----------------------------------------------------------- background

    fn spawn_parse(&mut self, id: BufferId) {
        let Some(buffer) = self.editor.buffer_mut(id) else {
            return;
        };
        if !buffer.language.has_tree_sitter() {
            return;
        }
        if buffer.syntax.status.is_pending() {
            return;
        }
        let version = buffer.version;
        let text = buffer.text();
        let language = match buffer.language.grammar() {
            Some(language) => language,
            None => return,
        };
        buffer.syntax.status = syntax::ParseStatus::Pending;
        let slot = TreeSlot::new();
        self.editor.job_started(
            id,
            crate::editor::JobKind::Parse,
            crate::event::JobId(version),
        );
        syntax::spawn_parse(self.events.sender(), id, version, language, text, slot);
    }

    /// Format `id`, preferring the language server over the external formatter.
    ///
    /// The server knows the language's own rules and can format a range, so it
    /// is asked first; the external formatter is the fallback for languages
    /// with no server, and for a server that refuses the request.
    fn spawn_format(&mut self, id: BufferId) {
        if self.request_lsp_format(id) {
            return;
        }
        self.spawn_external_format(id);
    }

    /// Run the configured external formatter for `id`.
    fn spawn_external_format(&mut self, id: BufferId) {
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let language = buffer.language.id;
        let path = buffer.path().map(|path| path.to_path_buf());
        let version = buffer.version;
        let text = buffer.text();
        let Some(formatter) = format::formatter_for(language, &self.editor.config) else {
            self.set_message(
                StatusLevel::Info,
                format!("no formatter configured for {language}"),
            );
            return;
        };
        let formatter: Formatter = formatter.resolved(path.as_deref(), language);
        self.editor.job_started(
            id,
            crate::editor::JobKind::Format,
            crate::event::JobId(version),
        );
        format::spawn_format(self.events.sender(), id, version, formatter, text);
    }

    fn spawn_spell(&mut self, id: BufferId) {
        if !self.editor.config.spell.enable {
            return;
        }
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let version = buffer.version;
        let text = buffer.text();
        self.spell_checked.insert(id, version);
        self.editor.job_started(
            id,
            crate::editor::JobKind::Spell,
            crate::event::JobId(version),
        );
        spell::spawn_spell(
            self.events.sender(),
            id,
            version,
            text,
            self.editor.config.spell.clone(),
        );
    }

    fn on_async(&mut self, event: AsyncEvent) {
        match event {
            AsyncEvent::Parsed {
                buffer,
                version,
                slot,
            } => {
                self.editor.adopt_tree(buffer, version, slot);
                self.editor
                    .job_finished(buffer, crate::editor::JobKind::Parse);
                self.needs_redraw = true;
            }
            AsyncEvent::Formatted(result) => {
                self.editor
                    .job_finished(result.buffer, crate::editor::JobKind::Format);
                if let Some(error) = result.error {
                    // A missing formatter is common; say it once, briefly.
                    self.set_message(StatusLevel::Warn, format!("{}: {error}", result.formatter));
                } else if let Some(text) = result.text {
                    // The formatter read `result.version` of the buffer.  If the
                    // buffer has moved on it edited while rustfmt ran, and this
                    // text no longer describes it: drop the result and let the
                    // idle timer try again.  Insert mode is the same hazard in
                    // slow motion — a whole-buffer rewrite would yank the caret
                    // out from under whoever is typing.
                    let edited = self
                        .editor
                        .buffer(result.buffer)
                        .is_none_or(|buffer| buffer.version != result.version);
                    if edited || self.editor.mode == Mode::Insert {
                        self.editor.schedule_auto_format(result.buffer);
                    } else {
                        match self.editor.apply_format(result.buffer, &text) {
                            Ok(true) => self.set_message(
                                StatusLevel::Info,
                                format!("formatted with {}", result.formatter),
                            ),
                            Ok(false) => {}
                            Err(error) => self.set_message(StatusLevel::Error, error),
                        }
                    }
                }
                self.needs_redraw = true;
            }
            AsyncEvent::Spelled(SpellResult {
                buffer,
                version,
                diagnostics,
                error,
                ..
            }) => {
                self.editor
                    .job_finished(buffer, crate::editor::JobKind::Spell);
                if let Some(error) = error {
                    self.set_message(StatusLevel::Warn, format!("spell check: {error}"));
                } else if self
                    .editor
                    .buffer(buffer)
                    .is_some_and(|candidate| candidate.version == version)
                {
                    self.editor
                        .set_diagnostics(buffer, DiagnosticSource::Spell, diagnostics);
                }
                self.needs_redraw = true;
            }
            AsyncEvent::Search(search) => self.on_search(search),
            AsyncEvent::Lsp(event) => self.on_lsp(event),
            AsyncEvent::Message(text) => self.set_message(StatusLevel::Info, text),
            AsyncEvent::Failed { what, error } => {
                self.set_message(StatusLevel::Error, format!("{what} failed: {error}"));
                self.needs_redraw = true;
            }
        }
    }

    fn on_search(&mut self, event: SearchEvent) {
        let stale = self
            .search_job
            .as_ref()
            .is_none_or(|handle| handle.id != event.job);
        let open = matches!(
            self.prompt.as_ref().map(|(kind, _)| kind),
            Some(PromptKind::FindFiles) | Some(PromptKind::Grep)
        );
        if stale || !open {
            return;
        }
        if let Some(error) = &event.error {
            self.set_message(StatusLevel::Error, format!("search: {error}"));
            return;
        }
        let root = self.search_root.clone();
        let items: Vec<prompt::Item> = event
            .hits
            .iter()
            .map(|hit| {
                prompt::Item::new(hit.label(&root))
                    .detail(hit.preview(70))
                    .payload(HitPayload {
                        path: hit.path.clone(),
                        line: hit.line,
                    })
            })
            .collect();
        // Every event carries a fresh slice of the current job's hits, and the
        // list was emptied when the job started, so this is a plain append.
        // Replacing here would throw away every batch but the last one.
        if let Some((_, prompt)) = self.prompt.as_mut() {
            prompt.extend_items(items);
        }
        if event.done {
            if event.truncated {
                self.set_message(StatusLevel::Warn, "results truncated; refine the pattern");
            }
            self.set_message(
                StatusLevel::Info,
                format!(
                    "search: {} result(s) with {}",
                    self.prompt
                        .as_ref()
                        .map(|(_, prompt)| prompt.items.len())
                        .unwrap_or(0),
                    event.engine
                ),
            );
        }
        self.needs_redraw = true;
    }

    fn on_lsp(&mut self, event: LspEvent) {
        match event {
            LspEvent::Ready { language, root } => {
                self.set_message(
                    StatusLevel::Info,
                    format!("{} server ready ({})", language, root.display()),
                );
            }
            LspEvent::Failed { language, error } => {
                self.set_message(StatusLevel::Warn, format!("{language} server: {error}"));
            }
            LspEvent::Closed { language, code } => {
                self.set_message(
                    StatusLevel::Info,
                    format!("{language} server closed ({code:?})"),
                );
            }
            LspEvent::Progress { language, message } => {
                self.set_message(StatusLevel::Info, format!("{language}: {message}"));
            }
            LspEvent::Diagnostics {
                uri,
                version,
                items,
            } => {
                let Some(path) = lsp::path_for_uri(&uri) else {
                    return;
                };
                let Some(id) = self.editor.buffer_by_path(&path) else {
                    return;
                };
                let Some(buffer) = self.editor.buffer(id) else {
                    return;
                };
                // A publish for a version the buffer has already moved past
                // describes text that no longer exists; its ranges would land
                // on the wrong characters.  Servers that omit the version are
                // trusted, since there is nothing to compare against.
                if version.is_some_and(|version| version < buffer.version as i64) {
                    return;
                }
                let diagnostics: Vec<_> = items
                    .iter()
                    .map(|item| item.to_diagnostic(buffer))
                    .collect();
                self.editor
                    .set_diagnostics(id, DiagnosticSource::Lsp, diagnostics);
                self.needs_redraw = true;
            }
            LspEvent::Completions {
                uri,
                request,
                items,
            } => {
                self.on_completions(uri, request, items);
            }
            LspEvent::Hover {
                uri,
                request,
                text,
            } => {
                self.on_hover(uri, request, text);
            }
            LspEvent::Formatted {
                uri,
                request,
                edits,
                error,
            } => {
                self.on_formatted(uri, request, edits, error);
            }
            LspEvent::CodeActions {
                uri,
                request,
                actions,
            } => {
                self.on_code_actions(uri, request, actions);
            }
        }
    }

    // ----------------------------------------------------------- completion

    /// Is completion wanted right now?
    ///
    /// It needs the server switch, the completion switch, insert mode (a popup
    /// while navigating would fight the keymap) and a buffer the server knows.
    fn completion_enabled(&self) -> bool {
        self.editor.config.lsp.enable
            && self.editor.config.lsp.completion
            && self.editor.mode == Mode::Insert
    }

    /// Ask the language server for completions at the caret.
    ///
    /// Only in insert mode, only for a buffer with a path (the server keys
    /// documents by URI) and only when a server is configured.  The request is
    /// debounced: typing a word sends one request once the keys settle, not one
    /// per character.
    fn schedule_completion(&mut self) {
        if !self.completion_enabled() {
            return;
        }
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        if buffer.path().is_none() {
            return;
        }
        let cursor = self.editor.cursor();
        if crate::completion::prefix_at(buffer, cursor).is_none() {
            // Not in a word: close any popup and stop asking.
            self.completion = None;
            self.completion_due = None;
            return;
        }
        self.completion_due = Some(Instant::now() + COMPLETION_IDLE);
    }

    /// Send the debounced request, if one is due.
    fn flush_completion(&mut self) {
        let Some(due) = self.completion_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.completion_due = None;
        self.request_completion();
    }

    fn request_completion(&mut self) {
        if !self.completion_enabled() {
            return;
        }
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            return;
        };
        let cursor = self.editor.cursor();
        let Some((start, prefix)) = crate::completion::prefix_at(buffer, cursor) else {
            return;
        };
        // A request that lands right after `::` or `.` is a trigger-character
        // request: that is how path and member lists are properly asked for.
        let trigger = if prefix.is_empty() {
            crate::completion::trigger_char(buffer, cursor)
        } else {
            None
        };
        let language = buffer.language.id.to_string();
        let Some(spec) = self.server_spec_for(&language) else {
            return;
        };
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let line = buffer.line_of(cursor);
        // LSP positions are UTF-16 code units, not character indices.
        let character = buffer.utf16_col_in_line(cursor);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.complete(uri, request, line, character, trigger);
            self.completion_pending = Some((request, start, id));
        }
    }

    /// A server answered a completion request.
    fn on_completions(
        &mut self,
        uri: String,
        request: i64,
        items: Vec<crate::lsp::CompletionItem>,
    ) {
        // Only the answer to the request we are waiting for is interesting: a
        // slower answer for an older caret would pop up under the wrong word.
        let Some((pending, start, buffer)) = self.completion_pending else {
            return;
        };
        if pending != request {
            return;
        }
        self.completion_pending = None;
        if self.editor.mode != Mode::Insert || self.editor.current_buffer_id() != Some(buffer) {
            return;
        }
        let Some(path) = self.editor.buffer(buffer).and_then(|b| b.path()) else {
            return;
        };
        if lsp::uri_for(path) != uri {
            return;
        }
        if items.is_empty() {
            self.completion = None;
            return;
        }
        let mut completion = crate::completion::Completion::new(buffer, request, start, items);
        if let Some(buffer) = self.editor.buffer(buffer) {
            let cursor = self.editor.cursor();
            // Filter by the word run at the caret, not everything since the
            // recorded start: after `use std::io` the filter is `io`, never a
            // string carrying path separators that would fuzzy-match junk.
            completion.refilter(&crate::completion::filter_prefix(buffer, cursor));
        }
        self.completion = Some(completion);
        self.needs_redraw = true;
    }

    // ------------------------------------------------------- hover / format

    /// Ask the server what is at the caret (`K`).
    fn request_hover(&mut self) {
        if !self.editor.config.lsp.enable {
            return;
        }
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            return;
        };
        let language = buffer.language.id.to_string();
        let Some(spec) = self.server_spec_for(&language) else {
            return;
        };
        let cursor = self.editor.cursor();
        let line = buffer.line_of(cursor);
        // LSP positions are UTF-16 code units, not character indices.
        let character = buffer.utf16_col_in_line(cursor);
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.hover(uri, request, line, character);
            self.hover_pending = Some((request, id));
        }
    }

    /// A server answered a hover request.
    fn on_hover(&mut self, uri: String, request: i64, text: String) {
        // Only the answer to the request we are waiting for is interesting: a
        // slower answer for an older caret would describe the wrong symbol.
        let Some((pending, buffer)) = self.hover_pending else {
            return;
        };
        if pending != request {
            return;
        }
        self.hover_pending = None;
        let Some(path) = self.editor.buffer(buffer).and_then(|b| b.path()) else {
            return;
        };
        if lsp::uri_for(path) != uri {
            return;
        }
        if text.trim().is_empty() {
            self.set_message(StatusLevel::Info, "no hover information");
            return;
        }
        self.hover = Some(text);
        self.needs_redraw = true;
    }

    /// Ask the server to format `id`, falling back to the external formatter.
    ///
    /// The server is preferred when one is configured: it knows the language's
    /// own rules, and it can format a range rather than the whole buffer.
    fn request_lsp_format(&mut self, id: BufferId) -> bool {
        if !self.editor.config.lsp.enable {
            return false;
        }
        let Some(buffer) = self.editor.buffer(id) else {
            return false;
        };
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            return false;
        };
        let language = buffer.language.id.to_string();
        let Some(spec) = self.server_spec_for(&language) else {
            return false;
        };
        let tab_size = self.editor.config.editor.tab_width_for(&language);
        let insert_spaces = self.editor.config.editor.expand_tab_for(&language);
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.format(uri, request, tab_size, insert_spaces);
            self.format_pending = Some((request, id));
            true
        } else {
            false
        }
    }

    /// Ask the server to format one range of `id` (`=` on a motion).
    ///
    /// Falls back to the whole-buffer formatter when no server is configured:
    /// an external formatter has no way to be told about a range.
    fn spawn_range_format(&mut self, id: BufferId, span: crate::selection::Span) {
        if !self.editor.config.lsp.enable {
            self.spawn_external_format(id);
            return;
        }
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            self.spawn_external_format(id);
            return;
        };
        let language = buffer.language.id.to_string();
        let Some(spec) = self.server_spec_for(&language) else {
            self.spawn_external_format(id);
            return;
        };
        let tab_size = self.editor.config.editor.tab_width_for(&language);
        let insert_spaces = self.editor.config.editor.expand_tab_for(&language);
        // LSP ranges are line/UTF-16-column pairs, so the byte span is
        // converted back through the buffer's own line table.
        let start = (
            buffer.line_of(span.start),
            buffer.utf16_col_in_line(span.start),
        );
        let end = (
            buffer.line_of(span.end),
            buffer.utf16_col_in_line(span.end),
        );
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.range_format(uri, request, start, end, tab_size, insert_spaces);
            self.format_pending = Some((request, id));
        } else {
            self.spawn_external_format(id);
        }
    }

    /// A server answered a formatting request.
    fn on_formatted(
        &mut self,
        uri: String,
        request: i64,
        edits: Vec<lsp::TextEdit>,
        error: Option<String>,
    ) {
        let Some((pending, buffer)) = self.format_pending else {
            return;
        };
        if pending != request {
            return;
        }
        self.format_pending = None;
        let Some(path) = self.editor.buffer(buffer).and_then(|b| b.path()) else {
            return;
        };
        if lsp::uri_for(path) != uri {
            return;
        }
        if let Some(error) = error {
            // The server refused the request.  Fall back to the external
            // formatter rather than leaving the buffer unformatted with no
            // explanation.
            self.set_message(
                StatusLevel::Warn,
                format!("language server could not format: {error}"),
            );
            self.spawn_external_format(buffer);
            return;
        }
        if edits.is_empty() {
            // The server had nothing to change; that is not an error.
            return;
        }
        // `:format` is something the user asked for, so it gets its own undo
        // step; an automatic run folds into the edit it followed.
        let asked = self.editor.format_requested == Some(buffer);
        self.editor.format_requested = None;
        let group = if asked {
            crate::buffer::ChangeGroup::Command
        } else {
            crate::buffer::ChangeGroup::Format
        };
        let applied = self.editor.apply_edits(buffer, &edits, group);
        if applied > 0 {
            self.set_message(
                StatusLevel::Info,
                format!("formatted with the language server ({applied} edit(s))"),
            );
        }
        self.needs_redraw = true;
    }

    // ---------------------------------------------------------- code actions

    /// Ask for quick fixes covering the caret's line.
    ///
    /// Every early exit says why: a key that silently does nothing reads as a
    /// broken key, and the usual cause is a switch that is off rather than a
    /// server with nothing to offer.
    fn request_code_actions(&mut self) {
        if !self.editor.config.lsp.enable {
            self.set_message(
                StatusLevel::Info,
                "quick fixes need [lsp] enable = true",
            );
            return;
        }
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            self.set_message(
                StatusLevel::Info,
                "quick fixes need a file on disk (the server keys documents by path)",
            );
            return;
        };
        let language = buffer.language.id.to_string();
        let Some(spec) = self.server_spec_for(&language) else {
            self.set_message(
                StatusLevel::Info,
                format!("no language server configured for {language}"),
            );
            return;
        };
        // The whole line, not just the caret: a quick fix for an unused binding
        // covers the binding, and the caret may sit anywhere on it.
        let cursor = self.editor.cursor();
        let line = buffer.line_of(cursor);
        let line_start = buffer.line_start(line);
        let line_end = buffer.line_end(line);
        // LSP positions are UTF-16 code units, so the byte offsets are converted
        // rather than sent as character indices.
        let start = (line, buffer.utf16_col_in_line(line_start));
        let end = (line, buffer.utf16_col_in_line(line_end));
        // The diagnostics on this line are what the server keys its fixes off;
        // sending an empty list means an empty answer.
        let diagnostics: Vec<serde_json::Value> = buffer
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.range.start <= line_end && diagnostic.range.end >= line_start
            })
            .map(|diagnostic| lsp::diagnostic_json(buffer, diagnostic))
            .collect();
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.code_action(uri, request, start, end, diagnostics);
            self.code_action_pending = Some((request, id));
        }
    }

    /// A server answered a code-action request.
    fn on_code_actions(&mut self, uri: String, request: i64, actions: Vec<lsp::CodeAction>) {
        let Some((pending, buffer)) = self.code_action_pending else {
            return;
        };
        if pending != request {
            return;
        }
        self.code_action_pending = None;
        let Some(path) = self.editor.buffer(buffer).and_then(|b| b.path()) else {
            return;
        };
        if lsp::uri_for(path) != uri {
            return;
        }
        if actions.is_empty() {
            self.set_message(StatusLevel::Info, "no quick fixes here");
            return;
        }
        let items: Vec<prompt::Item> = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                prompt::Item::new(action.title.clone())
                    .detail(format!("{} edit(s)", action.edits.len()))
                    .payload(CodeActionPayload {
                        buffer,
                        index,
                    })
            })
            .collect();
        self.code_actions = actions;
        self.open_picker(PromptKind::CodeActions, "quick fixes", "", items);
    }

    /// Handle a key while the completion popup is open.
    ///
    /// Returns true when the key was consumed.  `Tab`/`Enter` accept, the arrow
    /// keys move the highlight, and `Esc` closes the popup without leaving
    /// insert mode.  Anything else falls through to the editor, which is what
    /// keeps typing narrowing the list.
    fn completion_key(&mut self, key: KeyEvent) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        // With nothing to offer, the popup must not eat keys: `Tab` still
        // indents and `Enter` still opens a line.
        if !completion.is_visible() {
            self.completion = None;
            return false;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Enter => {
                self.accept_completion();
                true
            }
            KeyCode::Esc => {
                // Closing the popup must also cancel a request that was still
                // waiting for the typing to settle, or it would reopen at once.
                self.completion = None;
                self.completion_due = None;
                self.needs_redraw = true;
                true
            }
            KeyCode::Down => {
                completion.move_selection(1);
                self.needs_redraw = true;
                true
            }
            KeyCode::Up => {
                completion.move_selection(-1);
                self.needs_redraw = true;
                true
            }
            _ => false,
        }
    }

    /// Insert the highlighted candidate, replacing the typed prefix.
    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        // Accepting is a decision: a request still waiting for the typing to
        // settle must not reopen the popup on top of the inserted text.
        self.completion_due = None;
        let Some(item) = completion.selected_item().cloned() else {
            return;
        };
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        if id != completion.buffer {
            return;
        }
        // Read the buffer state before mutating it: where the caret is, where
        // the word under it starts now, and what follows the caret on the line.
        let (cursor, word_start, tail) = match self.editor.buffer(id) {
            Some(buffer) => {
                let cursor = self.editor.cursor();
                // The live word start, not `completion.start`: an idle rewrite
                // (auto-format) can move text between the request and this
                // keystroke, and a stale offset would leave the typed prefix in
                // place — the `printprintln` bug.
                let word_start = crate::completion::prefix_at(buffer, cursor)
                    .map(|(start, _)| start)
                    .unwrap_or(completion.start);
                let line = buffer.line_of(cursor);
                let tail = buffer.slice(crate::selection::Span::new(cursor, buffer.line_end(line)));
                (cursor, word_start, tail)
            }
            None => return,
        };
        // The server may name the exact range to replace, but only trust it
        // when it ends at the caret and covers everything typed so far: an
        // empty or short range would insert on top of the prefix instead of
        // replacing it.  Otherwise the live word is the range.
        let span = item
            .replace
            .filter(|range| range.end == cursor && range.start <= word_start.min(cursor))
            .unwrap_or_else(|| crate::selection::Span::new(word_start.min(cursor), cursor));

        // Snippet payloads carry tab stops; a macro in front of nothing grows
        // its call shape with the caret between the quotes.  Both live in
        // `completion::accepted` so they can be tested without an editor.
        let expanded = crate::completion::accepted(&item, &tail);

        let intents = self.editor.replace_span(span, &expanded.text);
        self.apply_intents(intents);
        self.editor.set_cursor(span.start + expanded.caret);
        self.needs_redraw = true;
    }

    /// Keep the popup in step with what has been typed since it opened.
    fn refresh_completion(&mut self) {
        let Some(completion) = self.completion.as_mut() else {
            return;
        };
        let Some(id) = self.editor.current_buffer_id() else {
            self.completion = None;
            return;
        };
        if id != completion.buffer || self.editor.mode != Mode::Insert {
            self.completion = None;
            return;
        }
        let cursor = self.editor.cursor();
        let Some(buffer) = self.editor.buffer(id) else {
            self.completion = None;
            return;
        };
        if cursor < completion.start {
            self.completion = None;
            return;
        }
        let since_start = buffer.slice(crate::selection::Span::new(completion.start, cursor));
        // Whitespace means the caret left the word.  A `:` or `.` means it
        // crossed into a new path segment: the list belongs to the old context
        // (members of `std` while typing `use std::io::`), so drop it and let
        // the trigger-character request replace it, rather than fuzzy-matching
        // separators against candidates that never contained them.
        if since_start
            .chars()
            .any(|c| c.is_whitespace() || c == ':' || c == '.')
        {
            self.completion = None;
            return;
        }
        completion.refilter(&crate::completion::filter_prefix(buffer, cursor));
        if !completion.is_visible() {
            self.completion = None;
        }
    }

    fn start_search(&mut self, kind: SearchKind, query: String) {
        if let Some(handle) = self.search_job.take() {
            handle.cancel();
        }
        // The picker shows exactly the hits for the pattern that is in the
        // prompt, so a new search starts from an empty list instead of
        // appending to the one it replaces.
        if let Some((_, prompt)) = self.prompt.as_mut() {
            prompt.clear_items();
        }
        let config = self.editor.config.search.clone();
        let root = self.search_root.clone();
        let tx = self.events.sender();
        match kind {
            SearchKind::FileNames => {
                self.search_job = Some(search::spawn_file_search(tx, root, query, config));
            }
            SearchKind::Contents => {
                if query.trim().is_empty() {
                    return;
                }
                self.search_job = search::spawn_grep(tx, root, query, config);
            }
        }
    }

    fn open_hit(&mut self, payload: &HitPayload) {
        let intents = self.editor.goto_hit(&payload.path, payload.line);
        self.apply_intents(intents);
        if let Some(id) = self.editor.current_buffer_id() {
            self.spawn_parse(id);
            // The language server only answers about documents it has been told
            // exist.  Without this `didOpen`, a file reached through the picker
            // or a grep hit gets completions from no crate context — global
            // symbols instead of the module's namespace — and no diagnostics.
            self.notify_lsp_open(id);
        }
    }

    // ------------------------------------------------------------------ lsp

    fn notify_lsp_open(&mut self, id: BufferId) {
        if !self.editor.config.lsp.enable {
            return;
        }
        let (path, language, text, root) = match self.editor.buffer(id) {
            Some(buffer) => {
                let Some(path) = buffer.path() else {
                    return;
                };
                (
                    path.to_path_buf(),
                    buffer.language.id.to_string(),
                    buffer.text(),
                    search::project_root(Some(path)),
                )
            }
            None => return,
        };
        let Some(spec) = self.server_spec_for(&language) else {
            return;
        };
        let uri = lsp::uri_for(&path);
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.did_open(uri, &language, text);
        }
    }

    fn notify_lsp_save(&mut self, id: BufferId) {
        if !self.editor.config.lsp.enable {
            return;
        }
        let Some(path) = self.editor.buffer(id).and_then(|buffer| buffer.path()) else {
            return;
        };
        let root = search::project_root(Some(path));
        let uri = lsp::uri_for(path);
        if let Some(client) = self.clients.get(&root) {
            client.did_save(uri);
        }
    }

    /// Tell the server the buffer changed.
    ///
    /// Without this the server answers completions and diagnostics against the
    /// text it was opened with, which is wrong from the first keystroke on.
    fn notify_lsp_change(&mut self, id: BufferId) {
        if !self.editor.config.lsp.enable {
            return;
        }
        let (path, text, version) = match self.editor.buffer(id) {
            Some(buffer) => {
                let Some(path) = buffer.path() else {
                    return;
                };
                (path.to_path_buf(), buffer.text(), buffer.version as i64)
            }
            None => return,
        };
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        if let Some(client) = self.clients.get(&root) {
            client.did_change(uri, text, version);
        }
    }

    fn server_spec_for(&self, language: &str) -> Option<ServerSpec> {
        lsp::server_for(language, &self.editor.config)
    }

    // -------------------------------------------------------------- drawing

    /// The pointer shape that matches what the editor is waiting for.
    fn cursor_shape(&self) -> term::CursorShape {
        if self.prompt.is_some() {
            // Typing a command, a path or a pattern: a text caret.
            term::CursorShape::Bar
        } else if self.editor.pending.operator.is_some() {
            // `d`, `c` … waiting for a motion.
            term::CursorShape::Underline
        } else {
            term::cursor_shape_for(self.editor.mode)
        }
    }

    fn pane_area(&self, area: Rect) -> Rect {
        let (_status, panes, _message) = ui::areas(area, &self.editor.config);
        panes
    }

    /// Build the frame contents and draw them.
    fn draw(&mut self) -> crate::error::Result<()> {
        let size = self.terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        self.last_frame = area;
        self.editor.set_last_area(area);

        // The terminal's own pointer is the mode indicator that is a keypress
        // ahead of the status line, so it is set on the first frame as well:
        // a fresh session must look like normal mode rather than like whatever
        // the shell left behind.
        let shape = self.cursor_shape();
        term::set_cursor_shape(shape);

        let status = Snapshot::collect(&self.editor, self.message.clone(), &self.cwd);

        // Borrow the pieces the screen needs without holding `self` mutably.
        let editor = &self.editor;
        let theme = &self.theme;
        let command_line: Option<CommandLine> = self
            .prompt
            .as_ref()
            .filter(|(kind, _)| {
                !matches!(
                    kind,
                    PromptKind::FindFiles
                        | PromptKind::Grep
                        | PromptKind::SwitchBuffer
                        | PromptKind::Palette
                        | PromptKind::Spell { .. }
                        | PromptKind::Diagnostics
                        | PromptKind::CodeActions
                )
            })
            .map(|(_, prompt)| CommandLine {
                prompt: &prompt.label,
                text: &prompt.text,
                caret: prompt.caret(),
                suffix: &prompt.completion_suffix,
            });

        let picker: Option<Picker> = self
            .prompt
            .as_ref()
            .filter(|(_, prompt)| {
                prompt.select_mode && !prompt.items.is_empty() || prompt.select_mode
            })
            .map(|(_, prompt)| Picker {
                title: &prompt.label,
                input: &prompt.text,
                caret: prompt.caret(),
                suffix: &prompt.completion_suffix,
                visible: prompt.visible(),
                items: &prompt.items,
                selected: prompt.selected,
            });

        let completion: Option<ui::CompletionView> = self
            .completion
            .as_ref()
            .map(|completion| ui::CompletionView { completion });

        // The float is resolved here rather than through a `&self` method: the
        // screen already holds an immutable borrow of `editor`, and a method
        // taking all of `self` would collide with the `&mut self.terminal`
        // below.
        let diagnostic_float: Option<ui::DiagnosticFloat> = self
            .diagnostic_hold
            .filter(|(_, since)| since.elapsed() >= DIAGNOSTIC_HOLD)
            .and_then(|(at, _)| {
                let cursor = editor.cursor();
                editor
                    .current_buffer()?
                    .diagnostics
                    .iter()
                    .find(|diagnostic| {
                        diagnostic.range.start == at && diagnostic.range.contains(cursor)
                    })
                    .map(|diagnostic| ui::DiagnosticFloat { diagnostic })
            });

        let screen = Screen {
            editor,
            config: &editor.config,
            theme,
            status,
            command_line,
            picker,
            completion,
            diagnostic_float,
            hover: self.hover.as_deref(),
        };

        let terminal = &mut self.terminal;
        terminal.draw(|frame| {
            let cursor = ui::draw(frame, &screen);
            match cursor {
                Some(position) => {
                    frame.set_cursor_position(position);
                    term::show_cursor();
                }
                None => term::hide_cursor(),
            }
        })?;
        Ok(())
    }

    // ------------------------------------------------------------- item lists

    fn buffer_items(&self) -> Vec<prompt::Item> {
        let current = self.editor.current_buffer_id();
        self.editor
            .buffers()
            .iter()
            .map(|buffer| {
                let mut detail = buffer.language.name.to_string();
                if buffer.is_dirty() {
                    detail.push_str(" modified");
                }
                if Some(buffer.id) == current {
                    detail.push_str(" (current)");
                }
                prompt::Item::new(buffer.display_name())
                    .detail(detail)
                    .payload(BufferPayload(buffer.id))
            })
            .collect()
    }

    fn palette_items(&self) -> Vec<prompt::Item> {
        command::palette_entries()
            .into_iter()
            .map(|(name, usage, help)| {
                prompt::Item::new(format!(":{name}"))
                    .detail(help.to_string())
                    .preview(format!("{usage}\n{help}"))
                    .payload(CommandPayload(name.to_string()))
            })
            .collect()
    }

    fn diagnostic_items(&self) -> Vec<prompt::Item> {
        let Some(buffer) = self.editor.current_buffer() else {
            return Vec::new();
        };
        buffer
            .diagnostics
            .iter()
            .map(|diagnostic| {
                let line = buffer.line_of(diagnostic.range.start) + 1;
                let column = buffer.display_col(diagnostic.range.start) + 1;
                // The headline carries the severity and the server's code, so
                // `error[E0308]` is visible without opening the message.
                let mut preview = diagnostic.headline();
                for (message, span) in &diagnostic.related {
                    let line = buffer.line_of(span.start) + 1;
                    preview.push_str(&format!("\n  {line}: {message}"));
                }
                prompt::Item::new(format!("{line}:{column}"))
                    .detail(diagnostic.headline())
                    .preview(preview)
                    .payload(DiagnosticPayload {
                        buffer: buffer.id,
                        offset: diagnostic.range.start,
                    })
            })
            .collect()
    }

    // --------------------------------------------------------------- config

    /// `:edit` — throw away the buffer and read the file again.
    fn reload_buffer(&mut self, force: bool) {
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        if let Some(intent) = self.editor.reload_from_disk(force) {
            self.apply_intents(vec![intent]);
        }
        // The text is new as far as the background jobs are concerned.
        self.versions.remove(&id);
        self.spell_checked.remove(&id);
        self.spawn_parse(id);
        if self.editor.config.spell.enable {
            self.spawn_spell(id);
        }
        self.notify_lsp_save(id);
    }

    fn reload_config(&mut self) {
        match Config::load(self.config_path.as_deref()) {
            Ok((config, warnings)) => {
                let mouse = config.editor.mouse;
                self.editor.config = config;
                let (keymaps, _) = Keymaps::build(&self.editor.config);
                self.keymaps = keymaps;
                self.theme = Theme::with_observed(&self.editor.config.theme, self.observed);
                term::set_mouse_capture(mouse);
                if warnings.is_empty() {
                    self.set_message(StatusLevel::Info, "configuration reloaded");
                } else {
                    let count = warnings.len();
                    for warning in warnings.iter().take(4) {
                        self.set_message(StatusLevel::Warn, warning.clone());
                    }
                    if count > 4 {
                        self.set_message(StatusLevel::Warn, format!("… and {} more", count - 4));
                    }
                }
                // Re-detect nothing; just re-parse so highlight changes land.
                let ids: Vec<BufferId> = self
                    .editor
                    .buffers()
                    .iter()
                    .map(|buffer| buffer.id)
                    .collect();
                for id in ids {
                    if let Some(buffer) = self.editor.buffer_mut(id) {
                        buffer.syntax.invalidate();
                    }
                    self.spawn_parse(id);
                }
            }
            Err(error) => self.set_message(StatusLevel::Error, error.to_string()),
        }
        self.needs_redraw = true;
    }
}

/// Remember a command-line or search entry, newest last, de-duplicated.
fn push_history(history: &mut Vec<String>, entry: String) {
    if entry.trim().is_empty() || history.last().is_some_and(|last| last == &entry) {
        return;
    }
    history.push(entry);
    if history.len() > 200 {
        let excess = history.len() - 200;
        history.drain(..excess);
    }
}

fn expand(text: &str) -> PathBuf {
    crate::buffer::expand_home(Path::new(text.trim()))
}

/// The `[keymap.…]` section a `:unmap` applies to.
fn context_name(mode: command::MappingMode) -> &'static str {
    match mode {
        command::MappingMode::Insert => "insert",
        command::MappingMode::Visual => "visual",
        command::MappingMode::Command => "command",
        command::MappingMode::Normal | command::MappingMode::All => "normal",
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}
