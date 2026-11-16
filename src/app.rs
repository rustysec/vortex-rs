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
use crate::buffer::{Buffer, BufferId};
use crate::command::{self, Command, ConfigAction, SetOutcome, SplitDirection};
use crate::config::Background;
use crate::config::Config;
use crate::diagnostic::{Diagnostic, DiagnosticScope, DiagnosticSource, ProjectDiagnostic};
use crate::editor::{Editor, Intent, PromptSpec, StatusLevel};
use crate::event::{AsyncEvent, Event, Events};
use crate::format::{self, Formatter};
use crate::input::Keymaps;
use crate::input::Mode;
use crate::lang;
use crate::layout::Axis;
use crate::lsp::{self, Clients, LspEvent, ServerSpec};
use crate::prompt::{self, Prompt, PromptAction};
use crate::search::{self, SearchEvent, SearchHandle, SearchKind};
use crate::spell::{self, SpellResult};
use crate::status::Snapshot;
use crate::syntax::{self, TreeSlot};
use crate::term::{self, Terminal, TerminalGuard};
use crate::theme::{Observed, Theme};
use crate::ui::{self, Notice, NoticeTone, Picker, Screen};

/// Idle time before a buffer is re-parsed.
const PARSE_IDLE: Duration = Duration::from_millis(120);
/// Idle time before a buffer is spell checked.
const SPELL_IDLE: Duration = Duration::from_millis(1_500);
/// Idle time before an auto-format runs.
const FORMAT_FALLBACK_IDLE: Duration = Duration::from_millis(500);
/// How long a `:write` waits for the `[format] on-save` run before writing
/// anyway.  A formatter that has not answered by its own ten-second timeout is
/// not going to, and a save must not be lost over the formatting.
const SAVE_FORMAT_TIMEOUT: Duration = Duration::from_secs(12);
/// Idle time before a completion request goes out, so typing a word sends one
/// request rather than one per character.
const COMPLETION_IDLE: Duration = Duration::from_millis(90);
/// How long the caret must rest on a diagnostic before its message is shown.
const DIAGNOSTIC_HOLD: Duration = Duration::from_millis(500);
/// How often the git branch behind the status row is re-probed; a branch
/// switch happens outside vortex, so only time can notice it.
const GIT_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Convert LSP diagnostics for an unopened workspace file into project entries.
///
/// A buffer converts ranges against its rope; these files have no buffer, so the
/// on-disk text does that job.  `line` and `column` are computed here too — a
/// picker row needs them to show a location without opening the file first.
fn project_diagnostics_from_items(
    text: &str,
    path: &Path,
    source: DiagnosticSource,
    items: &[lsp::DiagnosticItem],
) -> Vec<ProjectDiagnostic> {
    let line_starts = lsp::line_starts(text);
    let tab_width = 4;
    items
        .iter()
        .map(|item| {
            let span = lsp::range_to_span(
                text,
                &line_starts,
                (item.range.0, item.range.1),
                (item.range.2, item.range.3),
            );
            let line = text[..span.start].bytes().filter(|&b| b == b'\n').count();
            let column_start = text[..span.start]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(0);
            let column = crate::buffer::display_width(&text[column_start..span.start], tab_width);
            ProjectDiagnostic {
                path: path.to_path_buf(),
                offset: span.start,
                line,
                column,
                diagnostic: Diagnostic::new(
                    span,
                    item.severity,
                    format!("{}: {}", item.source, item.message),
                    source,
                )
                .with_code(item.code.clone())
                .with_tags(item.tags.clone()),
            }
        })
        .collect()
}

/// Append picker rows for one buffer's diagnostics.
///
/// Ordered by position, like the jump keys, so the list reads as a walk through
/// the text rather than publication order.  The headline carries the severity and
/// the server's code, so `error[E0308]` is visible without opening the message.
fn append_buffer_diagnostics(
    items: &mut Vec<prompt::Item>,
    buffer: &crate::buffer::Buffer,
    project_scope: bool,
) {
    let mut diagnostics: Vec<&Diagnostic> = buffer.diagnostics.iter().collect();
    diagnostics.sort_by_key(|diagnostic| diagnostic.range.start);
    for diagnostic in diagnostics {
        let line = buffer.line_of(diagnostic.range.start) + 1;
        let column = buffer.display_col(diagnostic.range.start) + 1;
        // Project rows carry the file name — the point of the wider scope is
        // that the target may not be where the caret is.
        let label = if project_scope {
            format!("{}:{line}:{column}", buffer.display_name())
        } else {
            format!("{line}:{column}")
        };
        let mut preview = diagnostic.headline();
        for (message, span) in &diagnostic.related {
            let related_line = buffer.line_of(span.start) + 1;
            preview.push_str(&format!("\n  {related_line}: {message}"));
        }
        items.push(
            prompt::Item::new(label)
                .detail(diagnostic.headline())
                .preview(preview)
                .payload(DiagnosticPayload {
                    buffer: Some(buffer.id),
                    path: buffer.path().map(Path::to_path_buf).unwrap_or_default(),
                    offset: diagnostic.range.start,
                }),
        );
    }
}

/// What the open prompt collects; drives how submission is interpreted.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptKind {
    Command,
    /// The guided parameter stage of a chosen command: `head` is the command
    /// word and `collected` the answers already given, so the next question can
    /// be asked with everything before it intact.
    Param {
        head: String,
        collected: Vec<String>,
    },
    Search {
        forward: bool,
    },
    Path,
    GotoLine,
    FindFiles,
    Grep,
    /// Locations a language server named — every reference to a symbol, or
    /// several candidate definitions — picked like grep hits.
    References,
    SwitchBuffer,
    Palette,
    Spell {
        span: crate::selection::Span,
    },
    Diagnostics,
    /// The new name asked for by a rename; submitting sends the request.
    Rename,
    /// Quick fixes offered by the language server.
    CodeActions,
}

/// A hit picked from a search list.
#[derive(Debug, Clone)]
pub struct HitPayload {
    pub path: PathBuf,
    pub line: usize,
}

/// The file shown beside the search pickers: its path, the label drawn above
/// the pane, and its lines pre-split into highlight runs (see
/// [`crate::syntax::line_runs`]).
type PreviewCache = (PathBuf, String, Vec<Vec<(String, syntax::Hl)>>);

/// A diagnostic row picked from `:error`-style list.
#[derive(Debug, Clone)]
pub struct DiagnosticPayload {
    /// The buffer to focus when the file is open; `None` for a workspace file
    /// that has no buffer yet (the picker opens it on selection).
    pub buffer: Option<BufferId>,
    /// Absolute path of the file this diagnostic belongs to.
    pub path: PathBuf,
    pub offset: usize,
}

/// Payload of the buffer picker.
#[derive(Debug, Clone, Copy)]
pub struct BufferPayload(pub BufferId);

/// Payload of a `:` popup row: the command word it runs.
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
    /// The file shown beside the search pickers' list: its path, the label
    /// drawn above it, and its pre-highlighted lines.  Rebuilt when the
    /// selection moves to another file — reading and parsing happen here, not
    /// in the renderer, which must stay a pure function of the frame.
    preview_cache: Option<PreviewCache>,
    pub message: Option<(StatusLevel, String)>,
    pub message_expiry: Option<Instant>,
    pub cwd: PathBuf,
    pub config_path: Option<PathBuf>,
    pub command_history: Vec<String>,
    pub search_history: Vec<String>,
    pub search_job: Option<SearchHandle>,
    pub search_root: PathBuf,
    /// The git branch of the focused buffer's repository, from the last probe.
    git_branch: Option<String>,
    /// Which file the current branch was probed for and when: a new file or an
    /// old answer schedules a fresh probe.
    git_probe: Option<(Option<PathBuf>, Instant)>,
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
    /// The LSP format request in flight: `(request, buffer, version)`.
    ///
    /// The version is what the server was asked about.  A format answer that
    /// lands after the buffer has moved on describes text that no longer
    /// exists, and applying it would rewrite the user's typing.
    format_pending: Option<(i64, BufferId, u64)>,
    /// A `:write` waiting for the `[format] on-save` run to land:
    /// `(buffer, target path, when it started waiting)`.
    ///
    /// The formatter has to run before the bytes go out, so the write is what
    /// waits — on the event loop, never by blocking the key press.  It always
    /// fires in the end: a formatter that fails, lands stale or never answers
    /// must not cost the user their save, so [`App::on_tick`] writes it once the
    /// formatter's own timeout has passed.
    pending_save: Option<(BufferId, Option<PathBuf>, Instant)>,
    /// The code-action request in flight: `(id, buffer)`.
    code_action_pending: Option<(i64, BufferId)>,
    /// The navigation request in flight (definition or references): `(id, buffer)`.
    ///
    /// One slot serves both kinds: the answer arrives tagged as what it is, and
    /// an answer whose id is no longer the pending one is dropped exactly as a
    /// stale hover is — asking `gd` twice in a row just discards the first.
    navigation_pending: Option<(i64, BufferId)>,
    /// The rename request in flight: `(id, buffer, new name, buffer version)`.
    /// The name is what the completion message quotes back; the version drops
    /// an answer the user has since typed past.
    rename_pending: Option<(i64, BufferId, String, u64)>,
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
            preview_cache: None,
            message: None,
            message_expiry: None,
            cwd,
            config_path,
            command_history: Vec::new(),
            search_history: Vec::new(),
            search_job: None,
            search_root,
            git_branch: None,
            git_probe: None,
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
            pending_save: None,
            code_action_pending: None,
            navigation_pending: None,
            rename_pending: None,
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
        // Register every workspace source file with its language server so the
        // project scope sees diagnostics across the whole tree from the start,
        // not just for files that happen to be open.
        app.register_workspace();
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
                // A quit can arrive while a `:write` is still waiting for the
                // on-save format; the file must not silently stay unwritten.
                if let Some((_, target, _)) = self.pending_save.take() {
                    self.write_now(target);
                }
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
                if self.editor.format_blocked(due_buffer) {
                    // Still mid-gesture (`d` waiting for its motion, a picker
                    // open): keep waiting rather than rewriting the text the
                    // gesture is about to resolve against.
                    self.editor.format_held = Some(due_buffer);
                } else {
                    self.spawn_format(due_buffer);
                }
            }
        }

        // A format that was put on hold because a gesture was open in its buffer
        // runs once that gesture closes.  Leaving insert mode arms it directly
        // (`Editor::set_mode`); an operator that finished or timed out, a visual
        // selection that ended and a prompt that closed all land here.
        let held = self.editor.format_held;
        if let Some(id) = held
            && !self.editor.format_blocked(id)
        {
            self.editor.format_held = None;
            self.editor.schedule_auto_format(id);
        }

        // A `:write` held for the on-save format must not wait forever: a server
        // that never answers would otherwise leave the file unwritten.
        let overdue = self
            .pending_save
            .as_ref()
            .is_some_and(|(_, _, waited)| waited.elapsed() >= SAVE_FORMAT_TIMEOUT);
        if overdue {
            let (_, target, _) = self.pending_save.take().expect("checked above");
            self.write_now(target);
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
                // A formatter's own rewrite is not a reason to format again: the
                // run would arm the next one, and a formatter that does not agree
                // with itself on the first pass would rewrite the buffer every
                // `on-idle` interval forever.
                let from_formatter = self
                    .editor
                    .buffer(id)
                    .is_some_and(|buffer| buffer.was_formatted());
                if !from_formatter {
                    self.editor.schedule_auto_format(id);
                }
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
                Intent::Format(id) => {
                    self.spawn_format(id);
                }
                Intent::RangeFormat(id, span) => self.spawn_range_format(id, span),
                Intent::Hover => self.request_hover(),
                Intent::CodeAction => self.request_code_actions(),
                Intent::GotoDefinition => self.request_navigation(false),
                Intent::FindReferences => self.request_navigation(true),
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
                Intent::OpenDiagnosticFile { path, offset } => {
                    self.open_diagnostic(DiagnosticPayload {
                        buffer: None,
                        path,
                        offset,
                    });
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
            PromptSpec::Command(initial) => self.open_command_popup(initial),
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
            PromptSpec::Palette(query) => self.open_command_popup(query),
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
            PromptSpec::Diagnostics(scope) => {
                let items = self.diagnostic_items(scope);
                let title = match scope {
                    DiagnosticScope::Buffer => "diagnostics",
                    DiagnosticScope::Project => "diagnostics (project)",
                };
                self.open_picker(PromptKind::Diagnostics, title, "", items);
            }
            PromptSpec::Rename { symbol } => {
                // The label names the symbol the server will rewrite; the
                // line itself starts empty like every other prompt.
                let label = if symbol.is_empty() {
                    "rename".to_string()
                } else {
                    format!("rename {symbol}: ")
                };
                let prompt = Prompt::new(label);
                self.prompt = Some((PromptKind::Rename, prompt));
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

    /// Read a file and split it into per-line highlight runs for the picker's
    /// preview pane.  Returns nothing when the file cannot be shown: a preview
    /// is a convenience, and a huge or unreadable one must not stall a frame.
    fn load_preview(&self, path: &Path) -> Vec<Vec<(String, syntax::Hl)>> {
        const MAX_PREVIEW_BYTES: u64 = 512 * 1024;

        let Ok(meta) = std::fs::metadata(path) else {
            return Vec::new();
        };
        if meta.len() > MAX_PREVIEW_BYTES {
            return Vec::new();
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        // A scratch buffer: the preview parses on its own tree and never joins
        // the editor's list, so only the path matters — it is what picks the
        // language.  The id is one no real buffer will ever take.
        let mut buffer = Buffer::new(
            BufferId(u32::MAX),
            Some(path.to_path_buf()),
            text,
            &self.editor.config.editor,
        );
        syntax::parse_in_place(&mut buffer);
        syntax::line_runs(&buffer, buffer.language.id)
    }

    /// Point the preview cache at the file under the picker's selection.
    ///
    /// Reading and parsing happen here rather than in the renderer, which must
    /// stay a pure function of the frame; the cache means walking through hits
    /// inside one file costs no I/O at all.
    fn refresh_preview(&mut self) {
        let hit = match self.prompt.as_ref() {
            Some((PromptKind::FindFiles | PromptKind::Grep | PromptKind::References, prompt)) => {
                prompt
                    .visible()
                    .get(prompt.selected)
                    .and_then(|index| prompt.items.get(*index))
                    .and_then(|item| item.payload_as::<HitPayload>())
                    .map(|payload| payload.path.clone())
            }
            _ => None,
        };
        let Some(path) = hit else {
            self.preview_cache = None;
            return;
        };
        if self
            .preview_cache
            .as_ref()
            .is_some_and(|(cached, _, _)| *cached == path)
        {
            return;
        }
        let display = path.display().to_string();
        let lines = self.load_preview(&path);
        self.preview_cache = Some((path, display, lines));
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

    /// The `:` popup: every command in the book, alphabetically, narrowing as
    /// the user types.
    ///
    /// It replaces both the old bare command line and the palette — one list is
    /// easier to learn than two that differ slightly.  A fully typed line still
    /// runs exactly as written (see [`App::submit_prompt`]), so nothing someone
    /// already knows how to type got slower, while anything they did not know
    /// existed is now four keystrokes and a glance away.
    fn open_command_popup(&mut self, initial: String) {
        let mut prompt = self.command_prompt();
        prompt.set_text(initial);
        self.prompt = Some((PromptKind::Command, prompt));
    }

    /// A fresh `:` popup over every command, with the session's history attached.
    ///
    /// It is a picker whose input row *is* the ex line: built from
    /// [`Prompt::command`] so tab-completion ghosting survives, then switched to
    /// select mode so the rows can be walked with the arrows.  Typing narrows the
    /// list either way, which is what makes a bare `:` safe to hand over to someone
    /// who means to type the whole line themselves.
    fn command_prompt(&self) -> Prompt {
        let mut prompt = Prompt::command(String::new()).with_history(self.command_history.clone());
        prompt.label = "commands".into();
        prompt.select_mode = true;
        prompt.set_items(self.command_items());
        prompt
    }

    /// Run a command the popup settled on, asking for its parameters or not.
    ///
    /// `from_row` says whether the choice came from a highlighted row rather than
    /// from a word typed at the top of the card.  It decides how much to ask:
    ///
    /// - A typed word is someone who already knows the command, so it runs as
    ///   written unless it cannot — `:grep` with no pattern still has to be asked,
    ///   while `:w`, `:e` and `:split` do their job on Enter because everything
    ///   left over is optional.  Stepping into questions there would break the
    ///   oldest habit in the book.
    /// - A row *chosen* from the list is the discovery path, so every parameter it
    ///   takes gets asked about, each one skippable with Enter on an empty answer.
    fn choose_command(&mut self, name: &str, from_row: bool) {
        let params = command::params_for(name);
        let asks = match params.first() {
            Some(first) => from_row || !first.optional,
            None => false,
        };
        if asks {
            self.open_param_stage(name.to_string(), Vec::new());
            return;
        }
        let parsed = command::parse(name);
        self.run_command(parsed);
    }

    /// The guided parameter stage of a chosen command.
    ///
    /// One question at a time, with the answers so far kept in `collected` and
    /// echoed back into the next row's candidates: `:config write` asks for an
    /// action, then a path, and each step offers what could possibly go there.
    fn open_param_stage(&mut self, head: String, collected: Vec<String>) {
        let params = command::params_for(&head);
        let Some(param) = params.get(collected.len()) else {
            let line = command::assemble(&head, &collected);
            push_history(&mut self.command_history, line.clone());
            let parsed = command::parse(&line);
            self.run_command(parsed);
            return;
        };
        // The title reads like the command line being assembled — `:config write
        // path` — so each step shows what has been answered and what is asked.
        let mut title = format!(":{head}");
        for answer in &collected {
            title.push_str(&format!(" {answer}"));
        }
        title.push_str(&format!(" {}", param.name));
        if param.optional {
            // An optional answer is accepted by Enter on an empty row, so say so
            // where the person will read it rather than in a manual.
            title.push_str("  (⏎ to skip)");
        }
        let items = self.param_items(param, "");
        // A parameter with nothing to offer — a search pattern, a key sequence —
        // is typed, so it gets a line on the card rather than an empty list whose
        // only row would be a placeholder standing between Enter and the answer.
        let prompt = if items.is_empty() {
            Prompt::new(format!("{title} "))
        } else {
            // Typing narrows the rows and Enter takes the highlighted one, so the
            // top match after a few characters is what the fragment names.  Paths
            // additionally re-list the directory each keystroke, which is how
            // `src/` becomes a list of what is inside it.
            Prompt::picker(title, items)
        };
        self.prompt = Some((PromptKind::Param { head, collected }, prompt));
        self.needs_redraw = true;
    }

    /// Candidates for one parameter: the fixed sets from the command table, and
    /// the two that can only be answered by looking — open buffers and paths.
    ///
    /// `fragment` is what has been typed so far; it only narrows anything for a
    /// path, whose candidates come off the file system rather than out of a list.
    fn param_items(&self, param: &command::Param, fragment: &str) -> Vec<prompt::Item> {
        let requirement = if param.optional {
            "optional"
        } else {
            "required"
        };
        let mut items: Vec<prompt::Item> = match param.kind {
            command::ParamKind::BufferName => self
                .editor
                .buffer_labels()
                .into_iter()
                .map(|(_, label, dirty)| {
                    prompt::Item::new(label).detail(if dirty { "modified" } else { "" }.to_string())
                })
                .collect(),
            command::ParamKind::OptionName => command::option_keys()
                .iter()
                .filter_map(|key| {
                    let value = self.editor.config.value_of(key)?;
                    Some(prompt::Item::new((*key).to_string()).detail(value))
                })
                .collect(),
            _ => command::param_candidates(param, fragment)
                .into_iter()
                .map(prompt::Item::new)
                .collect(),
        };
        for item in items.iter_mut() {
            item.preview = Some(format!("{} ({requirement})", param.help));
        }
        items
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
            match kind {
                // Esc inside a guided parameter backs out one step rather than
                // throwing away the command someone had already chosen: an
                // answer they did not mean to give should cost them only that
                // answer.
                PromptKind::Param {
                    head,
                    mut collected,
                } => {
                    if collected.pop().is_some() {
                        self.open_param_stage(head, collected);
                    } else {
                        // Back at the first parameter: step out to the command
                        // list with the chosen word still on the line.
                        let mut prompt = self.command_prompt();
                        prompt.set_text(format!("{head} "));
                        self.prompt = Some((PromptKind::Command, prompt));
                        self.needs_redraw = true;
                    }
                    return;
                }
                PromptKind::FindFiles | PromptKind::Grep => {
                    if let Some(handle) = self.search_job.take() {
                        handle.cancel();
                    }
                }
                PromptKind::Search { .. } => {
                    // `/` cancelled: vim keeps the previous term but drops the highlight.
                    self.editor.refresh_search_matches();
                }
                _ => {}
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
            PromptKind::Param { head, collected } => {
                // A path parameter is completed from the file system as it is
                // typed, so each keystroke re-lists the directory the fragment
                // names — a fixed list would stop being true the moment someone
                // descended into a folder.
                let Some(param) = command::params_for(&head).get(collected.len()) else {
                    return;
                };
                if matches!(param.kind, command::ParamKind::Path) {
                    let items = self.param_items(param, &text);
                    if let Some((_, prompt)) = self.prompt.as_mut() {
                        prompt.set_items(items);
                    }
                }
            }
            PromptKind::Command
            | PromptKind::Path
            | PromptKind::GotoLine
            | PromptKind::SwitchBuffer
            | PromptKind::Palette
            | PromptKind::Spell { .. }
            | PromptKind::References
            | PromptKind::Rename
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
                // The card already shows the `:` that opens it, but muscle memory
                // from vim types one anyway; a leading colon is punctuation on the
                // same command, not part of its name.
                let line = text.trim().trim_start_matches(':').trim_start().to_string();
                if !line.is_empty() {
                    push_history(&mut self.command_history, line.clone());
                }
                // A line with arguments runs exactly as written — someone who
                // typed `:write notes.txt` said the whole thing.  Otherwise the
                // question is which command they meant.
                if command::has_arguments(&line) {
                    let parsed = command::parse(&line);
                    self.run_command(parsed);
                    return;
                }
                let named = command::resolve(&line);
                let row = item
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<CommandPayload>())
                    .map(|payload| payload.0.clone());
                // Enter on an empty popup is a dismissal, not a choice: with no
                // filter the whole table is visible and row zero would otherwise
                // run whatever sorts first.  `:` then Enter did nothing in vim.
                // Walking down to a row *is* a choice, though, so a moved
                // highlight still counts.
                if line.is_empty() {
                    match (prompt.selected > 0, row) {
                        (true, Some(name)) => self.choose_command(&name, true),
                        _ => self.needs_redraw = true,
                    }
                    return;
                }
                // Which command Enter means, in order of who spoke loudest: a
                // highlight the user *walked to* is a choice; otherwise a typed
                // word that names a command wins, because `:q` must quit even
                // while the filter happens to be holding `:qall`.  With neither —
                // two guessed letters and no command of that name — the top row
                // stands, which is how an unfamiliar command gets found.
                let walked = prompt.selected > 0;
                let chosen = match (walked.then_some(row.as_deref()).flatten(), named) {
                    (Some(picked), _) => Some((picked.to_string(), true)),
                    (None, Some(word)) => Some((word.to_string(), false)),
                    (None, None) => row.map(|picked| (picked, true)),
                };
                match chosen {
                    Some((name, from_row)) => self.choose_command(&name, from_row),
                    // Neither a name nor a row: let the parser report it, as a bare
                    // ex line would.
                    None => {
                        let parsed = command::parse(&line);
                        self.run_command(parsed);
                    }
                }
            }
            PromptKind::Param {
                head,
                mut collected,
            } => {
                // The answer is the highlighted row when there is one, otherwise
                // whatever was typed; an empty answer to an optional parameter is
                // how the popup's "⏎ to skip" gets taken.
                let value = item
                    .and_then(|index| prompt.item(index))
                    .map(|item| item.label.clone())
                    .or_else(|| {
                        let typed = text.trim().to_string();
                        (!typed.is_empty()).then_some(typed)
                    });
                let params = command::params_for(&head);
                match value {
                    Some(value) if !value.is_empty() => {
                        collected.push(value);
                        self.open_param_stage(head, collected);
                    }
                    // Skipped or blank: only the remaining optional parameters
                    // are given up on, so run with what has been answered.
                    _ => {
                        let trailing = params.get(collected.len());
                        if trailing.is_some_and(|param| !param.optional) {
                            let name = &trailing.expect("checked above").name;
                            self.set_message(StatusLevel::Warn, format!(":{head} needs a {name}"));
                            self.open_param_stage(head, collected);
                        } else {
                            let line = command::assemble(&head, &collected);
                            push_history(&mut self.command_history, line.clone());
                            let parsed = command::parse(&line);
                            self.run_command(parsed);
                        }
                    }
                }
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
            PromptKind::References => {
                let index = item.or_else(|| self.prompt_index(&prompt));
                let payload = index
                    .and_then(|index| prompt.item(index))
                    .and_then(|item| item.payload_as::<HitPayload>())
                    .cloned();
                match payload {
                    Some(payload) => self.open_hit(&payload),
                    None => self.set_message(StatusLevel::Warn, "no location selected"),
                }
            }
            PromptKind::Rename => {
                let new_name = text.trim().to_string();
                if new_name.is_empty() {
                    self.set_message(StatusLevel::Warn, "no new name given");
                } else {
                    self.request_rename(new_name);
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
                    Some(payload) => self.open_diagnostic(payload),
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
        let applied =
            self.editor
                .apply_edits(payload.buffer, &edits, crate::buffer::ChangeGroup::Command);
        if applied == 0 {
            self.set_message(StatusLevel::Warn, "the quick fix changed nothing");
            return;
        }
        self.set_message(StatusLevel::Info, title);
        self.apply_intents(vec![Intent::EnsureVisible]);
    }

    /// Jump to a diagnostic row picked from the list.
    ///
    /// A buffer-backed row focuses its existing buffer; a workspace file that
    /// has no buffer yet is opened first (which also registers it with the
    /// server through `notify_lsp_open`), then the caret lands on the offset.
    fn open_diagnostic(&mut self, payload: DiagnosticPayload) {
        let id = match payload.buffer {
            Some(id) => id,
            None => {
                if payload.path.as_os_str().is_empty() {
                    self.set_message(StatusLevel::Warn, "no diagnostic selected");
                    return;
                }
                let id = self.editor.open_file(&payload.path);
                self.spawn_parse(id);
                if self.editor.config.spell.enable {
                    self.spawn_spell(id);
                }
                self.notify_lsp_open(id);
                id
            }
        };
        self.editor.focus_buffer(id);
        self.editor.set_cursor(payload.offset);
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
                // Show the buffer that was just made; asking which one is focused
                // afterwards reads back the pane we are about to replace, so
                // `:enew` looked like a no-op.
                let id = self.editor.new_scratch();
                self.editor.show_in_pane(id, None);
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
                // `[format] on-save` runs the formatter before the bytes go out,
                // so the write waits for the result rather than writing first and
                // being reformatted a moment later — which left an unformatted
                // file on disk and the buffer dirty again right after saving.
                // `:write!` means "now", and a buffer that still needs a name
                // gets its path prompt straight away.
                if !force
                    && self.editor.config.format.on_save
                    && self.write_worth_formatting(target.as_deref())
                {
                    if let Some(id) = self.editor.current_buffer_id() {
                        if self.pending_save.is_some() {
                            // A second `:w` while the first is still waiting: this
                            // one supersedes it rather than queueing two writers.
                            self.pending_save = None;
                        } else if self.spawn_format(id) {
                            self.pending_save = Some((id, target, Instant::now()));
                            return;
                        }
                    }
                }
                self.write_now(target);
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
                // Nothing shadows a binding here, so removing the line is not
                // enough to keep the key quiet across a reload: write an explicit
                // unbind in its place.
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
                    // Turning LSP on has to start the servers: the switch alone
                    // changes nothing until something asks for a client, and
                    // nothing does until the next edit.
                    if key == "lsp.enable" && self.editor.config.lsp.enable {
                        let ids: Vec<BufferId> = self
                            .editor
                            .buffers()
                            .iter()
                            .map(|buffer| buffer.id)
                            .collect();
                        for id in ids {
                            self.notify_lsp_open(id);
                        }
                    }
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

    /// Does this `:write` have a buffer, somewhere to write, and changes worth
    /// running the formatter over before the bytes go out?
    fn write_worth_formatting(&self, target: Option<&Path>) -> bool {
        let Some(buffer) = self.editor.current_buffer() else {
            return false;
        };
        if !buffer.is_dirty() {
            return false;
        }
        // A buffer with no name and no `:write <path>` opens the path prompt
        // first; holding that behind a formatter would just delay the prompt.
        target.is_some() || buffer.path().is_some()
    }

    /// The write itself, run once `[format] on-save` has landed or been given up
    /// on.
    fn write_now(&mut self, target: Option<PathBuf>) {
        // A save under a new name adopts that path: the language server has
        // never seen the new URI, so it will need an open rather than only the
        // save below.  Compute this before the write, which is what rewrites the
        // buffer's path.
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
    }

    /// Write the save an on-format wait was holding, when `buffer` is the buffer
    /// that save was for.
    ///
    /// Called whatever the formatter did — rewrote the text, refused, landed
    /// stale, or the server never answered: the format is a favour a `:write`
    /// does the file, the write is the thing the user asked for.
    fn finish_pending_save(&mut self, buffer: BufferId) {
        let Some((id, target, waited)) = self.pending_save.take() else {
            return;
        };
        if id != buffer {
            // A format of some other buffer landing is not the signal this save
            // is waiting for; keep the deadline it already had.
            self.pending_save = Some((id, target, waited));
            return;
        }
        self.write_now(target);
    }

    /// Format `id`, preferring the language server over the external formatter.
    ///
    /// The server knows the language's own rules and can format a range, so it
    /// is asked first; the external formatter is the fallback for languages
    /// with no server, and for a server that refuses the request.
    ///
    /// Returns whether a run actually started.  `[format] on-save` holds the
    /// write for the result, and a language with no formatter must write now
    /// rather than wait for an answer that will never come.
    fn spawn_format(&mut self, id: BufferId) -> bool {
        if self.request_lsp_format(id) {
            return true;
        }
        self.spawn_external_format(id)
    }

    /// Run the configured external formatter for `id`; true when it started.
    fn spawn_external_format(&mut self, id: BufferId) -> bool {
        let Some(buffer) = self.editor.buffer(id) else {
            return false;
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
            return false;
        };
        let formatter: Formatter = formatter.resolved(path.as_deref(), language);
        self.editor.job_started(
            id,
            crate::editor::JobKind::Format,
            crate::event::JobId(version),
        );
        format::spawn_format(self.events.sender(), id, version, formatter, text);
        true
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
        // `only-comments` means the checker may look only at comments and
        // strings.  The spans come from the syntax tree, which belongs to the
        // buffer and cannot cross into the blocking task, so they are resolved
        // here.
        let prose = if self.editor.config.spell.only_comments {
            if buffer.language.has_tree_sitter() {
                // A code language: wait for a tree that matches the buffer.
                // Falling back to the whole buffer while a parse is pending
                // would report every identifier as a misspelling, which is the
                // noise this setting exists to suppress.  Leaving `spell_checked`
                // alone means the idle timer retries once the parse lands.
                if !buffer.syntax.is_current(buffer.version) {
                    return;
                }
                Some(crate::syntax::comment_and_string_spans(
                    buffer,
                    crate::selection::Span::new(0, buffer.len_bytes()),
                ))
            } else {
                // Markdown, plain text and the like are all prose; there is no
                // code to exclude.
                None
            }
        } else {
            None
        };
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
            prose,
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
                    // idle timer try again.  An open gesture is the same hazard
                    // in slow motion — a whole-buffer rewrite would yank the
                    // caret, or worse, move the text an armed `d` is about to
                    // delete.
                    let edited = self
                        .editor
                        .buffer(result.buffer)
                        .is_none_or(|buffer| buffer.version != result.version);
                    if edited || self.editor.format_blocked(result.buffer) {
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
                // A `:write` may have been waiting on this run; it goes out now
                // whether or not the formatter had anything to say.
                self.finish_pending_save(result.buffer);
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
                // A failed run carries no diagnostics on purpose, so applying them
                // is also what clears the hints a *previous* engine left behind:
                // underlines from a checker that is no longer configured would read
                // as findings the current one agreed with.
                if self
                    .editor
                    .buffer(buffer)
                    .is_some_and(|candidate| candidate.version == version)
                {
                    self.editor
                        .set_diagnostics(buffer, DiagnosticSource::Spell, diagnostics);
                }
                if let Some(error) = error {
                    self.set_message(StatusLevel::Warn, format!("spell check: {error}"));
                }
                self.needs_redraw = true;
            }
            AsyncEvent::Search(search) => self.on_search(search),
            AsyncEvent::Lsp(event) => self.on_lsp(event),
            AsyncEvent::GitBranch(branch) => {
                // Only a finished probe speaks for the row: an older answer
                // still in flight just overwrites the last one, which is fine
                // for something this cosmetic.
                self.git_branch = branch;
                self.needs_redraw = true;
            }
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
            LspEvent::DiagnosticsRefresh { language } => {
                // The server's own analysis changed.  Pull it for every open
                // document this server owns; a push-only client would never see
                // an unsaved error at all.
                self.pull_diagnostics_for(&language);
            }
            LspEvent::Diagnostics {
                uri,
                version,
                items,
                origin,
                unchanged,
            } => {
                let Some(path) = lsp::path_for_uri(&uri) else {
                    return;
                };
                // An `unchanged` pull answer means the previous set still
                // stands; replacing it with nothing would erase live errors.
                if unchanged {
                    return;
                }
                let source = match origin {
                    lsp::DiagnosticOrigin::Push => DiagnosticSource::Lsp,
                    lsp::DiagnosticOrigin::Pull => DiagnosticSource::LspPull,
                };
                // A file registered at startup but not open as a buffer: its
                // diagnostics go into the project store, keyed by path.  Without
                // this branch every such report was dropped as "no such buffer",
                // which is exactly why only opened files ever showed up.
                if self.editor.buffer_by_path(&path).is_none() {
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        return;
                    };
                    let diagnostics = project_diagnostics_from_items(&text, &path, source, &items);
                    self.editor
                        .set_project_diagnostics(path.clone(), source, diagnostics);
                    self.needs_redraw = true;
                    return;
                }
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
                // Push and pull describe different text cuts (saved file vs.
                // edited buffer), so each replaces only its own set.  Merging
                // them is what makes an unsaved error show up next to the
                // compiler's view of what is on disk.  The source is also what
                // `set_diagnostics` retains by, so the items must carry it —
                // storing a pull under `Lsp` would leave the previous pull in
                // place and the list would grow on every keystroke.
                let diagnostics: Vec<_> = items
                    .iter()
                    .map(|item| item.to_diagnostic(buffer, source))
                    .collect();
                self.editor.set_diagnostics(id, source, diagnostics);
                self.needs_redraw = true;
            }
            LspEvent::Completions {
                uri,
                request,
                items,
            } => {
                self.on_completions(uri, request, items);
            }
            LspEvent::Hover { uri, request, text } => {
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
            LspEvent::Definition {
                uri,
                request,
                locations,
            } => {
                self.on_definition(uri, request, locations);
            }
            LspEvent::References {
                uri,
                request,
                locations,
            } => {
                self.on_references(uri, request, locations);
            }
            LspEvent::Renamed {
                uri,
                request,
                changes,
                error,
            } => {
                self.on_renamed(uri, request, changes, error);
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

    // ------------------------------------------------------- goto / references

    /// Ask the server about the symbol at the caret: its definition
    /// (`references` false, `gd`) or every location that mentions it (`gr`).
    ///
    /// Both travel the same path — same position, same pending slot, same
    /// staleness rule — so they share everything up to which request goes out.
    fn request_navigation(&mut self, references: bool) {
        let what = if references {
            "references"
        } else {
            "definition"
        };
        if !self.editor.config.lsp.enable {
            self.set_message(
                StatusLevel::Info,
                format!("{what} need [lsp] enable = true"),
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
                format!("{what} need a file on disk (the server keys documents by path)"),
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
            if references {
                client.references(uri, request, line, character);
            } else {
                client.definition(uri, request, line, character);
            }
            self.navigation_pending = Some((request, id));
        }
    }

    /// Is this the answer we are still waiting for?  Clears the pending slot
    /// when it is.  A slower answer for an older caret or a closed buffer is
    /// dropped rather than yanking the view somewhere it was never asked.
    fn accept_navigation(&mut self, request: i64, uri: &str) -> Option<()> {
        let (pending, buffer) = self.navigation_pending?;
        if pending != request {
            return None;
        }
        self.navigation_pending = None;
        let path = self.editor.buffer(buffer).and_then(|b| b.path())?;
        if lsp::uri_for(path) != uri {
            return None;
        }
        Some(())
    }

    /// The server answered `textDocument/definition`.
    fn on_definition(&mut self, uri: String, request: i64, locations: Vec<lsp::Location>) {
        if self.accept_navigation(request, &uri).is_none() {
            return;
        }
        match locations.as_slice() {
            [] => self.set_message(StatusLevel::Info, "no definition found"),
            // The common answer: go straight there, no popup in the way.
            [only] => self.jump_to_location(only),
            // Several candidates — a trait method and its implementations —
            // are a choice, so they go to the picker like grep hits.
            _ => self.open_locations_picker("definitions", &locations),
        }
    }

    /// The server answered `textDocument/references`.
    fn on_references(&mut self, uri: String, request: i64, locations: Vec<lsp::Location>) {
        if self.accept_navigation(request, &uri).is_none() {
            return;
        }
        if locations.is_empty() {
            self.set_message(StatusLevel::Info, "no references found");
            return;
        }
        self.open_locations_picker("references", &locations);
    }

    /// List locations in the same popup the find/grep pickers use: one row per
    /// hit labelled `path:line`, the file preview beside it, Enter opens the
    /// file with the caret on the hit.  Typing narrows the list with the
    /// picker's own fuzzy filter, so `gr` then a directory name is a quick way
    /// to ask "where does *this file* use the symbol".
    fn open_locations_picker(&mut self, title: &str, locations: &[lsp::Location]) {
        let items: Vec<prompt::Item> = locations
            .iter()
            .map(|location| {
                let path = lsp::path_for_uri(&location.uri).unwrap_or_default();
                let label = path
                    .strip_prefix(&self.search_root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let mut item = prompt::Item::new(format!("{label}:{}", location.line + 1)).payload(
                    HitPayload {
                        path: path.clone(),
                        line: location.line + 1,
                    },
                );
                // The line's text as the detail column, when vortex already has
                // the file in memory; a server happily points at files with no
                // buffer, and reading disk per row for a decoration is not a
                // trade worth making here — the preview pane has it anyway.
                if let Some(buffer) = self
                    .editor
                    .buffer_by_path(&path)
                    .and_then(|id| self.editor.buffer(id))
                {
                    let start = buffer.line_start(location.line);
                    let end = buffer.line_end(location.line);
                    let text = buffer.slice(crate::selection::Span::new(start, end));
                    item = item.detail(text.trim().chars().take(70).collect::<String>());
                }
                item
            })
            .collect();
        self.open_picker(PromptKind::References, title, "", items);
        // The answer arrives on an async event, and unlike the find/grep
        // pickers there is no streaming search behind it whose own events
        // would force a frame: without this the list sits in the prompt until
        // the next key happens to redraw the screen.
        self.needs_redraw = true;
    }

    /// Move the caret to a location the server named, opening its file when
    /// vortex has no buffer for it.  The old position goes on the jump list
    /// first, so `ctrl-o` walks back from a definition the way it does from
    /// `:60` or a mark.
    fn jump_to_location(&mut self, location: &lsp::Location) {
        let Some(path) = lsp::path_for_uri(&location.uri) else {
            self.set_message(
                StatusLevel::Warn,
                format!("unreadable location: {}", location.uri),
            );
            return;
        };
        if let Some(id) = self.editor.current_buffer_id() {
            let before = self.editor.cursor();
            self.editor.jumplist.push((id, before));
        }
        let intents = self
            .editor
            .goto_location(&path, location.line, location.character);
        self.apply_intents(intents);
        if let Some(id) = self.editor.current_buffer_id() {
            self.spawn_parse(id);
            // Same reason as `open_hit`: a file reached by jumping has to be
            // `didOpen`ed before its own server answers anything about it.
            self.notify_lsp_open(id);
        }
    }

    // --------------------------------------------------------------- rename

    /// Send the rename the prompt asked for, at the caret the prompt opened on
    /// (nothing moves the caret while a prompt is up).
    ///
    /// The buffer version travels with the request: a rename answer describes
    /// the text as the server last saw it, and applying it after the user
    /// typed would rewrite that typing at now-wrong offsets — the same rule
    /// formatting follows.
    fn request_rename(&mut self, new_name: String) {
        let Some(id) = self.editor.current_buffer_id() else {
            return;
        };
        let Some(buffer) = self.editor.buffer(id) else {
            return;
        };
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            self.set_message(
                StatusLevel::Warn,
                "renaming needs a file on disk (the server keys documents by path)",
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
        let cursor = self.editor.cursor();
        let line = buffer.line_of(cursor);
        let character = buffer.utf16_col_in_line(cursor);
        let version = buffer.version;
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.rename(uri, request, line, character, new_name.clone());
            self.rename_pending = Some((request, id, new_name, version));
        }
    }

    /// The server answered a rename with a workspace edit; perform it.
    ///
    /// Open files go through the editor, so the rename lands as one undo step
    /// and the change syncs back to the server like any other edit.  Files
    /// vortex has no buffer for are patched on disk: with no buffer there are
    /// no unsaved changes, so the file *is* the text the server edited, and
    /// writing it back cannot clobber anything.
    fn on_renamed(
        &mut self,
        uri: String,
        request: i64,
        changes: Vec<lsp::DocumentEdits>,
        error: Option<String>,
    ) {
        let Some((pending, buffer, new_name, version)) = self.rename_pending.take() else {
            return;
        };
        if pending != request {
            return;
        }
        let Some(path) = self.editor.buffer(buffer).and_then(|b| b.path()) else {
            return;
        };
        if lsp::uri_for(path) != uri {
            return;
        }
        if let Some(error) = error {
            self.set_message(StatusLevel::Warn, format!("rename failed: {error}"));
            self.needs_redraw = true;
            return;
        }
        if self
            .editor
            .buffer(buffer)
            .is_some_and(|b| b.version != version)
        {
            self.set_message(
                StatusLevel::Warn,
                format!("the buffer changed while the server computed the rename; discarded \"{new_name}\""),
            );
            self.needs_redraw = true;
            return;
        }
        let mut applied = 0usize;
        let mut files = 0usize;
        for document in &changes {
            let Some(path) = lsp::path_for_uri(&document.uri) else {
                continue;
            };
            if let Some(id) = self.editor.buffer_by_path(&path) {
                // Positions against the text the buffer actually holds.
                let edits: Vec<lsp::TextEdit> = {
                    let Some(open) = self.editor.buffer(id) else {
                        continue;
                    };
                    document
                        .edits
                        .iter()
                        .map(|edit| lsp::TextEdit {
                            range: crate::selection::Span::new(
                                open.byte_at_utf16(edit.start.0, edit.start.1),
                                open.byte_at_utf16(edit.end.0, edit.end.1),
                            ),
                            new_text: edit.new_text.clone(),
                        })
                        .collect()
                };
                let n = self
                    .editor
                    .apply_edits(id, &edits, crate::buffer::ChangeGroup::Command);
                if n > 0 {
                    files += 1;
                    applied += n;
                }
            } else {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (updated, n) = lsp::apply_edits_to_text(&text, &document.edits);
                if n > 0 && std::fs::write(&path, &updated).is_ok() {
                    files += 1;
                    applied += n;
                }
            }
        }
        if applied == 0 {
            self.set_message(StatusLevel::Warn, "the server sent nothing to change");
        } else {
            self.set_message(
                StatusLevel::Info,
                format!("renamed to {new_name}: {applied} edit(s) in {files} file(s)"),
            );
        }
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
        let version = buffer.version;
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.format(uri, request, tab_size, insert_spaces);
            self.format_pending = Some((request, id, version));
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
        let end = (buffer.line_of(span.end), buffer.utf16_col_in_line(span.end));
        let version = buffer.version;
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let request = self.next_request;
        self.next_request += 1;
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.range_format(uri, request, start, end, tab_size, insert_spaces);
            self.format_pending = Some((request, id, version));
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
        let Some((pending, buffer, version)) = self.format_pending else {
            return;
        };
        if pending != request {
            return;
        }
        self.format_pending = None;
        let Some(path) = self.editor.buffer(buffer).and_then(|b| b.path()) else {
            self.finish_pending_save(buffer);
            return;
        };
        if lsp::uri_for(path) != uri {
            self.finish_pending_save(buffer);
            return;
        }
        // The server formatted the text it was told about.  If the buffer has
        // moved on since, those edits describe text that no longer exists, and
        // applying them would rewrite whatever the user typed in the meantime.
        // An open gesture is the same hazard in slow motion: a rewrite under the
        // caret, or under the offsets an armed `d` is about to delete, is exactly
        // what the external formatter is held back for.
        let stale = self
            .editor
            .buffer(buffer)
            .is_none_or(|buffer| buffer.version != version);
        if stale || self.editor.format_blocked(buffer) {
            self.editor.schedule_auto_format(buffer);
            // The retry is an idle-format matter, not a reason to hold the file
            // open: the save goes out as it stands.
            self.finish_pending_save(buffer);
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
            // The fallback reports through `Formatted`, which is what releases a
            // waiting save; with nothing configured to run there is no result to
            // wait for, so write here instead.
            if !self.spawn_external_format(buffer) {
                self.finish_pending_save(buffer);
            }
            return;
        }
        if edits.is_empty() {
            // The server had nothing to change; that is not an error.
            self.finish_pending_save(buffer);
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
        self.finish_pending_save(buffer);
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
            self.set_message(StatusLevel::Info, "quick fixes need [lsp] enable = true");
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
                    .payload(CodeActionPayload { buffer, index })
            })
            .collect();
        self.code_actions = actions;
        self.open_picker(PromptKind::CodeActions, "quick fixes", "", items);
        // Same async-event rule as the references picker: nothing else will
        // schedule the frame that shows this list.
        self.needs_redraw = true;
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
        let enabled = self.editor.config.lsp.enable;
        // `didOpen` sends the whole document, so any changes recorded before it
        // are already included.  Draining them here stops a later tick from
        // replaying those ranges against a document that already has them.
        let Some(buffer) = self.editor.buffer_mut(id) else {
            return;
        };
        let _ = buffer.take_changes();
        if !enabled {
            return;
        }
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            return;
        };
        let language = buffer.language.id.to_string();
        let text = buffer.text();
        // The version the text belongs to.  Claiming 1 here made the app drop
        // every diagnostic for a buffer that was already past version 1 when
        // the server started, which is what happens when LSP is switched on
        // mid-session.
        let version = buffer.version as i64;
        let root = search::project_root(Some(&path));
        let Some(spec) = self.server_spec_for(&language) else {
            return;
        };
        let uri = lsp::uri_for(&path);
        let sender = self.events.sender();
        if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
            client.did_open(uri, &language, text, version);
        }
        // The server already published diagnostics for this file while it was
        // only registered (not open), and rust-analyzer will not republish
        // unchanged content on a re-open.  Move the stored set onto the buffer
        // so they show immediately instead of waiting for an edit.
        let migrated = self.editor.take_project_diagnostics(&path);
        if !migrated.is_empty() {
            let diagnostics: Vec<Diagnostic> =
                migrated.into_iter().map(|entry| entry.diagnostic).collect();
            self.editor
                .set_diagnostics(id, DiagnosticSource::Lsp, diagnostics);
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

    /// Register every workspace source file with its language server, without
    /// opening a buffer for it.
    ///
    /// A server only analyses documents it has been told about via `didOpen`, so
    /// the project scope used to be limited to files vortex had opened.  Walking
    /// the tree and sending `didOpen` for each matching file makes rust-analyzer
    /// (and friends) report diagnostics across the whole workspace from the very
    /// first frame, instead of only once a file happened to be reached.
    ///
    /// Files are read straight off disk — no buffer is created — so a large tree
    /// costs transient reads rather than permanent buffers.
    fn register_workspace(&mut self) {
        if !self.editor.config.lsp.enable || self.search_root.as_os_str().is_empty() {
            return;
        }
        let root = self.search_root.clone();
        // Collect the files first so we are not holding a borrow of `self` while
        // sending to clients.
        let mut candidates: Vec<(PathBuf, String)> = Vec::new();
        for entry in ignore::WalkBuilder::new(&root).build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.into_path();
            // Only files whose language has a configured server are worth
            // registering; everything else would just be sent to nothing.
            let language = lang::detect(Some(&path), None).id.to_string();
            if self.server_spec_for(&language).is_none() {
                continue;
            }
            candidates.push((path, language));
        }
        for (path, language) in candidates {
            // A file already open as a buffer is registered through the normal
            // `notify_lsp_open` path; registering it again would be redundant.
            if self.editor.buffer_by_path(&path).is_some() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(spec) = self.server_spec_for(&language) else {
                continue;
            };
            let root = search::project_root(Some(&path));
            let uri = lsp::uri_for(&path);
            let sender = self.events.sender();
            if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
                client.did_open(uri, &language, text, 1);
            }
        }
    }

    /// Pull the server's own diagnostics for every open document it owns.
    ///
    /// rust-analyzer pushes the compiler's view of the saved file but serves
    /// its analysis of the edited buffer only through `textDocument/diagnostic`.
    /// It signals that the analysis changed with `workspace/diagnostic/refresh`,
    /// which is what calls this.  Only documents whose language matches the
    /// server that asked are pulled: a Python buffer must not be sent to the
    /// Rust server.
    fn pull_diagnostics_for(&mut self, language: &str) {
        if !self.editor.config.lsp.enable {
            return;
        }
        let targets: Vec<(BufferId, PathBuf, String, i64)> = self
            .editor
            .buffers()
            .iter()
            .filter(|buffer| buffer.language.id == language)
            .filter_map(|buffer| {
                let path = buffer.path()?.to_path_buf();
                Some((
                    buffer.id,
                    path,
                    buffer.language.id.to_string(),
                    buffer.version as i64,
                ))
            })
            .collect();
        for (_id, path, language, version) in targets {
            let Some(spec) = self.server_spec_for(&language) else {
                continue;
            };
            let root = search::project_root(Some(&path));
            let uri = lsp::uri_for(&path);
            let request = self.next_request;
            self.next_request += 1;
            let sender = self.events.sender();
            if let Some(client) = self.clients.ensure(&sender, &spec, &root) {
                client.pull_diagnostics(uri, request, version);
            }
        }
    }

    /// Tell the server the buffer changed.
    ///
    /// Without this the server answers completions and diagnostics against the
    /// text it was opened with, which is wrong from the first keystroke on.
    ///
    /// Incremental sync sends one ranged edit per change; full sync resends the
    /// whole document.  The change log is drained either way, so a buffer whose
    /// server is not running does not accumulate edits forever.
    fn notify_lsp_change(&mut self, id: BufferId) {
        let enabled = self.editor.config.lsp.enable;
        let incremental = self.editor.config.lsp.sync.is_incremental();
        let Some(buffer) = self.editor.buffer_mut(id) else {
            return;
        };
        // `None` means the log overflowed: the ranges are gone, so the whole
        // document has to be sent instead.
        let changes = buffer.take_changes();
        if !enabled {
            return;
        }
        let Some(path) = buffer.path().map(|path| path.to_path_buf()) else {
            return;
        };
        let version = buffer.version as i64;
        let text = buffer.text();
        let root = search::project_root(Some(&path));
        let uri = lsp::uri_for(&path);
        let Some(client) = self.clients.get(&root) else {
            return;
        };
        match changes {
            // Nothing changed: a no-op notification would only make the server
            // re-analyse the document for no reason.
            Some(changes) if changes.is_empty() => {}
            Some(changes) if incremental => {
                client.did_change_ranged(uri, changes, version);
            }
            // Full sync, or the log overflowed and the ranges are gone.
            _ => client.did_change(uri, text, version),
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
        let (_status, panes) = ui::areas(area, &self.editor.config);
        panes
    }

    /// Schedule a git branch probe when it is due: the focused buffer moved to
    /// another file, or nobody has looked in a while.  Outside a repository no
    /// process runs at all — the segment simply stays away.
    fn maybe_probe_git(&mut self) {
        if !self.editor.config.status.show_git_branch {
            return;
        }
        let path = self
            .editor
            .current_buffer()
            .and_then(|buffer| buffer.path())
            .map(Path::to_path_buf);
        let due = match &self.git_probe {
            Some((probed, at)) => *probed != path || at.elapsed() >= GIT_PROBE_INTERVAL,
            None => true,
        };
        if !due {
            return;
        }
        self.git_probe = Some((path.clone(), Instant::now()));
        let root = search::project_root(path.as_deref());
        if !root.join(".git").exists() {
            self.git_branch = None;
            return;
        }
        crate::git::probe_branch(self.events.sender(), root);
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

        // The git branch lives outside vortex's own state, so it is probed off
        // the event loop; this schedules a fresh read when the focused file
        // changes or the last answer grows old.
        self.maybe_probe_git();
        let mut status = Snapshot::collect(&self.editor, self.message.clone(), &self.cwd);
        if self.editor.config.status.show_git_branch {
            status.git_branch = self.git_branch.clone();
        }
        if self.editor.config.status.show_lsp && self.editor.config.lsp.enable {
            if let Some(buffer) = self.editor.current_buffer() {
                status.lsp = self
                    .clients
                    .attached(buffer.language.name)
                    .map(str::to_string);
            }
        }

        // The search pickers show the selected hit's file beside the list.  It
        // is refreshed here rather than in the renderer — which must stay a
        // pure function of the frame — and cached by path so walking through
        // hits inside one file costs no I/O.
        self.refresh_preview();

        // Borrow the pieces the screen needs without holding `self` mutably.
        let editor = &self.editor;
        let theme = &self.theme;
        // A prompt that is not a picker still needs its line on screen: it draws
        // on the transient card, where messages and key echoes also go.  The
        // picker kinds draw their own input row inside the list card.
        let prompt_card: Option<Notice> = self
            .prompt
            .as_ref()
            .filter(|(_, prompt)| !prompt.select_mode)
            .map(|(_, prompt)| Notice {
                prompt: &prompt.label,
                text: &prompt.text,
                caret: Some(prompt.caret()),
                suffix: &prompt.completion_suffix,
                tone: NoticeTone::Prompt,
            });

        // Bound here so the picker closure borrows only this field: capturing
        // all of `self` would collide with the `&mut self.terminal` below.
        let preview_cache = self.preview_cache.as_ref();
        let picker: Option<Picker> = self
            .prompt
            .as_ref()
            .filter(|(_, prompt)| {
                prompt.select_mode && !prompt.items.is_empty() || prompt.select_mode
            })
            .map(|(kind, prompt)| Picker {
                title: &prompt.label,
                // The command popup and its parameter stages read as ex lines;
                // every other list is a plain `>` picker.
                prompt: match kind {
                    PromptKind::Command | PromptKind::Param { .. } => ": ",
                    _ => "> ",
                },
                input: &prompt.text,
                caret: prompt.caret(),
                suffix: &prompt.completion_suffix,
                visible: prompt.visible(),
                items: &prompt.items,
                selected: prompt.selected,
                // The search pickers preview the file under the selection; the
                // cache was pointed at it above.  `line` is 1-based, and the
                // renderer clamps a focus past a short file back into it.
                preview: match kind {
                    PromptKind::FindFiles | PromptKind::Grep | PromptKind::References => {
                        preview_cache.filter(|(_, _, lines)| !lines.is_empty()).map(
                            |(_, display, lines)| ui::FilePreview {
                                path: display.as_str(),
                                lines,
                                focus: prompt
                                    .visible()
                                    .get(prompt.selected)
                                    .and_then(|index| prompt.items.get(*index))
                                    .and_then(|item| item.payload_as::<HitPayload>())
                                    .map_or(0, |payload| payload.line.saturating_sub(1)),
                            },
                        )
                    }
                    _ => None,
                },
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
            prompt: prompt_card,
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

    /// The `:` popup's rows: every command, alphabetically, with its arguments
    /// after the name and its parameters in the preview.
    ///
    /// Alphabetical because the list is a lookup — someone arriving from vim
    /// looks under `w` for `write`, not wherever the table happened to grow.  The
    /// fuzzy filter then narrows it as they type, so the full listing costs
    /// nothing in speed and pays for itself in discoverability: a command nobody
    /// knows exists is a command nobody uses.
    fn command_items(&self) -> Vec<prompt::Item> {
        let mut items: Vec<prompt::Item> = command::listing()
            .into_iter()
            .map(|spec| {
                let preview = match command::param_lines(spec.params) {
                    lines if lines.is_empty() => spec.help.to_string(),
                    lines => format!("{}\n{}", spec.help, lines.trim_end()),
                };
                prompt::Item::new(format!(":{}", spec.name))
                    .detail(command::arguments(spec.usage).to_string())
                    .preview(preview)
                    .payload(CommandPayload(spec.name.to_string()))
            })
            .collect();
        // The actions that parse as one-shot commands but have no ex spec —
        // `:focus-next`, `:zoom-pane` …  They run today; the audit trail was a
        // keymap entry and a typed guess.  A row each makes them findable, and
        // keeps this list honest about what Enter will accept.
        items.extend(
            command::unlisted_commands()
                .into_iter()
                .map(|(name, help)| {
                    prompt::Item::new(format!(":{name}"))
                        .preview(help.to_string())
                        .payload(CommandPayload(name.to_string()))
                }),
        );
        items.sort_by(|a, b| a.label.cmp(&b.label));
        items
    }

    /// Picker rows for `:diagnostic list`, over the buffer or the whole
    /// project.  Project rows carry the file name — the point of the wider
    /// scope is that the target may not be where the caret is.
    fn diagnostic_items(&self, scope: DiagnosticScope) -> Vec<prompt::Item> {
        let mut items = Vec::new();
        match scope {
            DiagnosticScope::Buffer => {
                if let Some(buffer) = self.editor.current_buffer() {
                    append_buffer_diagnostics(&mut items, buffer, false);
                }
            }
            DiagnosticScope::Project => {
                for buffer in self.editor.buffers() {
                    append_buffer_diagnostics(&mut items, buffer, true);
                }
                // Workspace files that are not open as buffers.  Their
                // diagnostics were stored keyed by path at startup; without this
                // the project scope would only ever show opened files.
                for (path, entries) in &self.editor.project_diagnostics {
                    if self.editor.buffer_by_path(path).is_some() {
                        continue;
                    }
                    let mut sorted: Vec<&ProjectDiagnostic> = entries.iter().collect();
                    sorted.sort_by_key(|entry| entry.offset);
                    for entry in sorted {
                        // `line`/`column` are 0-based; the picker shows them
                        // 1-based like every other location.
                        let label =
                            format!("{}:{}:{}", path.display(), entry.line + 1, entry.column + 1);
                        items.push(
                            prompt::Item::new(label)
                                .detail(entry.diagnostic.headline())
                                .preview(entry.diagnostic.headline())
                                .payload(DiagnosticPayload {
                                    buffer: None,
                                    path: entry.path.clone(),
                                    offset: entry.offset,
                                }),
                        );
                    }
                }
            }
        }
        items
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
                    // A reload is how `[lsp] enable = true` takes effect, so the
                    // servers have to be started here: without this the switch
                    // flips and nothing else happens until the next restart.
                    self.notify_lsp_open(id);
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
