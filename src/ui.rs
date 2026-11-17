//! Terminal rendering: panes, text, gutter, highlights, status area and overlays.
//!
//! The renderer is a pure function of a [`Screen`] snapshot, so the whole UI can
//! be tested against a [`ratatui::buffer::Buffer`] without a terminal.  The app
//! builds a `Screen`, calls [`draw`], and uses the returned cursor position.
//!
//! Text rendering works in three steps per pane:
//! 1. map buffer lines to *visual rows* (one row per line, or several when
//!    `[editor] wrap = true`),
//! 2. compute a style per character for each logical line — syntax highlighting,
//!    then cursor line, selection, search matches and diagnostics layered on top,
//! 3. emit runs of equal style, expanding tabs to the configured tab stop.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;

use crate::action::VisualKind;
use crate::buffer::Buffer as TextBuffer;
use crate::config::{BorderStyle, Config, LineNumber, NoticePosition};
use crate::diagnostic::{Diagnostic, DiagnosticTag, Severity};
use crate::editor::{Editor, StatusLevel};
use crate::prompt;
use crate::selection::{Selection, Span};
use crate::status::{self, Snapshot};
use crate::syntax;
use crate::theme::Theme;

/// Most rows a picker preview may take.  A preview is a glance at the selected
/// row, not a document viewer: past a few lines it would crowd out the list it
/// is meant to explain.
const MAX_PREVIEW_ROWS: usize = 6;

/// Transient text drawn on a card over the panes: an open prompt, a message or
/// the echo of a partial key sequence.
///
/// This replaces the command row the status area used to carry.  One type covers
/// all three because they are the same thing on screen — one line of text that is
/// true only for this moment — and because drawing them in one place is what
/// keeps the status row from ever being overwritten.
#[derive(Debug, Clone, Copy)]
pub struct Notice<'a> {
    /// Label drawn before the text: a prompt's `:` or `/`, empty for a message.
    pub prompt: &'a str,
    pub text: &'a str,
    /// Caret as a character index into `text`; `Some` means the card is being
    /// typed into, so the terminal cursor goes there.
    pub caret: Option<usize>,
    /// Ghost text from tab completion, drawn dimmed after the caret.
    pub suffix: &'a str,
    /// How the text is coloured — a message's severity, or prompt styling.
    pub tone: NoticeTone,
}

/// Colouring of a [`Notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeTone {
    Prompt,
    Info,
    Warn,
    Error,
}

/// A fuzzy picker drawn over the panes.
pub struct Picker<'a> {
    pub title: &'a str,
    /// Drawn before the typed text: `:` while a command or one of its parameters
    /// is being written, `>` for every other list.  The ex-style glyph is what
    /// makes the `:` popup read like the line it replaced.
    pub prompt: &'a str,
    pub input: &'a str,
    pub caret: usize,
    pub suffix: &'a str,
    /// Indices into `items`, already filtered and ordered.
    pub visible: &'a [usize],
    pub items: &'a [prompt::Item],
    /// Index into `visible` of the highlighted row.
    pub selected: usize,
    /// The selected candidate's file, shown syntax-highlighted on the right
    /// half of the card.  Only the search pickers populate it; a plain list
    /// keeps the full-width layout.
    pub preview: Option<FilePreview<'a>>,
}

/// A file preview drawn beside the search pickers' candidate list.
///
/// The runs are pre-highlighted by the app (the renderer must not read files or
/// parse), so this is just text split into styled segments, one entry per line.
pub struct FilePreview<'a> {
    /// Path shown above the pane, relative to the search root where possible.
    pub path: &'a str,
    /// One row of runs per file line; each run is `(text, highlight group)`.
    pub lines: &'a [Vec<(String, syntax::Hl)>],
    /// Index into `lines` of the line to mark and scroll to (the grep hit).
    pub focus: usize,
}

/// A completion popup drawn next to the caret.
pub struct CompletionView<'a> {
    /// The popup state: candidates, the filtered order and the selection.
    pub completion: &'a crate::completion::Completion,
}

/// The full message of the diagnostic under the caret, shown on a card.
pub struct DiagnosticFloat<'a> {
    pub diagnostic: &'a Diagnostic,
}

/// Everything the renderer needs for one frame.
pub struct Screen<'a> {
    pub editor: &'a Editor,
    pub config: &'a Config,
    pub theme: &'a Theme,
    pub status: Snapshot,
    /// The prompt being typed into, if one is open.
    pub prompt: Option<Notice<'a>>,
    pub picker: Option<Picker<'a>>,
    pub completion: Option<CompletionView<'a>>,
    pub diagnostic_float: Option<DiagnosticFloat<'a>>,
    /// What the language server said about the symbol under the caret (`K`).
    pub hover: Option<&'a str>,
}

impl<'a> Screen<'a> {
    /// The one transient line this frame has to show, if it has one.
    ///
    /// A prompt being typed into wins over everything, then the echo of a key
    /// sequence that is still waiting for its rest, then a message.  They cannot
    /// genuinely overlap — typing a key dismisses a message — but the order says
    /// which one does if they ever do, and it is the order that put the newest,
    /// most actionable text on top.
    pub fn notice(&self) -> Option<Notice<'_>> {
        if let Some(prompt) = &self.prompt {
            return Some(Notice {
                prompt: prompt.prompt,
                text: prompt.text,
                caret: prompt.caret,
                suffix: prompt.suffix,
                tone: prompt.tone,
            });
        }
        if !self.status.echo.is_empty() {
            return Some(Notice {
                prompt: "",
                text: &self.status.echo,
                caret: None,
                suffix: "",
                tone: NoticeTone::Prompt,
            });
        }
        let (level, message) = self.status.message.as_ref()?;
        Some(Notice {
            prompt: "",
            text: message,
            caret: None,
            suffix: "",
            tone: match level {
                StatusLevel::Info => NoticeTone::Info,
                StatusLevel::Warn => NoticeTone::Warn,
                StatusLevel::Error => NoticeTone::Error,
            },
        })
    }
}

/// Split the terminal into the status row and the panes.
///
/// The status area is exactly one row — there is no command or message row of
/// its own any more.  Everything transient (messages, key echoes, prompts) is
/// drawn on a card floating over the panes, so the row never has to be cleared
/// and restored, and `[status] position` only decides which edge it sits on.
pub fn areas(area: Rect, config: &Config) -> (Rect, Rect) {
    let status_height = 1.min(area.height);
    if config.status.position.is_top() {
        (
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: status_height,
            },
            Rect {
                x: area.x,
                y: area.y + status_height,
                width: area.width,
                height: area.height.saturating_sub(status_height),
            },
        )
    } else {
        (
            Rect {
                x: area.x,
                y: area.bottom().saturating_sub(status_height),
                width: area.width,
                height: status_height,
            },
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height.saturating_sub(status_height),
            },
        )
    }
}

/// Draw one frame; the return value is where the terminal cursor should sit.
pub fn draw(frame: &mut Frame, screen: &Screen) -> Option<Position> {
    let area = frame.area();
    let (status_area, pane_area) = areas(area, screen.config);
    let buf = frame.buffer_mut();
    buf.set_style(area, screen.theme.text);

    let mut cursor = draw_panes(buf, pane_area, screen);

    status::render(
        status_area,
        buf,
        &screen.config.status,
        &screen.status,
        screen.theme,
    );

    // Transient text — a message or the echo of a partial key sequence — floats on
    // a card over the panes at `[status] notice-position`.  The status row keeps
    // its own contents whatever happens, which is the point of not having a
    // command row: nothing has to be cleared and restored, and the file you are
    // reading is only ever covered for as long as the message lasts.  While a
    // list card is open, a bottom-anchored notice flips to the top edge so a
    // warning is not buried under the thing that caused it.
    if let Some(card) = screen.notice() {
        let place = if screen.picker.is_some() {
            screen.config.status.notice_position.avoid_picker()
        } else {
            screen.config.status.notice_position
        };
        let status_on_top = screen.config.status.position.is_top();
        if let Some(position) =
            draw_notice(buf, pane_area, &card, place, status_on_top, screen.theme)
        {
            cursor = Some(position);
        }
    }
    if let Some(picker) = &screen.picker {
        if let Some(position) = draw_picker(buf, area, picker, screen.theme) {
            cursor = Some(position);
        }
    }
    if let Some(completion) = &screen.completion {
        // The popup hangs off the caret, so it needs the position the panes
        // just computed; without one (no focused pane) there is nowhere to put
        // it.  It is clipped to the pane area so it never covers the status row.
        if let Some(anchor) = cursor {
            draw_completion(buf, pane_area, anchor, completion, screen.theme);
        }
    }
    if let Some(float) = &screen.diagnostic_float {
        // The float sits above the caret's line so it never covers the text the
        // user is reading, and it is drawn last so it wins over the popup.
        if let Some(anchor) = cursor {
            draw_diagnostic_float(buf, pane_area, anchor, float, screen.theme);
        }
    }
    if let Some(hover) = screen.hover {
        // Hover is asked for explicitly (`K`), so it is drawn over everything
        // else: the user is waiting for this answer.
        if let Some(anchor) = cursor {
            draw_hover(buf, pane_area, anchor, hover, screen.theme);
        }
    }
    cursor
}

// ---------------------------------------------------------------------- panes

fn draw_panes(buf: &mut Buffer, area: Rect, screen: &Screen) -> Option<Position> {
    let layout = &screen.editor.layout;
    let mut cursor = None;
    let panes = layout.panes();
    if panes.is_empty() || area.width == 0 || area.height == 0 {
        draw_welcome(buf, area, screen);
        return None;
    }
    for (pane_id, pane_area) in layout.rects(area) {
        let Some(pane) = layout.pane(pane_id) else {
            continue;
        };
        let focused = layout.is_focused(pane_id);
        match screen.editor.buffer(pane.buffer) {
            Some(buffer) => {
                if let Some(position) = draw_pane(buf, pane_area, buffer, pane_id, focused, screen)
                {
                    if focused {
                        cursor = Some(position);
                    }
                }
            }
            None => {
                let (content, _) = draw_frame(buf, pane_area, "", "", screen, focused);
                write_centered(buf, content, "no buffer", screen.theme.status_inactive);
            }
        }
    }
    cursor
}

fn draw_welcome(buf: &mut Buffer, area: Rect, screen: &Screen) {
    const LINES: &[&str] = &[
        "vortex",
        "",
        "a modal, async code editor",
        "",
        "no keys are bound until your config binds them",
        "vortex --write-config   the vim starter, to edit",
        ":edit <file>            open a file",
        ":help                   list commands",
    ];
    let start = area.y + area.height.saturating_sub(LINES.len() as u16) / 2;
    for (index, line) in LINES.iter().enumerate() {
        let y = start + index as u16;
        if y >= area.bottom() {
            break;
        }
        let style = if index == 0 {
            screen.theme.label_focused
        } else {
            screen.theme.status_inactive
        };
        let width = line.chars().count() as u16;
        buf.set_stringn(
            area.x + area.width.saturating_sub(width) / 2,
            y,
            *line,
            area.width as usize,
            style,
        );
    }
}

fn write_centered(buf: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = text.chars().count() as u16;
    buf.set_stringn(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height / 2,
        text,
        area.width as usize,
        style,
    );
}

/// Draw one pane: border, gutter, text and cursor position.
fn draw_pane(
    buf: &mut Buffer,
    area: Rect,
    buffer: &TextBuffer,
    pane_id: crate::layout::PaneId,
    focused: bool,
    screen: &Screen,
) -> Option<Position> {
    let config = screen.config;
    let theme = screen.theme;
    let editor = screen.editor;
    let pane = editor.layout.pane(pane_id)?;

    let (content, _) = draw_frame(
        buf,
        area,
        &buffer.display_name(),
        &pane_flags(buffer, editor, focused),
        screen,
        focused,
    );
    if content.width == 0 || content.height == 0 {
        return None;
    }

    let tab_width = config.editor.tab_width_for(buffer.language.id);
    let line_count = buffer.display_line_count();
    let scroll = pane.view.scroll_line.min(line_count.saturating_sub(1));
    let cursor = pane.cursor();
    let cursor_line = buffer.line_of(cursor);
    let cursor_char = buffer.char_in_line(cursor);

    let gutter = Gutter::new(config, buffer, cursor_line, focused);
    let text_width = content.width.saturating_sub(gutter.width as u16).max(1) as usize;

    let highlights = highlight_runs(
        buffer,
        scroll,
        content.height as usize,
        text_width,
        config,
        theme,
    );
    let selection = if focused {
        SelectionView::new(editor, &pane.selection)
    } else {
        SelectionView::none()
    };
    // Markdown is decorated in place: the glyphs painted differ from the buffer's,
    // one for one, and every character carries its own base style.  Everything that
    // follows still measures the *source* text, which is what keeps the caret, the
    // wrap points and click hit-testing in buffer coordinates.
    let decor = decorations(
        buffer,
        config,
        theme,
        scroll,
        content.height as usize,
        focused.then_some(cursor_line),
    );

    let rows = visual_rows(
        buffer,
        scroll,
        content.height as usize,
        text_width,
        tab_width,
        config.editor.wrap,
        decor.as_ref(),
    );

    let mut cursor_position = None;
    for (row, slot) in rows.iter().enumerate() {
        let y = content.y + row as u16;
        if y >= content.bottom() {
            break;
        }
        let Some(line) = slot.line else {
            // Past the end of the buffer: a `~` in the gutter column.
            gutter.render(buf, content.x, y, None, theme);
            buf.set_stringn(
                content.x + gutter.width as u16,
                y,
                "~",
                1,
                theme.status_inactive,
            );
            continue;
        };

        gutter.render(buf, content.x, y, Some(line), theme);

        let text = line_text(buffer, line);
        let decorated = decor.as_ref().and_then(|decor| decor.get(line));
        // A reflowed table row arrives as its grid: one row of cells wider or
        // narrower than the characters it was written with, addressed through
        // `LineRender::columns` instead of cell-equals-character.  Every other
        // decorated line is the same character count by construction; the
        // fallback covers a buffer edited between planning and drawing.
        let grid = decorated.filter(|render| render.reflowed());
        let painted = match decorated {
            Some(render) if grid.is_some() || render.glyphs.len() == text.chars().count() => {
                render.display()
            }
            _ => text.clone(),
        };
        let layers = Layers {
            highlights: &highlights,
            selection: &selection,
            decorated,
        };
        let styles = line_styles(buffer, line, &text, &layers, screen, focused);
        let x = content.x + gutter.width as u16;
        let limit = content.right().saturating_sub(x) as usize;
        let paint = LinePaint {
            x,
            y,
            limit,
            tab_width,
        };
        // A grid row is drawn whole, its columns already placed by the plan; a
        // normal row slices `painted` by character index exactly as before, since
        // for it cell equals character.
        let (chunk_start, chunk_end) = match grid {
            Some(_) => (0usize, painted.chars().count()),
            None => (slot.start, slot.end),
        };
        render_chunk(buf, &paint, &painted, chunk_start, chunk_end, &styles);

        // The message for the line's worst diagnostic, dimmed after the text.
        // Only on the last visual row of the line, so a wrapped line does not
        // repeat it on every chunk.
        if config.editor.inline_diagnostics && slot.end >= text.chars().count() {
            if let Some(diagnostic) = inline_diagnostic(buffer, line) {
                let column = match grid {
                    Some(render) => render.column_of(text.chars().count()),
                    None => display_column(&text, slot.start, slot.end, slot.end, tab_width),
                };
                let message = format!("  {}", diagnostic.message);
                emit(buf, x, y, &message, theme.status_inactive, column, limit);
            }
        }

        if focused && line == cursor_line && cursor_in_row(buffer, &text, cursor_char, slot) {
            let column = match grid {
                Some(render) => render.column_of(cursor_char),
                None => display_column(&text, slot.start, slot.end, cursor_char, tab_width),
            };
            if column < limit {
                cursor_position = Some(Position {
                    x: x + column as u16,
                    y,
                });
            } else {
                cursor_position = Some(Position {
                    x: content.right().saturating_sub(1),
                    y,
                });
            }
        }
    }
    cursor_position
}

/// The palette markdown decoration paints with, from the active theme.
///
/// Heading levels borrow the syntax groups a code editor already colours loudly
/// (keyword / function / type) and inline code takes the string colour, so decorated
/// markdown reads in the same palette as highlighted code rather than inventing a
/// second one.
fn markdown_styles(theme: &Theme) -> crate::markdown::MarkdownStyles {
    use crate::syntax::Hl;
    let base = theme.text;
    crate::markdown::MarkdownStyles {
        text: base,
        heading1: theme.highlight(Hl::Keyword),
        heading2: theme.highlight(Hl::Function),
        heading3: theme.highlight(Hl::Type),
        bold: base.add_modifier(Modifier::BOLD),
        italic: base.add_modifier(Modifier::ITALIC),
        strike: base.add_modifier(Modifier::CROSSED_OUT),
        code: theme.highlight(Hl::String),
        link: base.add_modifier(Modifier::UNDERLINED),
        marker: theme.status_inactive,
        quote: theme.highlight(Hl::Comment),
        bullet: base.add_modifier(Modifier::BOLD),
        list_number: theme.status_inactive,
        checkbox: base.add_modifier(Modifier::BOLD),
        rule: theme.status_inactive,
        table_border: theme.status_inactive,
        table_header: base.add_modifier(Modifier::BOLD),
    }
}

/// Decorate a markdown buffer's visible rows, or `None` when this buffer is not one.
///
/// The whole text is read because an open fence or a table header can sit above the
/// viewport; only the drawn window allocates glyphs.  Gutter, wrapping and cursor
/// arithmetic deliberately still run on the *source* characters: decoration is
/// one-for-one in column width, which is exactly what keeps the caret over the
/// glyph it refers to.  The line under the cursor (`active_line`) reveals its own
/// markup as written, so the characters being edited are never hidden.
fn decorations(
    buffer: &TextBuffer,
    config: &Config,
    theme: &Theme,
    scroll: usize,
    height: usize,
    active_line: Option<usize>,
) -> Option<crate::markdown::Decorated> {
    if !config.editor.markdown_render || buffer.language.id != "markdown" {
        return None;
    }
    // Classifying the document means reading all of it, every frame.  A file past
    // the size that already skips background parsing and formatting is far from the
    // README this was built for, so it draws as source rather than cost a copy per
    // keystroke — the same bargain `[editor] large-file-threshold` already strikes.
    if buffer.len_bytes() > config.editor.large_file_threshold {
        return None;
    }
    let styles = markdown_styles(theme);
    Some(crate::markdown::decorate_window(
        &buffer.text(),
        scroll..scroll.saturating_add(height).saturating_add(1),
        active_line,
        &styles,
    ))
}

/// A visual row: a buffer line plus the character range shown on that row.
#[derive(Debug, Clone, Copy)]
struct RowSlot {
    line: Option<usize>,
    start: usize,
    end: usize,
}

/// Map buffer lines to visual rows, wrapping when `[editor] wrap` is on.
///
/// A reflowed table row always takes exactly one row: its columns were laid out
/// as a grid, and wrapping the second half under text it no longer borders would
/// undo the alignment that made the row worth reflowing.  Off-pane surplus
/// clips at the edge, which is what a too-wide table does unwrapped anyway.
fn visual_rows(
    buffer: &TextBuffer,
    scroll: usize,
    height: usize,
    width: usize,
    tab_width: usize,
    wrap: bool,
    decor: Option<&crate::markdown::Decorated>,
) -> Vec<RowSlot> {
    let mut rows = Vec::with_capacity(height);
    let mut line = scroll;
    // A trailing line break does not make an extra line: `~` goes there, as in vim.
    let lines = buffer.display_line_count();
    while rows.len() < height {
        if line >= lines {
            rows.push(RowSlot {
                line: None,
                start: 0,
                end: 0,
            });
            continue;
        }
        let text = line_text(buffer, line);
        if !wrap || decor.is_some_and(|d| d.get(line).is_some_and(|r| r.reflowed())) {
            rows.push(RowSlot {
                line: Some(line),
                start: 0,
                end: text.chars().count(),
            });
            line += 1;
            continue;
        }
        let chunks = char_chunks(&text, width, tab_width);
        for (start, end) in chunks {
            if rows.len() >= height {
                break;
            }
            rows.push(RowSlot {
                line: Some(line),
                start,
                end,
            });
        }
        line += 1;
    }
    rows
}

/// Split a line into character ranges that fit `width` columns.
fn char_chunks(text: &str, width: usize, tab_width: usize) -> Vec<(usize, usize)> {
    if width == 0 {
        return vec![(0, text.chars().count())];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut column = 0usize;
    for (index, character) in text.chars().enumerate() {
        let advance = if character == '\t' {
            tab_width - (column % tab_width.max(1))
        } else {
            1
        };
        if column + advance > width && index > start {
            chunks.push((start, index));
            start = index;
            column = 0;
        }
        column += advance;
    }
    chunks.push((start, text.chars().count()));
    chunks
}

/// Does the cursor sit on this visual row?
fn cursor_in_row(buffer: &TextBuffer, text: &str, cursor_char: usize, slot: &RowSlot) -> bool {
    let count = text.chars().count();
    // A cursor at the end of a wrapped line belongs to the last chunk.
    if cursor_char >= slot.end && slot.end == count {
        return true;
    }
    let _ = buffer;
    cursor_char >= slot.start && cursor_char < slot.end
}

/// Column of `cursor_char` within the chunk `[start, end)`, tabs expanded.
fn display_column(
    text: &str,
    start: usize,
    _end: usize,
    cursor_char: usize,
    tab_width: usize,
) -> usize {
    let mut column = 0usize;
    for (index, character) in text.chars().enumerate() {
        if index >= cursor_char {
            break;
        }
        let advance = if character == '\t' {
            tab_width - (column % tab_width.max(1))
        } else {
            1
        };
        if index < start {
            // Wrapped continuation rows start their own column count.
            column = 0;
        } else {
            column += advance;
        }
    }
    column
}

fn line_text(buffer: &TextBuffer, line: usize) -> String {
    // The trailing newline belongs to the line, but never to the rendered row.
    buffer
        .line_str(line)
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

/// The diagnostic whose message belongs at the end of `line`, if any.
///
/// A line can carry several; the worst one wins, so an error is never hidden
/// behind a hint that happens to start earlier.  Ties go to the earliest span,
/// which keeps the choice stable between frames.
fn inline_diagnostic(buffer: &TextBuffer, line: usize) -> Option<&Diagnostic> {
    let line_start = buffer.line_start(line);
    let line_end = buffer.line_end(line);
    buffer
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.range.start <= line_end && diagnostic.range.end >= line_start
        })
        .max_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then(b.range.start.cmp(&a.range.start))
        })
}

/// What paints on top of a line's base colour, in the order it is layered.
struct Layers<'a> {
    /// Syntax highlighting runs, empty for a language with no grammar.
    highlights: &'a [(Span, Style)],
    /// The visual selection, when this pane has one showing.
    selection: &'a SelectionView,
    /// Markdown decoration, which supplies the per-character base instead of the
    /// flat text colour.
    decorated: Option<&'a crate::markdown::LineRender>,
}

/// One style per drawn cell of a logical line.
///
/// Normal cells *are* characters, so the decoration's per-character styles pass
/// straight through.  A reflowed table row is wider than its character count:
/// there each drawn column resolves to whichever source character owns it (see
/// [`crate::markdown::LineRender::owners`]), and every layer below — cursor
/// line, highlights, selection, search matches, diagnostics — then reads that
/// character's byte offset as it always has.  Padding inherits its neighbour's
/// layers, which is what makes a selection or a spell squiggle stretch across
/// the blank a column grew by rather than stopping short of it.
fn line_styles(
    buffer: &TextBuffer,
    line: usize,
    text: &str,
    layers: &Layers<'_>,
    screen: &Screen,
    focused: bool,
) -> Vec<Style> {
    let Layers {
        highlights,
        selection,
        decorated,
    } = *layers;
    let theme = screen.theme;
    let base = theme.text;
    let line_start = buffer.line_start(line);
    let characters: Vec<char> = text.chars().collect();

    // Byte offset of every character, computed in one pass.
    let mut char_offsets = Vec::with_capacity(characters.len());
    let mut byte = 0usize;
    for character in &characters {
        char_offsets.push(line_start + byte);
        byte += character.len_utf8();
    }

    // Decorated markdown brings its own per-character base, which the layers below
    // then patch — so a selection or a search match still wins over a heading.
    let grid = decorated.filter(|render| render.reflowed());
    let (mut styles, offsets): (Vec<Style>, Vec<usize>) = match grid {
        Some(render) => (
            render.styles.clone(),
            (0..render.glyphs.len())
                .map(|column| {
                    let owner = render.character_at(column).min(characters.len());
                    char_offsets
                        .get(owner)
                        .copied()
                        .unwrap_or(line_start + byte)
                })
                .collect(),
        ),
        None => {
            let decorated = decorated.filter(|render| render.styles.len() == characters.len());
            (
                match decorated {
                    Some(render) => render.styles.clone(),
                    None => vec![base; characters.len()],
                },
                char_offsets,
            )
        }
    };
    if offsets.is_empty() {
        return styles;
    }
    if screen.config.editor.cursorline && focused && selection.cursor_line == Some(line) {
        for style in styles.iter_mut() {
            *style = style.patch(theme.cursor_line);
        }
    }

    for (index, style) in styles.iter_mut().enumerate() {
        let at = offsets[index];
        if let Some((_, run)) = highlights.iter().find(|(span, _)| span.contains(at)) {
            *style = style.patch(*run);
        }
    }

    for span in &selection.spans {
        let style = match selection.kind {
            Some(VisualKind::Block) => theme.selection,
            _ => theme.selection,
        };
        for (index, entry) in styles.iter_mut().enumerate() {
            let at = offsets[index];
            if span.contains(at) {
                *entry = entry.patch(style);
            }
        }
    }

    if screen.config.search.highlight {
        for span in &screen.editor.search_matches {
            for (index, entry) in styles.iter_mut().enumerate() {
                let at = offsets[index];
                if span.contains(at) {
                    *entry = entry.patch(theme.search_match);
                }
            }
        }
    }

    for diagnostic in &buffer.diagnostics {
        let style = diagnostic_style(diagnostic, theme);
        if diagnostic.range.end <= line_start || diagnostic.range.start > line_start + text.len() {
            continue;
        }
        for (index, entry) in styles.iter_mut().enumerate() {
            let at = offsets[index];
            if diagnostic.range.contains(at) {
                *entry = entry.patch(style);
            }
        }
    }

    styles
}

/// Where a line of text is drawn, and how far it may run.
struct LinePaint {
    x: u16,
    y: u16,
    /// Columns available before the edge of the pane.
    limit: usize,
    /// Spaces a tab advances to reach the next stop.
    tab_width: usize,
}

/// Emit one visual row, expanding tabs and coalescing equal styles.
fn render_chunk(
    buf: &mut Buffer,
    paint: &LinePaint,
    text: &str,
    start: usize,
    end: usize,
    styles: &[Style],
) {
    let x = paint.x;
    let y = paint.y;
    let limit = paint.limit;
    let tab_width = paint.tab_width;
    let mut column = 0usize;
    let mut run = String::new();
    let mut run_style: Option<Style> = None;

    for (index, character) in text.chars().enumerate() {
        if index < start {
            continue;
        }
        if index >= end {
            break;
        }
        let style = styles.get(index).copied().unwrap_or(Style::default());
        if run_style != Some(style) && !run.is_empty() {
            column += emit(
                buf,
                x,
                y,
                &run,
                run_style.unwrap_or_default(),
                column,
                limit,
            );
            run.clear();
        }
        run_style = Some(style);
        if character == '\t' {
            let width = tab_width - (column % tab_width.max(1));
            let pad = " ".repeat(width.min(limit.saturating_sub(column)).max(1));
            column += emit(buf, x, y, &pad, style, column, limit);
        } else {
            run.push(if character.is_control() {
                '?'
            } else {
                character
            });
        }
        if column + run.chars().count() >= limit {
            break;
        }
    }
    if !run.is_empty() {
        emit(
            buf,
            x,
            y,
            &run,
            run_style.unwrap_or_default(),
            column,
            limit,
        );
    }
}

/// Write a run of text at the given column, returning the columns consumed.
fn emit(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
    column: usize,
    limit: usize,
) -> usize {
    let available = limit.saturating_sub(column);
    if available == 0 || text.is_empty() {
        return 0;
    }
    let width = text.chars().count().min(available);
    buf.set_stringn(x + column as u16, y, text, width, style);
    width
}

/// Blank a cell to `symbol` under exactly one style.
///
/// `Cell::set_style` patches (it keeps the old colours and modifiers unless the
/// new style subtracts them), so painting an overlay straight over the buffer
/// leaves the buffer's styling — a spellcheck or markdown underline especially —
/// showing through.  Resetting first gives the overlay cell a clean slate; call
/// this for every cell the overlay claims before writing text over it.
fn paint_cell(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    let cell = &mut buf[(x, y)];
    cell.reset();
    cell.set_symbol(symbol).set_style(style);
}

// ------------------------------------------------------------------- gutter

struct Gutter {
    width: usize,
    kind: LineNumber,
    cursor_line: usize,
    /// One entry per line: the worst diagnostic on it, if any.
    signs: Vec<Option<Severity>>,
    /// Columns reserved to the left of the numbers for the sign column.
    sign_width: usize,
}

impl Gutter {
    /// A gutter with `width == 0` means "no line numbers at all".
    fn new(config: &Config, buffer: &TextBuffer, cursor_line: usize, focused: bool) -> Self {
        let kind = match config.editor.line_number {
            LineNumber::None => LineNumber::None,
            // Relative numbers only make sense in the focused pane; others show
            // the absolute line so they stay readable.
            LineNumber::Relative if !focused => LineNumber::Absolute,
            other => other,
        };
        let total_lines = buffer.display_line_count();
        let signs = if config.editor.diagnostic_signs {
            sign_map(buffer)
        } else {
            Vec::new()
        };
        // Only reserve the column when something is actually marked, so a clean
        // buffer keeps the full text width.
        let sign_width = usize::from(signs.iter().any(Option::is_some));
        let width = if kind == LineNumber::None {
            sign_width
        } else {
            digits(total_lines) + 1 + sign_width
        };
        Self {
            width,
            kind,
            cursor_line,
            signs,
            sign_width,
        }
    }

    fn number_for(&self, line: usize) -> String {
        match self.kind {
            LineNumber::None => String::new(),
            LineNumber::Absolute => (line + 1).to_string(),
            LineNumber::Relative => {
                if line == self.cursor_line {
                    (line + 1).to_string()
                } else {
                    (line as isize - self.cursor_line as isize)
                        .abs()
                        .to_string()
                }
            }
        }
    }

    fn render(&self, buf: &mut Buffer, x: u16, y: u16, line: Option<usize>, theme: &Theme) {
        if self.width == 0 {
            return;
        }
        let Some(line) = line else {
            return;
        };
        if self.sign_width > 0 {
            let sign = self.signs.get(line).copied().flatten();
            let (symbol, style) = match sign {
                Some(severity) => (
                    severity_sign(severity),
                    severity_style(severity, theme).remove_modifier(Modifier::UNDERLINED),
                ),
                None => (' ', theme.gutter),
            };
            buf.set_stringn(x, y, symbol.to_string(), 1, style);
        }
        if self.kind == LineNumber::None {
            return;
        }
        let text = self.number_for(line);
        let style = if line == self.cursor_line {
            theme.gutter_active
        } else {
            theme.gutter
        };
        let right = x + self.width as u16 - 1;
        buf.set_stringn(
            right.saturating_sub(text.chars().count() as u16),
            y,
            text,
            self.width - self.sign_width,
            style,
        );
    }
}

/// The worst diagnostic severity on each line, for the sign column.
///
/// A diagnostic that spans several lines marks every line it covers, so a
/// multi-line error is visible wherever the user scrolls to.
fn sign_map(buffer: &TextBuffer) -> Vec<Option<Severity>> {
    let mut signs: Vec<Option<Severity>> = vec![None; buffer.display_line_count()];
    for diagnostic in &buffer.diagnostics {
        let first = buffer.line_of(diagnostic.range.start);
        let last = buffer.line_of(diagnostic.range.end);
        for line in first..=last.min(signs.len().saturating_sub(1)) {
            let slot = &mut signs[line];
            *slot = Some(match *slot {
                Some(existing) => existing.max(diagnostic.severity),
                None => diagnostic.severity,
            });
        }
    }
    signs
}

/// The one-character marker for a severity in the sign column.
fn severity_sign(severity: Severity) -> char {
    match severity {
        Severity::Error => '●',
        Severity::Warning => '▲',
        Severity::Info => '■',
        Severity::Hint => '○',
    }
}

fn digits(value: usize) -> usize {
    let mut value = value.max(1);
    let mut count = 0;
    while value > 0 {
        value /= 10;
        count += 1;
    }
    count
}

// ------------------------------------------------------------- highlighting

/// Syntax highlight runs as (byte span, style) for the visible slice.
fn highlight_runs(
    buffer: &TextBuffer,
    scroll: usize,
    height: usize,
    _text_width: usize,
    config: &Config,
    theme: &Theme,
) -> Vec<(Span, Style)> {
    if !buffer.language.has_tree_sitter() {
        return Vec::new();
    }
    if buffer.len_bytes() > config.editor.large_file_threshold {
        return Vec::new();
    }
    let last_line = (scroll + height + 8).min(buffer.last_line());
    let start = buffer.line_start(scroll.min(buffer.last_line()));
    let end = buffer.line_range_with_ending(last_line).end;
    syntax::highlights(buffer, Span::new(start, end), buffer.language.id)
        .into_iter()
        .map(|highlight| (highlight.span, theme.highlight(highlight.hl)))
        .collect()
}

/// The style a diagnostic paints its span with.
///
/// The severity picks the colour; the tags change the shape of the mark, so an
/// unused binding reads as "faded" and a deprecated one as "struck through"
/// without having to read the message.
fn diagnostic_style(diagnostic: &Diagnostic, theme: &Theme) -> Style {
    let mut style = match diagnostic.severity {
        Severity::Error => theme.diagnostic_error,
        Severity::Warning => theme.diagnostic_warning,
        Severity::Info => theme.diagnostic_info,
        Severity::Hint => theme.diagnostic_hint,
    };
    if diagnostic.has_tag(DiagnosticTag::Unnecessary) {
        style = style.add_modifier(Modifier::DIM);
    }
    if diagnostic.has_tag(DiagnosticTag::Deprecated) {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}

/// The style for a severity alone, for the gutter sign column.
fn severity_style(severity: Severity, theme: &Theme) -> Style {
    match severity {
        Severity::Error => theme.diagnostic_error,
        Severity::Warning => theme.diagnostic_warning,
        Severity::Info => theme.diagnostic_info,
        Severity::Hint => theme.diagnostic_hint,
    }
}

/// The highlighted byte spans of the focused pane, resolved once per frame.
struct SelectionView {
    spans: Vec<Span>,
    kind: Option<VisualKind>,
    cursor_line: Option<usize>,
}

impl SelectionView {
    fn none() -> Self {
        Self {
            spans: Vec::new(),
            kind: None,
            cursor_line: None,
        }
    }

    fn new(editor: &Editor, selection: &Selection) -> Self {
        let kind = editor.mode.visual_kind();
        let Some(buffer) = editor.current_buffer() else {
            return Self::none();
        };
        let cursor_line = buffer.line_of(selection.cursor());
        let mut spans = Vec::new();
        for span in selection.spans() {
            if span.is_empty() {
                continue;
            }
            match kind {
                // A characterwise visual selection covers the character under the
                // cursor too.
                Some(VisualKind::Char) => spans.push(Span::new(
                    span.start,
                    (span.end + 1).min(buffer.len_bytes()),
                )),
                Some(VisualKind::Line) => {
                    let first = buffer.line_start(buffer.line_of(span.start));
                    let last = buffer.line_range_with_ending(buffer.line_of(span.end)).end;
                    spans.push(Span::new(first, last));
                }
                _ => spans.push(*span),
            }
        }
        Self {
            spans,
            kind,
            cursor_line: Some(cursor_line),
        }
    }
}

// ------------------------------------------------------------------ borders

/// Draw a pane border and its label; returns the inner area.
fn draw_frame(
    buf: &mut Buffer,
    area: Rect,
    title: &str,
    flags: &str,
    screen: &Screen,
    focused: bool,
) -> (Rect, border::Set<'static>) {
    let theme = screen.theme;
    let set = screen.config.layout.border.symbols();
    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border
    };
    let plain = screen.config.layout.border == BorderStyle::None;
    let content = if plain || area.width < 2 || area.height < 2 {
        area
    } else {
        // The style goes on every cell of the frame, not just the top row:
        // a border that colours three sides differently from the fourth reads
        // as damage, not design.
        for x in area.left()..area.right() {
            buf[(x, area.y)]
                .set_symbol(set.horizontal_top)
                .set_style(border_style);
            buf[(x, area.bottom() - 1)]
                .set_symbol(set.horizontal_bottom)
                .set_style(border_style);
        }
        for y in area.top()..area.bottom() {
            buf[(area.x, y)]
                .set_symbol(set.vertical_left)
                .set_style(border_style);
            buf[(area.right() - 1, y)]
                .set_symbol(set.vertical_right)
                .set_style(border_style);
        }
        for (x, y, symbol) in [
            (area.x, area.y, set.top_left),
            (area.right() - 1, area.y, set.top_right),
            (area.x, area.bottom() - 1, set.bottom_left),
            (area.right() - 1, area.bottom() - 1, set.bottom_right),
        ] {
            buf[(x, y)].set_symbol(symbol).set_style(border_style);
        }
        Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width - 2,
            height: area.height - 2,
        }
    };

    if screen.config.layout.show_labels && !title.is_empty() && content.width > 4 {
        let label_style = if focused {
            theme.label_focused
        } else {
            theme.label
        };
        buf.set_stringn(
            area.x + 1,
            area.y,
            format!(" {title}{flags} "),
            content.width as usize,
            label_style,
        );
    }
    (content, set)
}

/// The markers shown next to a pane's file name.
fn pane_flags(buffer: &TextBuffer, editor: &Editor, focused: bool) -> String {
    let mut flags = String::new();
    if buffer.is_dirty() {
        flags.push('+');
    }
    if buffer.read_only {
        flags.push_str(" ro");
    }
    if !buffer.language.has_tree_sitter() {
        flags.push_str(" plain");
    }
    if buffer.syntax.status.is_pending() {
        flags.push_str(" parsing");
    }
    if focused && !editor.search_matches.is_empty() {
        flags.push_str(&format!(" {} match", editor.search_matches.len()));
    }
    flags
}

// ------------------------------------------------------------- hit testing

/// Which byte of the buffer sits at terminal `(column, row)` inside `pane_area`.
///
/// This mirrors the arithmetic in [`draw_pane`] — same gutter, same wrapped rows —
/// so a click lands exactly where the text was drawn.
pub fn byte_at(
    buffer: &TextBuffer,
    pane_area: Rect,
    pane: &crate::layout::Pane,
    config: &Config,
    theme: &Theme,
    column: u16,
    row: u16,
) -> Option<usize> {
    let content = pane_content(pane_area, config);
    if content.width == 0
        || content.height == 0
        || !pane_area.contains(Position { x: column, y: row })
    {
        return None;
    }
    let tab_width = config.editor.tab_width_for(buffer.language.id);
    let line_count = buffer.display_line_count();
    let scroll = pane.view.scroll_line.min(line_count.saturating_sub(1));
    let cursor_line = buffer.line_of(pane.selection.cursor());
    let gutter = Gutter::new(config, buffer, cursor_line, true);
    let text_width = content.width.saturating_sub(gutter.width as u16).max(1) as usize;
    let line_row = usize::from(row.checked_sub(content.y)?);

    // The same decoration map the pane was painted with: on a reflowed table
    // row the columns are not characters, and only the grid knows where each
    // one actually sits.  `None` for the caret line because a click never
    // reveals markup — it resolves against the geometry that is drawn.
    let decor = decorations(buffer, config, theme, scroll, content.height as usize, None);
    let rows = visual_rows(
        buffer,
        scroll,
        content.height as usize,
        text_width,
        tab_width,
        config.editor.wrap,
        decor.as_ref(),
    );
    let slot = *rows.get(line_row)?;
    let line = slot.line?;

    // Clicking the gutter selects the line's indentation.
    if column < content.x + gutter.width as u16 {
        return Some(buffer.first_non_blank(line));
    }

    let text = line_text(buffer, line);
    let characters: Vec<char> = text.chars().collect();
    let target = usize::from(column - (content.x + gutter.width as u16));
    // A reflowed row answers for itself which character owns the clicked cell;
    // anything past its drawn width clamps to the end of the line.
    let index = match decor
        .as_ref()
        .and_then(|decor| decor.get(line))
        .filter(|render| render.reflowed())
    {
        Some(render) => render.character_at(target.min(render.glyphs.len().saturating_sub(1))),
        None => {
            let mut displayed = 0usize;
            let mut index = slot.start.min(characters.len());
            while index < slot.end.min(characters.len()) {
                let character = characters[index];
                let advance = if character == '\t' {
                    tab_width - (displayed % tab_width)
                } else {
                    1
                };
                if displayed + advance > target {
                    break;
                }
                displayed += advance;
                index += 1;
            }
            index
        }
    };
    let byte = buffer.line_start(line)
        + characters[slot.start.min(characters.len())..index.max(slot.start)]
            .iter()
            .map(|character| character.len_utf8())
            .sum::<usize>();
    Some(buffer.clamp_byte(byte))
}

/// The area inside a pane's border.
pub fn pane_content(area: Rect, config: &Config) -> Rect {
    if config.layout.border == BorderStyle::None || area.width < 2 || area.height < 2 {
        area
    } else {
        Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width - 2,
            height: area.height - 2,
        }
    }
}

// ------------------------------------------------------------------ overlays

/// Draw one transient line — a prompt, an echo or a message — on a card
/// floating over the panes at `[status] notice-position`.
///
/// The card is one row tall inside its border: this text is read in a glance, and
/// a taller box would cover the file for no reason.  It is as wide as the text
/// needs up to most of the pane, so a long path or error still reads in one
/// piece while a short "42 lines yanked" leaves the buffer visible around it.
/// On the edge where the status row sits the card keeps one clear row between
/// its border and the bar — two borders stacked read as one broken line.
/// The return value is where the terminal cursor goes: inside the card when the
/// user is typing into it, nowhere otherwise.
fn draw_notice(
    buf: &mut Buffer,
    pane_area: Rect,
    notice: &Notice,
    place: NoticePosition,
    status_on_top: bool,
    theme: &Theme,
) -> Option<Position> {
    if pane_area.width < 8 || pane_area.height < 3 {
        return None;
    }
    let body = format!("{}{}{}", notice.prompt, notice.text, notice.suffix);
    let wanted = body.chars().count() + 6;
    let width = (wanted as u16)
        .min(pane_area.width.saturating_sub(2).max(8))
        .max(12.min(pane_area.width));
    // The clear row is only due on the status side, and only when the pane
    // has room for the card plus the gap; a clipped card is worse than a
    // flush one on a tiny terminal.
    let on_status_edge = (status_on_top && place.is_top_anchored())
        || (!status_on_top && place.is_bottom_anchored());
    let gap = u16::from(on_status_edge).min(pane_area.height.saturating_sub(3));
    let height = 3.min(pane_area.height - gap);
    let y = if place.is_top_anchored() {
        pane_area.y + gap
    } else if place.is_bottom_anchored() {
        pane_area.bottom().saturating_sub(height + gap)
    } else {
        // Left / Right: vertically centred, so no edge is shared with the bar.
        pane_area.y + pane_area.height.saturating_sub(height) / 2
    };
    let x = match place {
        NoticePosition::TopLeft | NoticePosition::BottomLeft | NoticePosition::Left => pane_area.x,
        NoticePosition::TopRight | NoticePosition::BottomRight | NoticePosition::Right => {
            pane_area.right().saturating_sub(width)
        }
        NoticePosition::Top | NoticePosition::Bottom => {
            pane_area.x + pane_area.width.saturating_sub(width) / 2
        }
    };
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    draw_card(buf, area, theme);

    let style = match notice.tone {
        NoticeTone::Prompt => theme.prompt,
        NoticeTone::Info => theme.text,
        NoticeTone::Warn => theme.diagnostic_warning,
        NoticeTone::Error => theme.diagnostic_error,
    };
    let text_x = area.x + 2;
    let limit = area.right().saturating_sub(text_x) as usize;
    let row = area.y + 1;
    for column in (area.x + 1)..area.right().saturating_sub(1) {
        buf[(column, row)].set_style(style);
    }
    emit(buf, text_x, row, notice.prompt, theme.prompt, 0, limit);
    let typed = notice.prompt.chars().count();
    emit(buf, text_x, row, notice.text, style, typed, limit);
    if !notice.suffix.is_empty() {
        emit(
            buf,
            text_x,
            row,
            notice.suffix,
            theme.status_inactive,
            typed + notice.text.chars().count(),
            limit,
        );
    }

    // Only a prompt has a caret; a message is read, not edited.
    let caret = notice.caret?;
    let column = text_x as usize + typed + caret.min(limit);
    Some(Position {
        x: (column as u16).min(area.right().saturating_sub(1)),
        y: row,
    })
}

/// Draw a fuzzy picker as a bordered box over the panes.
fn draw_picker(buf: &mut Buffer, screen: Rect, picker: &Picker, theme: &Theme) -> Option<Position> {
    if screen.width < 12 || screen.height < 5 {
        return None;
    }
    // A card that carries a file preview wants more of the screen than a plain
    // list does; without the extra width the two columns are useless.  The
    // renderer only splits at 90 columns (below), so do not widen for less.
    let width = if picker.preview.is_some() && screen.width >= 100 {
        screen.width as usize * 9 / 10
    } else {
        (screen.width as usize * 3 / 4).max(30.min(screen.width as usize))
    }
    .clamp(30.min(screen.width as usize), screen.width as usize)
    .min(screen.width as usize);
    // The selected row's preview, wrapped to the card.  It is computed before
    // the height so the card can reserve rows for it: a preview that pushed the
    // list off the bottom would be worse than no preview at all.  The cap also
    // leaves room for the border, the input row, one list row and the count
    // line, so a short terminal still shows a usable list.
    let max_preview = MAX_PREVIEW_ROWS.min((screen.height as usize).saturating_sub(8));
    let preview_lines = picker
        .visible
        .get(picker.selected)
        .and_then(|index| picker.items.get(*index))
        .and_then(|item| item.preview.as_deref())
        .filter(|_| max_preview > 0)
        .map(|text| wrap_preview(text, width.saturating_sub(4), max_preview))
        .unwrap_or_default();
    let preview_rows = preview_lines.len();
    // Leave room for the status row at the bottom: a card whose border sits
    // flush on the status line reads as part of it.  The card is centred in
    // what remains, so it sits where the eye already is rather than sliding
    // up the screen as the list grows; the height is capped at
    // `screen.height - 2` above, so even a maximum-height card keeps a clear
    // row between its bottom border and the status row.
    let list_rows = picker
        .visible
        .len()
        .min((screen.height as usize).saturating_sub(8 + preview_rows))
        .max(1);
    let height = (list_rows + 4 + preview_rows).min(screen.height.saturating_sub(2) as usize);
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width as u16) / 2,
        y: screen.y + screen.height.saturating_sub(height as u16) / 2,
        width: width as u16,
        height: height as u16,
    };
    if area.width < 8 || area.height < 4 {
        return None;
    }

    let set = border::ROUNDED;
    let style = theme.prompt;
    // Paint the whole card first: ratatui only rewrites cells the picker writes,
    // so without this the buffer text behind the overlay stays visible between
    // the lines of the list.  `paint_cell` also clears stale styles — see there.
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            paint_cell(buf, x, y, " ", theme.overlay);
        }
    }
    for x in area.left()..area.right() {
        buf[(x, area.y)].set_symbol(set.horizontal_top);
        buf[(x, area.bottom() - 1)].set_symbol(set.horizontal_bottom);
    }
    for y in area.top()..area.bottom() {
        buf[(area.x, y)].set_symbol(set.vertical_left);
        buf[(area.right() - 1, y)].set_symbol(set.vertical_right);
    }
    buf[(area.x, area.y)].set_symbol(set.top_left);
    buf[(area.right() - 1, area.y)].set_symbol(set.top_right);
    buf[(area.x, area.bottom() - 1)].set_symbol(set.bottom_left);
    buf[(area.right() - 1, area.bottom() - 1)].set_symbol(set.bottom_right);
    for row in area.rows() {
        buf.set_style(row, style);
    }
    buf.set_stringn(
        area.x + 2,
        area.y,
        format!(" {} ", picker.title),
        area.width.saturating_sub(2) as usize,
        theme.label_focused,
    );

    // A file preview turns the card into two columns: candidates on the left,
    // source on the right, divided by a vertical rule.  Narrow screens keep the
    // single-column layout — a preview three characters wide helps nobody.
    let show_preview = picker.preview.is_some() && area.width >= 90;
    let split_x = if show_preview {
        area.x + (area.width as usize * 45 / 100) as u16
    } else {
        area.right()
    };

    // Input row.  The glyph is the prompt itself — `:` for a command or one of
    // its parameters, `>` for a plain list — so the popup reads as the line it
    // replaced rather than as a search box.
    let input_y = area.y + 1;
    let glyph_width = picker.prompt.chars().count() as u16 + 1;
    buf.set_stringn(
        area.x + 2,
        input_y,
        picker.prompt,
        glyph_width as usize,
        theme.prompt,
    );
    let text_x = area.x + 2 + glyph_width;
    let limit = area.right().saturating_sub(text_x) as usize;
    emit(buf, text_x, input_y, picker.input, theme.text, 0, limit);
    let typed = picker.input.chars().count();
    if !picker.suffix.is_empty() {
        emit(
            buf,
            text_x,
            input_y,
            picker.suffix,
            theme.status_inactive,
            typed,
            limit,
        );
    }

    // Candidates.  The preview, when there is one, owns the rows just above the
    // count line, so the list stops short of them.
    let list_top = area.y + 2;
    let count_y = area.bottom().saturating_sub(1);
    let list_bottom = count_y.saturating_sub(preview_rows as u16);
    let rows = list_bottom.saturating_sub(list_top) as usize;
    // The window follows the selection.  A grep can return hundreds of hits and
    // only the first `rows` of them fit on screen, so without scrolling the
    // rest could be selected but never seen.
    let first_row = picker
        .selected
        .saturating_sub(rows.saturating_sub(1))
        .min(picker.visible.len().saturating_sub(1));
    for (row, item_index) in picker.visible[first_row..].iter().enumerate() {
        let y = list_top + row as u16;
        if y >= list_bottom {
            break;
        }
        let Some(item) = picker.items.get(*item_index) else {
            continue;
        };
        let selected = first_row + row == picker.selected;
        let base = if selected {
            theme
                .selection
                .patch(Style::default().add_modifier(Modifier::BOLD))
        } else {
            theme.text
        };
        for column in (area.x + 2)..split_x.min(area.right().saturating_sub(1)) {
            buf[(column, y)].set_style(if selected { theme.selection } else { style });
        }
        let label_limit = split_x.saturating_sub(area.x + 3) as usize;
        emit(buf, area.x + 3, y, &item.label, base, 0, label_limit);
        if let Some(detail) = &item.detail {
            let used = item.label.chars().count() + 2;
            if used + 4 < label_limit {
                emit(
                    buf,
                    area.x + 3,
                    y,
                    &format!("{}{}", " ".repeat(2), detail),
                    theme.status_inactive,
                    used,
                    label_limit,
                );
            }
        }
    }

    // Preview of the selected row, on the rows just above the count line.
    if preview_rows > 0 {
        let preview_top = count_y.saturating_sub(preview_rows as u16);
        let preview_limit = area.width.saturating_sub(4) as usize;
        for (row, line) in preview_lines.iter().enumerate() {
            let y = preview_top + row as u16;
            if y >= count_y {
                break;
            }
            // Clear the row first: the card was painted before the list, and a
            // shorter preview line must not leave the previous one behind.
            for column in (area.x + 2)..area.right().saturating_sub(1) {
                paint_cell(buf, column, y, " ", style);
            }
            emit(
                buf,
                area.x + 2,
                y,
                line,
                theme.status_inactive,
                0,
                preview_limit,
            );
        }
    }

    // The file-preview column: a rule, then the selected hit's file painted on
    // its right.  The app pre-highlights it (the renderer never reads disk), so
    // this only windows and colours the runs it is given.
    if let Some(preview) = picker.preview.as_ref().filter(|_| show_preview) {
        for y in list_top..count_y {
            buf[(split_x, y)]
                .set_symbol(set.vertical_left)
                .set_style(style);
        }
        draw_file_preview(buf, area, split_x, list_top, count_y, preview, theme);
    }

    // Count line.
    emit(
        buf,
        area.x + 2,
        area.bottom().saturating_sub(1),
        &format!(" {}/{} match", picker.selected + 1, picker.visible.len()),
        theme.status_inactive,
        0,
        area.width.saturating_sub(2) as usize,
    );

    Some(Position {
        x: text_x + typed.min(limit) as u16,
        y: input_y,
    })
}

/// Draw the selected file, syntax-highlighted, in the right-hand column of a
/// search picker's card.
///
/// The app pre-highlights the file (the renderer never reads disk or parses),
/// so this only windows the lines around the hit and paints the runs.  The
/// window follows `preview.focus` the way the candidate list follows its own
/// selection: the hit line must be on screen, everything else is context.
fn draw_file_preview(
    buf: &mut Buffer,
    area: Rect,
    split_x: u16,
    top: u16,
    bottom: u16,
    preview: &FilePreview,
    theme: &Theme,
) {
    let x = split_x + 2;
    let right = area.right().saturating_sub(2);
    if right <= x || top >= bottom || preview.lines.is_empty() {
        return;
    }
    // Header: the file's path, dimmed, on the pane's first row.
    emit(
        buf,
        x,
        top,
        preview.path,
        theme.status_inactive,
        0,
        (right - x) as usize,
    );

    let text_top = top + 1;
    let rows = bottom.saturating_sub(text_top) as usize;
    if rows == 0 {
        return;
    }
    // Centre the window on the hit line where the file allows it.
    let focus = preview.focus.min(preview.lines.len() - 1);
    let first = focus
        .saturating_sub(rows / 2)
        .min(preview.lines.len().saturating_sub(rows));

    // The line-number gutter is as wide as the largest number on screen.
    let last_line = (first + rows).min(preview.lines.len());
    let gutter = digit_width(last_line.max(1)) as u16;

    for (row, index) in (first..preview.lines.len()).take(rows).enumerate() {
        let y = text_top + row as u16;
        // Clear the row first: a shorter line must not leave its predecessor.
        for column in (x - 1)..right {
            paint_cell(buf, column, y, " ", theme.overlay);
        }
        let focused = index == focus;
        let number = format!("{:>w$}", index + 1, w = gutter as usize);
        emit(
            buf,
            x,
            y,
            &number,
            if focused {
                theme.label_focused
            } else {
                theme.status_inactive
            },
            0,
            (gutter + 1) as usize,
        );
        let text_x = x + gutter + 1;
        let limit = right.saturating_sub(text_x) as usize;
        let mut column = 0usize;
        for (text, hl) in &preview.lines[index] {
            if column >= limit {
                break;
            }
            let style = theme.highlight(*hl);
            // The hit line gets the cursor-line background so it can be found
            // at a glance without hiding its syntax colours.
            let style = if focused {
                style.patch(theme.cursor_line)
            } else {
                style
            };
            column += emit(buf, text_x, y, text, style, column, limit);
        }
    }
}

/// Decimal digits in `n`, for sizing the preview's line-number gutter.
fn digit_width(n: usize) -> usize {
    let mut width = 1;
    let mut value = n / 10;
    while value > 0 {
        width += 1;
        value /= 10;
    }
    width
}

/// Paint a solid bordered card and return the area inside it.
///
/// The card is opaque on purpose: text bleeding through an overlay is
/// unreadable.  Shared by the completion popup and the diagnostic float so the
/// two cannot drift apart.
fn draw_card(buf: &mut Buffer, area: Rect, theme: &Theme) {
    // `paint_cell` clears stale styles: an underline on the buffer underneath
    // (spellcheck, markdown decoration) must not bleed through the card.
    for row in area.top()..area.bottom() {
        for column in area.left()..area.right() {
            paint_cell(buf, column, row, " ", theme.overlay);
        }
    }
    let set = border::ROUNDED;
    for column in area.left()..area.right() {
        buf[(column, area.y)].set_symbol(set.horizontal_top);
        buf[(column, area.bottom() - 1)].set_symbol(set.horizontal_bottom);
    }
    for row in area.top()..area.bottom() {
        buf[(area.x, row)].set_symbol(set.vertical_left);
        buf[(area.right() - 1, row)].set_symbol(set.vertical_right);
    }
    buf[(area.x, area.y)].set_symbol(set.top_left);
    buf[(area.right() - 1, area.y)].set_symbol(set.top_right);
    buf[(area.x, area.bottom() - 1)].set_symbol(set.bottom_left);
    buf[(area.right() - 1, area.bottom() - 1)].set_symbol(set.bottom_right);
}

/// Draw the completion popup hanging off the caret.
///
/// It opens below the caret when there is room and above it otherwise, and is
/// clipped to the pane area so it never covers the status row or the cards on it.
fn draw_completion(
    buf: &mut Buffer,
    screen: Rect,
    anchor: Position,
    view: &CompletionView,
    theme: &Theme,
) {
    let completion = view.completion;
    let items: Vec<&crate::lsp::CompletionItem> = completion
        .visible
        .iter()
        .filter_map(|index| completion.all.get(*index))
        .collect();
    if items.is_empty() || screen.width < 8 || screen.height < 4 {
        return;
    }
    let widest = items
        .iter()
        .map(|item| {
            item.label.chars().count()
                + item.detail.as_deref().map_or(0, |d| d.chars().count() + 1)
                + item.kind_label().chars().count()
                + 3
        })
        .max()
        .unwrap_or(0);
    let width = (widest + 2)
        .clamp(12, screen.width as usize)
        .min(screen.width as usize) as u16;
    let rows = items.len().clamp(1, 10) as u16;
    let height = rows + 2;

    // Prefer below the caret; flip above when the bottom would run off screen.
    let below = anchor.y + 1;
    let y = if below + height <= screen.bottom() {
        below
    } else {
        anchor.y.saturating_sub(height).max(screen.y)
    };
    let x = anchor.x.min(screen.right().saturating_sub(width));
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    if area.width < 4 || area.height < 3 {
        return;
    }

    draw_card(buf, area, theme);

    // Scroll the list so the highlighted row is always drawn.
    let visible_rows = (area.height - 2) as usize;
    let first = completion
        .selected
        .saturating_sub(visible_rows.saturating_sub(1));
    for (row, item) in items.iter().enumerate().skip(first).take(visible_rows) {
        let y = area.y + 1 + (row - first) as u16;
        let selected = row == completion.selected;
        let base = if selected {
            theme
                .selection
                .patch(Style::default().add_modifier(Modifier::BOLD))
        } else {
            theme.text
        };
        if selected {
            for column in (area.x + 1)..area.right().saturating_sub(1) {
                buf[(column, y)].set_style(theme.selection);
            }
        }
        let limit = area.width.saturating_sub(2) as usize;
        emit(buf, area.x + 1, y, &item.label, base, 0, limit);
        let mut detail = String::new();
        let kind = item.kind_label();
        if !kind.is_empty() {
            detail.push(' ');
            detail.push_str(kind);
        }
        if let Some(extra) = &item.detail {
            detail.push(' ');
            detail.push_str(extra);
        }
        if !detail.is_empty() {
            let used = item.label.chars().count() + 1;
            if used + 2 < limit {
                emit(
                    buf,
                    area.x + 1,
                    y,
                    &detail,
                    theme.status_inactive,
                    used,
                    limit,
                );
            }
        }
    }
}

/// Draw the full message of the diagnostic under the caret on a card.
///
/// It opens above the caret's line when there is room, so it never covers the
/// text being read, and wraps long messages instead of clipping them.
fn draw_diagnostic_float(
    buf: &mut Buffer,
    screen: Rect,
    anchor: Position,
    float: &DiagnosticFloat,
    theme: &Theme,
) {
    if screen.width < 12 || screen.height < 5 {
        return;
    }
    let diagnostic = float.diagnostic;
    let headline = diagnostic.headline();
    let width = (headline.chars().count() + 4)
        .clamp(20, (screen.width as usize).saturating_sub(2))
        .min(screen.width as usize) as u16;
    let inner = width.saturating_sub(2) as usize;
    let lines = wrap_text(&headline, inner);
    let height = (lines.len() as u16 + 2).min(screen.height);
    if height < 3 {
        return;
    }

    // Above the caret's line when it fits, below it otherwise.
    let y = if anchor.y >= height {
        anchor.y - height
    } else {
        (anchor.y + 1).min(screen.bottom().saturating_sub(height))
    };
    let x = anchor.x.min(screen.right().saturating_sub(width));
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    if area.width < 4 || area.height < 3 {
        return;
    }

    draw_card(buf, area, theme);
    let style = severity_style(diagnostic.severity, theme);
    for (row, line) in lines.iter().enumerate() {
        let y = area.y + 1 + row as u16;
        if y >= area.bottom().saturating_sub(1) {
            break;
        }
        emit(buf, area.x + 1, y, line, style, 0, inner);
    }
}

/// Draw the language server's answer to `K` on a card.
///
/// Hover text is often several lines (a signature plus documentation), so the
/// card is sized to the wrapped text and capped to the pane.  It opens above the
/// caret when there is room, so it never covers the symbol being asked about.
fn draw_hover(buf: &mut Buffer, screen: Rect, anchor: Position, text: &str, theme: &Theme) {
    if screen.width < 12 || screen.height < 5 {
        return;
    }
    let widest = text
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let width = (widest + 4)
        .clamp(20, (screen.width as usize).saturating_sub(2))
        .min(screen.width as usize) as u16;
    let inner = width.saturating_sub(2) as usize;
    // Wrap each source line separately: a hover's own line breaks are
    // meaningful (signature, then documentation).
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrap_text(line, inner));
        }
    }
    if lines.is_empty() {
        return;
    }
    let height = (lines.len() as u16 + 2).min(screen.height);
    if height < 3 {
        return;
    }

    let y = if anchor.y >= height {
        anchor.y - height
    } else {
        (anchor.y + 1).min(screen.bottom().saturating_sub(height))
    };
    let x = anchor.x.min(screen.right().saturating_sub(width));
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    if area.width < 4 || area.height < 3 {
        return;
    }

    draw_card(buf, area, theme);
    for (row, line) in lines.iter().enumerate() {
        let y = area.y + 1 + row as u16;
        if y >= area.bottom().saturating_sub(1) {
            break;
        }
        emit(buf, area.x + 1, y, line, theme.text, 0, inner);
    }
}

/// Break `text` into lines of at most `width` characters, on word boundaries
/// where possible.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
        // A single word longer than the line is hard-split so it stays visible.
        while current.chars().count() > width {
            let split = current
                .char_indices()
                .nth(width)
                .map(|(index, _)| index)
                .unwrap_or(current.len());
            let rest = current.split_off(split);
            lines.push(std::mem::take(&mut current));
            current = rest;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Wrap a picker preview to `width`, keeping its own line breaks.
///
/// A preview is multi-line by design — the palette puts usage above help, and a
/// diagnostic puts its related locations under the headline — so each source
/// line is wrapped separately rather than reflowed into one paragraph.  At most
/// `max_rows` lines come back; a longer preview is cut and the last line is
/// marked so the cut is visible rather than looking like the end of the text.
fn wrap_preview(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    let width = width.max(1);
    let max_rows = max_rows.max(1);
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if lines.len() >= max_rows {
            break;
        }
        if line.trim().is_empty() {
            lines.push(String::new());
            continue;
        }
        for wrapped in wrap_text(line, width) {
            if lines.len() >= max_rows {
                break;
            }
            lines.push(wrapped);
        }
    }
    if lines.len() == max_rows && text.lines().count() > max_rows {
        if let Some(last) = lines.last_mut() {
            // Mark the cut so a truncated preview does not read as complete.
            let keep = width.saturating_sub(1).max(1);
            let truncated: String = last.chars().take(keep).collect();
            *last = format!("{truncated}…");
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer as TextBuffer, BufferId};
    use crate::config::Config;
    use crate::editor::Editor;
    use crate::layout::Pane;

    fn editor_with(text: &str) -> Editor {
        let mut editor = Editor::new(Config::default());
        let id = editor.new_scratch();
        if let Some(buffer) = editor.buffer_mut(id) {
            let mut selection = Selection::point(0);
            buffer.set_text(text, crate::buffer::ChangeGroup::Command, &mut selection);
        }
        editor.show_in_pane(id, None);
        editor
    }

    fn render_to(width: u16, height: u16, editor: &Editor) -> (Buffer, Option<Position>) {
        render_with(width, height, editor, &Config::default())
    }

    fn render_with(
        width: u16,
        height: u16,
        editor: &Editor,
        config: &Config,
    ) -> (Buffer, Option<Position>) {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        let snapshot = Snapshot::collect(editor, None, std::path::Path::new("/tmp"));
        let screen = Screen {
            editor,
            config,
            theme: &theme,
            status: snapshot,
            prompt: None,
            picker: None,
            completion: None,
            diagnostic_float: None,
            hover: None,
        };
        let (status_area, pane_area) = areas(area, config);
        buffer.set_style(area, theme.text);
        let cursor = draw_panes(&mut buffer, pane_area, &screen);
        status::render(
            status_area,
            &mut buffer,
            &config.status,
            &screen.status,
            &theme,
        );
        (buffer, cursor)
    }

    fn cell_symbol(buffer: &Buffer, x: u16, y: u16) -> String {
        buffer
            .cell((x, y))
            .map(|cell| cell.symbol().to_string())
            .unwrap_or_default()
    }

    /// Every rendered character, for assertions about whole frames.
    fn frame_text(buffer: &Buffer) -> String {
        let area = buffer.area();
        (0..area.height)
            .flat_map(|row| (0..area.width).map(move |column| (column, row)))
            .map(|(column, row)| cell_symbol(buffer, column, row))
            .collect()
    }

    fn row_text(buffer: &Buffer, row: u16) -> String {
        let width = buffer.area().width;
        (0..width).map(|x| cell_symbol(buffer, x, row)).collect()
    }

    /// An editor holding `text` as a markdown buffer.
    ///
    /// Decoration follows the buffer's language, and a path-less scratch buffer
    /// detects as plain text — so without this the tests below would pass while
    /// proving nothing.  The language is set the way opening `note.md` sets it.
    fn markdown_editor(text: &str) -> Editor {
        let mut editor = editor_with(text);
        let id = editor.current_buffer_id().expect("a buffer");
        let markdown = crate::lang::detect(std::path::Path::new("note.md").into(), None);
        editor.buffer_mut(id).expect("the buffer").language = markdown;
        editor
    }

    /// A markdown buffer drawn through the real pane path, with the caret parked
    /// on a spare line after the text so it does not reveal markup there — see
    /// [`crate::markdown::LineRender::reveal`].
    fn rendered_markdown(text: &str) -> Buffer {
        let mut editor = markdown_editor(text);
        editor.set_cursor(text.len());
        let (buffer, _) = render_to(60, 10, &editor);
        buffer
    }

    #[test]
    fn a_ragged_table_grows_into_one_aligned_grid() {
        // The complaint this answers: rows, cells and columns of a table written
        // without padding must still line up on screen.  Every row of the run
        // draws at the same width with its rules in the same cells, and the
        // caret — parked off the table so no row reveals itself — resolves
        // through the column map like everything else.
        let text = "| name | value |\n| --- | ----- |\n| a longer cell | 1 |\n\nDone.\n";
        let mut editor = markdown_editor(text);
        editor.set_cursor(text.find("Done.").expect("a closing line") + 1);
        let (buffer, _) = render_to(70, 12, &editor);

        // Screen rows 1..=3 below the pane border; the card's own edge pipes are
        // present too, which is harmless — they sit at the same column on every
        // row of any frame.
        let rules_on = |row: u16| -> Vec<usize> {
            row_text(&buffer, row)
                .chars()
                .enumerate()
                .filter(|(_, c)| matches!(c, '│' | '┼' | '├' | '┤'))
                .map(|(index, _)| index)
                .collect()
        };
        let header = rules_on(1);
        for row in 2..=3 {
            assert_eq!(rules_on(row), header, "row {row} broke the grid");
        }
        // Content starts where its column starts on every row, alignment included.
        let first_row = row_text(&buffer, 1);
        let last_row = row_text(&buffer, 3);
        assert_eq!(first_row.find("value"), last_row.find('1'));
    }

    #[test]
    fn a_click_on_a_grown_column_lands_on_the_character_drawn_there() {
        // The click path resolves through the same map the paint path does:
        // the header's `v` has travelled from buffer column 9 to its grown
        // grid cell, and clicking that cell must still name character 9.
        let text = "| name | value |\n| --- | ----- |\n| a longer cell | 1 |\n\nDone.\n";
        let mut editor = markdown_editor(text);
        let click_line = 0usize;
        editor.set_cursor(text.find("Done.").expect("a closing line") + 1);
        let config = Config::default();
        let theme = Theme::default();
        let area = Rect::new(0, 0, 70, 12);
        let (_, pane_area) = areas(area, &config);
        let buffer = editor.current_buffer().expect("a buffer");
        let mut pane = crate::layout::Pane::new(
            crate::layout::PaneId(1),
            editor.current_buffer_id().expect("a buffer"),
        );
        pane.selection = Selection::point(text.find("Done.").expect("a closing line") + 1);

        // Where does the map say that `v` ended up?  Ask, then click there.
        let decor = decorations(buffer, &config, &theme, 0, 12, None).expect("markdown");
        let render = decor.get(click_line).expect("a row");
        assert!(render.reflowed(), "the header did not reflow");
        let column = pane_content(pane_area, &config).x
            + Gutter::new(&config, buffer, 4, true).width as u16
            + render.column_of(9) as u16;
        let row = pane_content(pane_area, &config).y + click_line as u16;

        let byte = byte_at(buffer, pane_area, &pane, &config, &theme, column, row)
            .expect("the click is inside the pane");
        assert_eq!(
            byte,
            buffer.line_start(click_line) + 9,
            "the click missed the `v`"
        );
    }

    #[test]
    fn the_carets_own_line_reveals_its_markup() {
        // Whatever line you are editing shows its source; the rest stays clean.
        let mut editor = markdown_editor("# Heading\n\nsome **bold** text\n");
        editor.set_cursor(0);
        let (buffer, _) = render_to(60, 10, &editor);
        let frame = frame_text(&buffer);
        assert!(frame.contains("# Heading"), "{frame:?}");
        assert!(!frame.contains("some **bold**"), "revealed a quiet line");

        editor.set_cursor(editor.current_buffer().expect("a").len_chars() - 1);
        let (buffer, _) = render_to(60, 10, &editor);
        let frame = frame_text(&buffer);
        assert!(
            !frame.contains('#'),
            "the heading hid again under nothing:\n{frame:?}"
        );
    }

    #[test]
    fn markdown_is_decorated_in_the_buffer_itself() {
        // The markers a reader never wants to see are gone; the words they wrapped
        // stay exactly where they were written.
        let frame = rendered_markdown("# Heading\n\nsome **bold** text\n");
        assert!(
            frame_text(&frame).contains("Heading"),
            "{:?}",
            frame_text(&frame)
        );
        assert!(!frame_text(&frame).contains('#'), "hash leaked");
        assert!(!frame_text(&frame).contains("**"), "asterisks leaked");
    }

    #[test]
    fn a_bullet_is_drawn_in_the_editors_column() {
        let buffer = rendered_markdown("- one\n");
        // Gutter aside, the row reads as a bullet rather than a hyphen.
        assert!(
            frame_text(&buffer).contains('•'),
            "{:?}",
            frame_text(&buffer)
        );
    }

    #[test]
    fn decoration_keeps_the_cursor_over_its_character() {
        // The invariant that matters most while editing: with concealing glyph
        // substitution the caret must still land on the character under it, so a
        // decorated line reports the same column as an undecorated one.
        let text = "# Heading\n";
        let mut editor = markdown_editor(text);
        editor.set_cursor(2); // the `H`, two characters past the concealed `#`
        let (buffer, cursor) = render_to(60, 10, &editor);
        let decorated = cursor.expect("a caret in a decorated buffer");

        let mut config = Config::default();
        config.editor.markdown_render = false;
        let mut plain_editor = markdown_editor(text);
        plain_editor.set_cursor(2);
        let (_, plain) = render_with(60, 10, &plain_editor, &config);

        assert_eq!(Some(decorated), plain, "decoration moved the caret");
        // And the character drawn there is the title's first letter.
        assert_eq!(cell_symbol(&buffer, decorated.x, decorated.y), "H");
    }

    #[test]
    fn a_theme_gives_each_heading_level_its_own_colour() {
        // The fallback style bag can only vary weight; the palette is what separates
        // `#`, `##` and `###` on screen.
        let styles = markdown_styles(&Theme::default());
        assert_ne!(styles.heading1, styles.heading2);
        assert_ne!(styles.heading2, styles.heading3);
    }

    #[test]
    fn a_file_too_big_for_background_work_is_drawn_as_source() {
        // Decoration reads the whole document to classify a frame; past the threshold
        // that already excuses a buffer from parsing and formatting, it draws plain.
        let mut config = Config::default();
        config.editor.large_file_threshold = 8;
        let editor = markdown_editor("# Heading\n\nsome **bold** text\n");
        let (buffer, _) = render_with(60, 10, &editor, &config);
        assert!(
            frame_text(&buffer).contains("**bold**"),
            "decorated an oversized file"
        );
    }

    #[test]
    fn turning_markdown_rendering_off_shows_the_source_verbatim() {
        let mut config = Config::default();
        config.editor.markdown_render = false;
        let editor = markdown_editor("# Heading\n- one\n");
        let (buffer, _) = render_with(60, 10, &editor, &config);
        assert!(frame_text(&buffer).contains("# Heading"));
        assert!(!frame_text(&buffer).contains('•'));
    }

    #[test]
    fn one_status_row_and_the_panes_take_the_rest() {
        // There is no command row any more: transient text floats on a card, so
        // the panes get every row the status area does not.
        let config = Config::default();
        let area = Rect::new(0, 0, 80, 24);
        let (status_area, pane_area) = areas(area, &config);
        assert_eq!(status_area.height, 1);
        assert_eq!(status_area.y, 23, "the status row sits at the bottom");
        assert_eq!(pane_area.y, 0);
        assert_eq!(pane_area.height, 23);

        let mut top = Config::default();
        top.status.position = crate::config::StatusPosition::Top;
        let (status_area, pane_area) = areas(area, &top);
        assert_eq!(status_area.y, 0);
        assert_eq!(pane_area.y, status_area.height);
        assert_eq!(pane_area.height, 23);
    }

    #[test]
    fn renders_text_with_gutter_and_cursor_line() {
        let editor = editor_with("alpha\nbeta\ngamma\n");
        let (buffer, cursor) = render_to(40, 8, &editor);
        let first = row_text(&buffer, 1);
        assert!(first.contains("alpha"), "{first:?}");
        assert!(first.contains('1'), "gutter should show the line number");
        // The cursor starts at the beginning of the buffer.
        assert_eq!(
            cursor.map(|position| (position.x, position.y)),
            Some((3, 1))
        );
    }

    #[test]
    fn past_end_of_buffer_shows_tildes() {
        let editor = editor_with("one\n");
        let (buffer, _) = render_to(40, 6, &editor);
        let text: Vec<String> = (0..6).map(|row| row_text(&buffer, row)).collect();
        // Row 0 is the pane border, row 1 the only line, row 2 the `~`.
        assert!(text.iter().skip(2).any(|row| row.contains('~')), "{text:?}");
    }

    #[test]
    fn diagnostic_tags_change_the_shape_of_the_mark() {
        use ratatui::style::Modifier;

        let theme = Theme::default();
        let plain = Diagnostic::new(
            Span::point(0),
            Severity::Warning,
            "unused",
            crate::diagnostic::DiagnosticSource::Lsp,
        );
        let unnecessary = plain.clone().with_tags(vec![DiagnosticTag::Unnecessary]);
        let deprecated = plain.clone().with_tags(vec![DiagnosticTag::Deprecated]);

        assert!(
            !diagnostic_style(&plain, &theme)
                .add_modifier
                .contains(Modifier::DIM),
            "an untagged diagnostic is not dimmed"
        );
        assert!(
            diagnostic_style(&unnecessary, &theme)
                .add_modifier
                .contains(Modifier::DIM),
            "an unnecessary span fades out"
        );
        assert!(
            diagnostic_style(&deprecated, &theme)
                .add_modifier
                .contains(Modifier::CROSSED_OUT),
            "a deprecated span is struck through"
        );
        // The severity colour survives the tag, so a tagged error still reads
        // as an error.
        assert_eq!(
            diagnostic_style(&unnecessary, &theme).fg,
            diagnostic_style(&plain, &theme).fg
        );
    }

    #[test]
    fn the_float_wraps_long_messages_instead_of_clipping_them() {
        let lines = wrap_text("the quick brown fox jumps over the lazy dog", 12);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(line.chars().count() <= 12, "{line:?} is too wide");
        }
        // No words are lost in the wrap.
        assert_eq!(
            lines.join(" "),
            "the quick brown fox jumps over the lazy dog"
        );
    }

    #[test]
    fn a_word_longer_than_the_line_is_hard_split() {
        let lines = wrap_text("supercalifragilistic", 8);
        assert_eq!(lines.concat(), "supercalifragilistic");
        for line in &lines {
            assert!(line.chars().count() <= 8, "{line:?} is too wide");
        }
    }

    #[test]
    fn the_diagnostic_float_draws_the_full_message() {
        let mut editor = editor_with("let x = ;\n");
        let id = editor.current_buffer_id().expect("a buffer");
        let at = editor.buffer(id).expect("a buffer").line_start(0);
        let diagnostic = Diagnostic::new(
            Span::new(at + 8, at + 9),
            Severity::Error,
            "expected expression",
            crate::diagnostic::DiagnosticSource::Lsp,
        )
        .with_code(Some("E0308".into()));
        editor.set_diagnostics(
            id,
            crate::diagnostic::DiagnosticSource::Lsp,
            vec![diagnostic],
        );

        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let config = Config::default();
        let theme = Theme::default();
        let snapshot = Snapshot::collect(&editor, None, std::path::Path::new("/tmp"));
        let diagnostic = &editor.current_buffer().expect("a buffer").diagnostics[0];
        let screen = Screen {
            editor: &editor,
            config: &config,
            theme: &theme,
            status: snapshot,
            prompt: None,
            picker: None,
            completion: None,
            diagnostic_float: Some(DiagnosticFloat { diagnostic }),
            hover: None,
        };
        let (_, pane_area) = areas(area, &config);
        draw_panes(&mut buffer, pane_area, &screen);
        // The float hangs off the caret, which the panes just placed.
        let anchor = Position { x: 3, y: 1 };
        draw_diagnostic_float(
            &mut buffer,
            pane_area,
            anchor,
            &screen.diagnostic_float.unwrap(),
            &theme,
        );

        let rendered = frame_text(&buffer);
        assert!(rendered.contains("expected expression"), "{rendered}");
        assert!(rendered.contains("E0308"), "the code is in the headline");
    }

    #[test]
    fn inline_diagnostics_draw_the_message_after_the_line() {
        let mut editor = editor_with("let x = ;\nsecond line\n");
        let id = editor.current_buffer_id().expect("a buffer");
        let at = editor.buffer(id).expect("a buffer").line_start(0);
        editor.set_diagnostics(
            id,
            crate::diagnostic::DiagnosticSource::Lsp,
            vec![Diagnostic::new(
                Span::new(at + 8, at + 9),
                Severity::Error,
                "expected expression",
                crate::diagnostic::DiagnosticSource::Lsp,
            )],
        );

        // Off by default: the message costs horizontal space.
        let (buffer, _) = render_to(60, 8, &editor);
        assert!(
            !row_text(&buffer, 1).contains("expected expression"),
            "inline text is opt-in"
        );

        let mut config = Config::default();
        config.editor.inline_diagnostics = true;
        let (buffer, _) = render_with(60, 8, &editor, &config);
        let row = row_text(&buffer, 1);
        assert!(row.contains("expected expression"), "{row:?}");
        // The line's own text is still there, ahead of the message.
        assert!(row.contains("let x = ;"), "{row:?}");
    }

    #[test]
    fn the_worst_diagnostic_on_a_line_wins_the_inline_slot() {
        let mut editor = editor_with("let x = 1;\n");
        let id = editor.current_buffer_id().expect("a buffer");
        let at = editor.buffer(id).expect("a buffer").line_start(0);
        editor.set_diagnostics(
            id,
            crate::diagnostic::DiagnosticSource::Lsp,
            vec![
                Diagnostic::new(
                    Span::new(at, at + 3),
                    Severity::Hint,
                    "hint message",
                    crate::diagnostic::DiagnosticSource::Lsp,
                ),
                Diagnostic::new(
                    Span::new(at + 4, at + 5),
                    Severity::Error,
                    "error message",
                    crate::diagnostic::DiagnosticSource::Lsp,
                ),
            ],
        );

        let mut config = Config::default();
        config.editor.inline_diagnostics = true;
        let (buffer, _) = render_with(60, 8, &editor, &config);
        let row = row_text(&buffer, 1);
        assert!(row.contains("error message"), "{row:?}");
        assert!(!row.contains("hint message"), "{row:?}");
    }

    #[test]
    fn long_lines_wrap_into_several_rows() {
        let editor = editor_with(&"x".repeat(120));
        let mut config = Config::default();
        config.editor.wrap = true;
        let area = Rect::new(0, 0, 40, 8);
        let (_, pane_area) = areas(area, &config);
        let content_width = pane_area.width as usize - 2 - 3;
        let rows = visual_rows(
            editor.buffer(BufferId(1)).unwrap(),
            0,
            pane_area.height as usize - 2,
            content_width,
            4,
            true,
            None,
        );
        assert!(rows.len() >= 4, "120 characters should wrap");
        assert_eq!(rows[0].end, content_width);
        assert_eq!(rows[0].line, Some(0));
    }

    #[test]
    fn tabs_expand_to_the_tab_stop() {
        let mut editor = editor_with("\tone\n");
        editor.config.editor.tab_width = 4;
        let (buffer, _) = render_to(40, 6, &editor);
        let row = row_text(&buffer, 1);
        assert!(row.contains("    one"), "{row:?}");
    }

    #[test]
    fn selection_highlights_the_selected_characters() {
        let mut editor = editor_with("hello world\n");
        editor.mode = crate::input::Mode::Visual(VisualKind::Char);
        let mut selection = Selection::point(0);
        selection.set_primary_span(Span::new(0, 4), crate::selection::Dir::Forward);
        selection.extend_to(4);
        editor.set_selection(selection);
        let pane = editor.layout.focused_pane().unwrap();
        let view = SelectionView::new(&editor, &pane.selection.clone());
        assert!(!view.spans.is_empty());
        assert!(view.spans[0].contains(1));
        assert!(!view.spans[0].contains(9));
    }

    #[test]
    fn a_message_floats_on_a_card_and_leaves_the_status_row_alone() {
        // The old two-row area overwrote whatever the status line said with each
        // message and had to restore it afterwards.  A card cannot do that, which
        // is the point: the row keeps reporting the buffer while a message is up.
        let editor = editor_with("alpha\nbeta\n");
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let config = Config::default();
        let theme = Theme::default();
        let mut snapshot = Snapshot::collect(&editor, None, std::path::Path::new("/tmp"));
        snapshot.message = Some((StatusLevel::Info, "42 lines yanked".into()));
        let screen = Screen {
            editor: &editor,
            config: &config,
            theme: &theme,
            status: snapshot,
            prompt: None,
            picker: None,
            completion: None,
            diagnostic_float: None,
            hover: None,
        };

        let (status_area, pane_area) = areas(area, &config);
        draw_panes(&mut buffer, pane_area, &screen);
        let notice = screen.notice().expect("the message is the notice");
        assert_eq!(notice.tone, NoticeTone::Info);
        assert_eq!(notice.caret, None, "a message is read, not edited");
        // No caret: a message takes no input, so the terminal cursor stays on the
        // text it belongs to rather than jumping into the card.
        assert_eq!(
            draw_notice(
                &mut buffer,
                pane_area,
                &notice,
                NoticePosition::Bottom,
                false,
                &theme
            ),
            None
        );
        status::render(
            status_area,
            &mut buffer,
            &config.status,
            &screen.status,
            &theme,
        );

        let rendered = frame_text(&buffer);
        assert!(rendered.contains("42 lines yanked"), "{rendered}");
        // The status row still reports the buffer, untouched by the card above it.
        let status_row = row_text(&buffer, 11);
        assert!(status_row.contains("*scratch*"), "{status_row:?}");
    }

    #[test]
    fn a_prompt_card_carries_the_caret_and_beats_a_message() {
        // Typing into the popup is the newest thing on screen; a stale message
        // must not win the row over it.
        let editor = editor_with("alpha\n");
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let config = Config::default();
        let theme = Theme::default();
        let mut snapshot = Snapshot::collect(&editor, None, std::path::Path::new("/tmp"));
        snapshot.message = Some((StatusLevel::Warn, "older news".into()));
        let screen = Screen {
            editor: &editor,
            config: &config,
            theme: &theme,
            status: snapshot,
            prompt: Some(Notice {
                prompt: "/",
                text: "vortex",
                caret: Some(6),
                suffix: "",
                tone: NoticeTone::Prompt,
            }),
            picker: None,
            completion: None,
            diagnostic_float: None,
            hover: None,
        };

        let notice = screen.notice().expect("the prompt is the notice");
        assert_eq!(notice.text, "vortex", "the message waits its turn");
        let (_, pane_area) = areas(area, &config);
        let caret = draw_notice(
            &mut buffer,
            pane_area,
            &notice,
            NoticePosition::Bottom,
            false,
            &theme,
        )
        .expect("a caret");
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("/vortex"), "{rendered}");
        assert!(!rendered.contains("older news"), "{rendered}");
        // The cursor lands after the six characters typed, on the card's text
        // row — one clear row above sits between the card and the status row.
        assert_eq!(caret.y, pane_area.bottom() - 3);
    }

    /// The notice card as `(left, top, right, bottom)`, found by its corners.
    fn card_box(buffer: &Buffer) -> (u16, u16, u16, u16) {
        let area = buffer.area;
        let top = (0..area.height)
            .find(|&row| (0..area.width).any(|column| buffer[(column, row)].symbol() == "\u{256d}"))
            .expect("the card draws a top border");
        let left = (0..area.width)
            .find(|&column| buffer[(column, top)].symbol() == "\u{256d}")
            .expect("left corner");
        let right = (left..area.width)
            .find(|&column| buffer[(column, top)].symbol() == "\u{256e}")
            .expect("right corner");
        let bottom = (top..area.height)
            .find(|&row| buffer[(left, row)].symbol() == "\u{2570}")
            .expect("bottom border");
        (left, top, right, bottom)
    }

    /// A notice card over the default screen; returns the buffer and pane area.
    fn notice_over_message(place: NoticePosition, status_top: bool) -> (Buffer, Rect) {
        let editor = editor_with("alpha\nbeta\n");
        let area = Rect::new(0, 0, 60, 12);
        let mut config = Config::default();
        config.status.position = if status_top {
            crate::config::StatusPosition::Top
        } else {
            crate::config::StatusPosition::Bottom
        };
        let theme = Theme::default();
        let mut snapshot = Snapshot::collect(&editor, None, std::path::Path::new("/tmp"));
        snapshot.message = Some((StatusLevel::Info, "search highlight cleared".into()));
        let screen = Screen {
            editor: &editor,
            config: &config,
            theme: &theme,
            status: snapshot,
            prompt: None,
            picker: None,
            completion: None,
            diagnostic_float: None,
            hover: None,
        };
        let (_, pane_area) = areas(area, &config);
        let mut buffer = Buffer::empty(area);
        let notice = screen.notice().expect("the message is the notice");
        draw_notice(&mut buffer, pane_area, &notice, place, status_top, &theme);
        (buffer, pane_area)
    }

    #[test]
    fn the_notice_keeps_a_clear_row_from_the_status_bar() {
        // Two borders stacked read as one broken line: the card must leave a
        // full row between itself and the bar, on whichever edge the bar is.
        // On the opposite edge there is no bar to keep clear of, and the card
        // sits flush.
        for status_top in [false, true] {
            let (buffer, pane_area) = notice_over_message(NoticePosition::Bottom, status_top);
            let (_, _, _, bottom) = card_box(&buffer);
            let gap = u16::from(!status_top);
            assert_eq!(
                bottom,
                pane_area.bottom() - 1 - gap,
                "bottom-anchored card, status_top={status_top}"
            );

            let (buffer, pane_area) = notice_over_message(NoticePosition::Top, status_top);
            let (_, top, _, _) = card_box(&buffer);
            let gap = u16::from(status_top);
            assert_eq!(
                top,
                pane_area.y + gap,
                "top-anchored card, status_top={status_top}"
            );
        }
    }

    #[test]
    fn notice_positions_place_the_card_where_they_say() {
        let cases = [
            (NoticePosition::Top, true, None),
            (NoticePosition::TopLeft, true, Some(true)),
            (NoticePosition::TopRight, true, Some(false)),
            (NoticePosition::Bottom, false, None),
            (NoticePosition::BottomLeft, false, Some(true)),
            (NoticePosition::BottomRight, false, Some(false)),
        ];
        for (place, top_edge, left_edge) in cases {
            let (buffer, pane_area) = notice_over_message(place, false);
            let (left, top, right, bottom) = card_box(&buffer);
            if top_edge {
                assert_eq!(top, pane_area.y, "{place:?} hugs the top edge");
            } else {
                assert_eq!(
                    bottom,
                    pane_area.bottom() - 2,
                    "{place:?} hugs the bottom edge"
                );
            }
            match left_edge {
                Some(true) => assert_eq!(left, pane_area.x, "{place:?} hugs the left"),
                Some(false) => assert_eq!(right, pane_area.right() - 1, "{place:?} hugs the right"),
                None => {
                    let left_margin = left - pane_area.x;
                    let right_margin = pane_area.right() - 1 - right;
                    assert_eq!(left_margin, right_margin, "{place:?} is centred");
                }
            }
        }
        // Left and Right: vertically centred, hugging the named side.
        for (place, expect_left) in [(NoticePosition::Left, true), (NoticePosition::Right, false)] {
            let (buffer, pane_area) = notice_over_message(place, false);
            let (left, top, right, bottom) = card_box(&buffer);
            if expect_left {
                assert_eq!(left, pane_area.x);
            } else {
                assert_eq!(right, pane_area.right() - 1);
            }
            let above = top - pane_area.y;
            let below = pane_area.bottom() - 1 - bottom;
            assert!(
                below.abs_diff(above) <= 1,
                "{place:?} should sit vertically centred: {top}..{bottom} in {pane_area:?}"
            );
        }
    }

    /// Locate a drawn picker's card by its rounded corners.
    fn card_rows(buffer: &Buffer, area: Rect) -> (u16, u16) {
        let top = (0..area.height)
            .find(|&row| (0..area.width).any(|column| buffer[(column, row)].symbol() == "\u{256d}"))
            .expect("the picker draws a top border");
        let left = (0..area.width)
            .find(|&column| buffer[(column, top)].symbol() == "\u{256d}")
            .expect("left corner");
        let bottom = (top..area.height)
            .find(|&row| buffer[(left, row)].symbol() == "\u{2570}")
            .expect("bottom border");
        (top, bottom)
    }

    #[test]
    fn the_picker_is_centred_vertically() {
        // The card floats in the middle of the screen rather than hugging the
        // status row: a short list and a long one both open where the eye
        // already is, and the top edge does not creep upward as hits arrive.
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let items = vec![prompt::Item::new("src/main.rs")];
        let picker = Picker {
            prompt: "> ",
            title: "find",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0],
            items: &items,
            selected: 0,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());

        let (top, bottom) = card_rows(&buffer, area);
        let card = bottom - top + 1;
        assert_eq!(
            top as usize,
            (area.height as usize - card as usize) / 2,
            "the card should sit centred in the frame"
        );
        assert!(
            (bottom as usize) < area.height as usize - 1,
            "the card must not sit on the status row"
        );
    }

    #[test]
    fn buffer_underlining_does_not_bleed_through_the_picker() {
        // A pane paints underlines (spellcheck, markdown) onto its cells before
        // the picker opens; ratatui styles patch rather than replace, so without
        // a full clear every card cell inheriting one keeps the underline and
        // the popup looks like it is drawn over striped paper.
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        for row in 0..area.height {
            for column in 0..area.width {
                buffer[(column, row)]
                    .set_symbol("x")
                    .set_style(Style::default().add_modifier(Modifier::UNDERLINED));
            }
        }
        let items = vec![prompt::Item::new("src/main.rs")];
        let picker = Picker {
            prompt: "> ",
            title: "find",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0],
            items: &items,
            selected: 0,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());

        let (top, bottom) = card_rows(&buffer, area);
        for row in top..=bottom {
            for column in 0..area.width {
                let cell = &buffer[(column, row)];
                if cell.symbol() == "x" {
                    continue; // outside the card: the pane underneath may keep its styles
                }
                assert!(
                    !cell.style().add_modifier.contains(Modifier::UNDERLINED),
                    "card cell ({column},{row}) inherited an underline"
                );
            }
        }
    }

    #[test]
    fn picker_renders_title_and_candidates() {
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        let items = vec![
            prompt::Item::new("src/main.rs"),
            prompt::Item::new("src/lib.rs").detail("rust"),
        ];
        let picker = Picker {
            prompt: "> ",
            title: "find",
            input: "ma",
            caret: 2,
            suffix: "",
            visible: &[0, 1],
            items: &items,
            selected: 0,
            preview: None,
        };
        let cursor = draw_picker(&mut buffer, area, &picker, &theme).expect("a cursor");
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("find"), "{rendered}");
        assert!(rendered.contains("src/main.rs"), "{rendered}");
        assert!(rendered.contains("src/lib.rs"), "{rendered}");
        assert!(cursor.y < area.height);
    }

    #[test]
    fn the_picker_draws_the_selected_rows_preview() {
        // The palette puts usage above help and a diagnostic puts its related
        // locations under the headline; both were populated and never drawn.
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let items = vec![
            prompt::Item::new(":write").preview(":w [file]\nwrite the buffer"),
            prompt::Item::new(":quit").preview(":q\nleave the editor"),
        ];
        let picker = Picker {
            prompt: "> ",
            title: "palette",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0, 1],
            items: &items,
            selected: 0,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("write the buffer"), "{rendered}");
        assert!(
            !rendered.contains("leave the editor"),
            "only the selected row's preview is drawn:\n{rendered}"
        );
    }

    #[test]
    fn the_preview_follows_the_selection() {
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let items = vec![
            prompt::Item::new(":write").preview("write the buffer"),
            prompt::Item::new(":quit").preview("leave the editor"),
        ];
        let picker = Picker {
            prompt: "> ",
            title: "palette",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0, 1],
            items: &items,
            selected: 1,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("leave the editor"), "{rendered}");
        assert!(!rendered.contains("write the buffer"), "{rendered}");
    }

    #[test]
    fn a_picker_without_a_preview_keeps_its_old_height() {
        // The preview must not shrink the list for pickers that have none.
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let items: Vec<prompt::Item> = (0..10)
            .map(|index| prompt::Item::new(format!("src/file{index}.rs")))
            .collect();
        let visible: Vec<usize> = (0..10).collect();
        let picker = Picker {
            prompt: "> ",
            title: "find",
            input: "",
            caret: 0,
            suffix: "",
            visible: &visible,
            items: &items,
            selected: 0,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let rendered = frame_text(&buffer);
        // All ten rows fit in a 20-row screen with no preview.
        assert!(rendered.contains("src/file9.rs"), "{rendered}");
    }

    #[test]
    fn a_search_picker_draws_its_file_preview_on_the_right() {
        // The find/grep pickers show the selected hit's file beside the list.
        let area = Rect::new(0, 0, 120, 20);
        let mut buffer = Buffer::empty(area);
        let items = vec![
            prompt::Item::new("src/main.rs:2"),
            prompt::Item::new("src/lib.rs:1"),
        ];
        let lines: Vec<Vec<(String, syntax::Hl)>> = (0..40)
            .map(|index| vec![(format!("let hit{index} = true;"), syntax::Hl::None)])
            .collect();
        let preview = FilePreview {
            path: "src/main.rs",
            lines: &lines,
            focus: 20,
        };
        let picker = Picker {
            prompt: "> ",
            title: "grep",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0, 1],
            items: &items,
            selected: 0,
            preview: Some(preview),
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let rendered = frame_text(&buffer);
        // The list keeps the left column and the file the right one.
        assert!(rendered.contains("src/main.rs:2"), "{rendered}");
        assert!(
            rendered.contains("let hit20"),
            "window centred on the hit:\n{rendered}"
        );
        assert!(
            !rendered.contains("let hit0 "),
            "the top of the file is off-window:\n{rendered}"
        );
    }

    #[test]
    fn a_narrow_screen_drops_the_preview_column() {
        // Below 90 columns there is no room for two columns; the list wins.
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let items = vec![prompt::Item::new("src/main.rs:2")];
        let lines: Vec<Vec<(String, syntax::Hl)>> =
            vec![(vec![("let previewed = true;".to_string(), syntax::Hl::None)])];
        let preview = FilePreview {
            path: "src/main.rs",
            lines: &lines,
            focus: 0,
        };
        let picker = Picker {
            prompt: "> ",
            title: "grep",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0],
            items: &items,
            selected: 0,
            preview: Some(preview),
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("src/main.rs:2"), "{rendered}");
        assert!(!rendered.contains("let previewed"), "{rendered}");
    }

    #[test]
    fn a_preview_keeps_its_own_line_breaks() {
        // Usage and help are separate lines; reflowing them into one paragraph
        // would run the usage into the description.
        let lines = wrap_preview(":w [file]\nwrite the buffer", 40, 6);
        assert_eq!(lines, vec![":w [file]", "write the buffer"]);
    }

    #[test]
    fn a_long_preview_is_cut_and_marked() {
        let text = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = wrap_preview(&text, 20, 3);
        assert_eq!(lines.len(), 3, "capped at the row limit");
        assert!(
            lines.last().expect("a line").ends_with('…'),
            "the cut is marked: {lines:?}"
        );
    }

    #[test]
    fn a_preview_wraps_a_line_that_is_too_wide() {
        let lines = wrap_preview("the quick brown fox jumps over the lazy dog", 12, 6);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(line.chars().count() <= 12, "{line:?} is too wide");
        }
    }

    #[test]
    fn a_long_result_list_scrolls_to_the_selection() {
        // `:grep` streams in far more hits than fit on screen; the highlighted
        // row has to be a row that is actually drawn, otherwise the list looks
        // frozen once the selection passes the last visible line.
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let items: Vec<prompt::Item> = (0..500)
            .map(|index| prompt::Item::new(format!("src/file{index:03}.rs:{}", index + 1)))
            .collect();
        let visible: Vec<usize> = (0..500).collect();
        let picker = Picker {
            prompt: "> ",
            title: "grep",
            input: "vortex",
            caret: 6,
            suffix: "",
            visible: &visible,
            items: &items,
            selected: 499,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let rendered = frame_text(&buffer);
        assert!(
            rendered.contains("src/file499.rs:500"),
            "the selected row is off screen:\n{rendered}"
        );
        assert!(
            !rendered.contains("src/file000"),
            "the head of the list should scroll away:\n{rendered}"
        );
        assert!(
            rendered.contains("500 match"),
            "the count is wrong:\n{rendered}"
        );
    }

    #[test]
    fn the_picker_covers_what_is_behind_it() {
        // An overlay with text bleeding through it is unreadable, and ratatui
        // only rewrites the cells a widget actually writes — so the card itself
        // has to be painted.
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        for row in 0..area.height {
            for column in 0..area.width {
                buffer[(column, row)].set_symbol("x");
            }
        }
        let items = vec![prompt::Item::new("only/one.rs")];
        let picker = Picker {
            prompt: "> ",
            title: "find",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0],
            items: &items,
            selected: 0,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());

        let top = (0..area.height)
            .find(|&row| (0..area.width).any(|c| buffer[(c, row)].symbol() == "\u{256d}"))
            .expect("the picker draws a top border");
        let left = (0..area.width)
            .find(|&column| buffer[(column, top)].symbol() == "\u{256d}")
            .expect("left corner");
        let right = (0..area.width)
            .rev()
            .find(|&column| buffer[(column, top)].symbol() == "\u{256e}")
            .expect("right corner");
        let bottom = (top..area.height)
            .find(|&row| buffer[(left, row)].symbol() == "\u{2570}")
            .expect("bottom border");

        for row in top + 1..bottom {
            for column in left + 1..right {
                assert_ne!(
                    buffer[(column, row)].symbol(),
                    "x",
                    "cell ({column},{row}) inside the picker still shows the buffer"
                );
            }
        }
        assert_ne!(
            buffer[(left + 1, bottom - 1)].bg,
            ratatui::style::Color::Reset,
            "the overlay needs a solid background of its own"
        );
    }

    #[test]
    fn the_completion_popup_draws_next_to_the_caret() {
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        let items = vec![
            crate::lsp::CompletionItem {
                label: "print".into(),
                insert_text: "print".into(),
                snippet: false,
                kind: Some(3),
                detail: Some("fn(&str)".into()),
                documentation: None,
                replace: None,
            },
            crate::lsp::CompletionItem {
                label: "println".into(),
                insert_text: "println".into(),
                snippet: false,
                kind: Some(3),
                detail: None,
                documentation: None,
                replace: None,
            },
        ];
        let completion =
            crate::completion::Completion::new(crate::buffer::BufferId(1), 1, 0, items);
        let view = CompletionView {
            completion: &completion,
        };
        draw_completion(&mut buffer, area, Position { x: 4, y: 3 }, &view, &theme);
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("print"), "{rendered}");
        assert!(rendered.contains("println"), "{rendered}");
        assert!(rendered.contains("fn"), "the kind is shown: {rendered}");
    }

    #[test]
    fn the_completion_popup_flips_above_the_caret_at_the_bottom() {
        // A caret on the last row has no room below it; the popup must open
        // upwards rather than run off the screen.
        let area = Rect::new(0, 0, 40, 10);
        let mut buffer = Buffer::empty(area);
        let items: Vec<crate::lsp::CompletionItem> = (0..5)
            .map(|index| crate::lsp::CompletionItem {
                label: format!("candidate{index}"),
                insert_text: String::new(),
                snippet: false,
                kind: None,
                detail: None,
                documentation: None,
                replace: None,
            })
            .collect();
        let completion =
            crate::completion::Completion::new(crate::buffer::BufferId(1), 1, 0, items);
        let view = CompletionView {
            completion: &completion,
        };
        draw_completion(
            &mut buffer,
            area,
            Position { x: 2, y: 9 },
            &view,
            &Theme::default(),
        );
        let rendered = frame_text(&buffer);
        assert!(
            rendered.contains("candidate0"),
            "the popup is off screen:\n{rendered}"
        );
    }

    #[test]
    fn welcome_screen_appears_without_panes() {
        let editor = Editor::new(Config::default());
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let config = Config::default();
        let theme = Theme::default();
        let screen = Screen {
            editor: &editor,
            config: &config,
            theme: &theme,
            status: Snapshot::default(),
            prompt: None,
            picker: None,
            completion: None,
            diagnostic_float: None,
            hover: None,
        };
        draw_panes(&mut buffer, area, &screen);
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("vortex"), "{rendered}");
    }

    #[test]
    fn pane_border_shows_the_file_name_and_modified_flag() {
        let mut editor = editor_with("hello\n");
        let id = editor.current_buffer_id().unwrap();
        // Make the buffer dirty with a real edit.
        editor.show_in_pane(id, None);
        editor.apply(
            vec![crate::buffer::Change::insert(0, "x")],
            crate::buffer::ChangeGroup::Command,
        );
        let (buffer, _) = render_to(40, 6, &editor);
        let top = row_text(&buffer, 0);
        assert!(top.contains("+"), "modified flag missing: {top:?}");
    }

    #[test]
    fn pane_border_style_covers_every_side() {
        // A border that colours only its top row reads as damage: the focus
        // style has to reach the sides and the bottom too.
        let editor = editor_with("hello\n");
        let config = Config::default();
        let theme = Theme::default();
        let (buffer, _) = render_to(40, 8, &editor);
        let (_, pane_area) = areas(Rect::new(0, 0, 40, 8), &config);
        let expected = theme
            .border_focused
            .fg
            .expect("the focused border carries a colour");
        let left = (pane_area.x, pane_area.y + pane_area.height / 2);
        let right = (pane_area.right() - 1, pane_area.y + pane_area.height / 2);
        let bottom = (pane_area.x + pane_area.width / 2, pane_area.bottom() - 1);
        for (x, y) in [left, right, bottom] {
            assert_eq!(
                buffer[(x, y)].fg,
                expected,
                "border cell ({x},{y}) missed the focus style"
            );
        }
    }

    #[test]
    fn the_picker_leaves_a_row_between_itself_and_the_status_area() {
        // The card grows from the bottom of the frame; its border must not sit
        // flush on the status row, which reads as one continuous line.
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let items = vec![prompt::Item::new("src/main.rs")];
        let picker = Picker {
            prompt: "> ",
            title: "find",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0],
            items: &items,
            selected: 0,
            preview: None,
        };
        draw_picker(&mut buffer, area, &picker, &Theme::default());
        let top = (0..area.height)
            .find(|&row| (0..area.width).any(|c| buffer[(c, row)].symbol() == "\u{256d}"))
            .expect("the picker draws a top border");
        let left = (0..area.width)
            .find(|&column| buffer[(column, top)].symbol() == "\u{256d}")
            .expect("left corner");
        let bottom = (top..area.height)
            .find(|&row| buffer[(left, row)].symbol() == "\u{2570}")
            .expect("bottom border");
        // Two rows below the card: one of buffer, one for the status area.
        assert!(
            area.height - 1 - bottom >= 2,
            "card ends at row {bottom}, leaving no buffer above the status row"
        );
    }

    #[test]
    fn row_slots_cover_every_visible_row() {
        let buffer = TextBuffer::new(
            BufferId(1),
            None,
            "one\ntwo\nthree\n".into(),
            &Config::default().editor,
        );
        let rows = visual_rows(&buffer, 0, 5, 40, 4, false, None);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].line, Some(0));
        assert_eq!(rows[2].line, Some(2));
        assert_eq!(rows[3].line, None, "the empty line after the final newline");
        let _ = Pane::new(crate::layout::PaneId(0), BufferId(1));
    }
}
