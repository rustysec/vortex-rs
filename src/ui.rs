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
use crate::config::{BorderStyle, Config, LineNumber};
use crate::diagnostic::{Diagnostic, Severity};
use crate::editor::Editor;
use crate::prompt;
use crate::selection::{Selection, Span};
use crate::status::{self, Snapshot};
use crate::syntax::{self, Hl};
use crate::theme::Theme;

/// The command line drawn on the message row.
#[derive(Debug, Clone, Copy)]
pub struct CommandLine<'a> {
    pub prompt: &'a str,
    pub text: &'a str,
    /// Caret as a character index into `text`.
    pub caret: usize,
    /// Ghost text from tab completion, drawn dimmed after the caret.
    pub suffix: &'a str,
}

/// A fuzzy picker drawn over the panes.
pub struct Picker<'a> {
    pub title: &'a str,
    pub input: &'a str,
    pub caret: usize,
    pub suffix: &'a str,
    /// Indices into `items`, already filtered and ordered.
    pub visible: &'a [usize],
    pub items: &'a [prompt::Item],
    /// Index into `visible` of the highlighted row.
    pub selected: usize,
}

/// A completion popup drawn next to the caret.
pub struct CompletionView<'a> {
    /// The popup state: candidates, the filtered order and the selection.
    pub completion: &'a crate::completion::Completion,
}

/// Everything the renderer needs for one frame.
pub struct Screen<'a> {
    pub editor: &'a Editor,
    pub config: &'a Config,
    pub theme: &'a Theme,
    pub status: Snapshot,
    pub command_line: Option<CommandLine<'a>>,
    pub picker: Option<Picker<'a>>,
    pub completion: Option<CompletionView<'a>>,
}

/// Split the terminal into status area, pane area and the message line.
///
/// The last row of the screen is always the message line, wherever `[status]
/// position` puts the status area.
pub fn areas(area: Rect, config: &Config) -> (Rect, Rect, Rect) {
    let status_height = config
        .status
        .height
        .clamp(1, 6)
        .min(area.height.saturating_sub(1));
    let message = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(1),
        width: area.width,
        height: 1.min(area.height),
    };
    let panes_height = area.height.saturating_sub(1 + status_height);
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
                height: panes_height,
            },
            message,
        )
    } else {
        (
            Rect {
                x: area.x,
                y: area.y + panes_height,
                width: area.width,
                height: status_height,
            },
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: panes_height,
            },
            message,
        )
    }
}

/// Draw one frame; the return value is where the terminal cursor should sit.
pub fn draw(frame: &mut Frame, screen: &Screen) -> Option<Position> {
    let area = frame.area();
    let (status_area, pane_area, message_area) = areas(area, screen.config);
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
    let command = screen
        .command_line
        .as_ref()
        .map(|line| (line.prompt, line.text, line.caret));
    status::render_message(message_area, buf, &screen.status, command, screen.theme);

    if let Some(line) = screen.command_line {
        cursor = Some(command_line_cursor(message_area, &line));
    }
    if let Some(picker) = &screen.picker {
        if let Some(position) = draw_picker(buf, area, picker, screen.theme) {
            cursor = Some(position);
        }
    }
    if let Some(completion) = &screen.completion {
        // The popup hangs off the caret, so it needs the position the panes
        // just computed; without one (no focused pane) there is nowhere to put
        // it.  It is clipped to the pane area so it never covers the status or
        // message rows.
        if let Some(anchor) = cursor {
            draw_completion(buf, pane_area, anchor, completion, screen.theme);
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
        ":edit <file>    open a file",
        "ctrl-p          find a file in the project",
        ":grep <text>    search the project",
        ":help           list commands",
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

    let tab_width = config.editor.tab_width.max(1) as usize;
    let line_count = buffer.display_line_count();
    let scroll = pane.view.scroll_line.min(line_count.saturating_sub(1));
    let cursor = pane.cursor();
    let cursor_line = buffer.line_of(cursor);
    let cursor_char = buffer.char_in_line(cursor);

    let gutter = Gutter::new(config, line_count, cursor_line, focused);
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

    let rows = visual_rows(
        buffer,
        scroll,
        content.height as usize,
        text_width,
        tab_width,
        config.editor.wrap,
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
        let styles = line_styles(
            buffer,
            line,
            &text,
            &highlights,
            &selection,
            screen,
            focused,
        );
        let x = content.x + gutter.width as u16;
        let limit = content.right().saturating_sub(x) as usize;
        let paint = LinePaint {
            x,
            y,
            limit,
            tab_width,
        };
        render_chunk(buf, &paint, &text, slot.start, slot.end, &styles);

        if focused && line == cursor_line && cursor_in_row(buffer, &text, cursor_char, slot) {
            let column = display_column(&text, slot.start, slot.end, cursor_char, tab_width);
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

/// A visual row: a buffer line plus the character range shown on that row.
#[derive(Debug, Clone, Copy)]
struct RowSlot {
    line: Option<usize>,
    start: usize,
    end: usize,
}

/// Map buffer lines to visual rows, wrapping when `[editor] wrap` is on.
fn visual_rows(
    buffer: &TextBuffer,
    scroll: usize,
    height: usize,
    width: usize,
    tab_width: usize,
    wrap: bool,
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
        if !wrap {
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

/// One style per character of a logical line.
fn line_styles(
    buffer: &TextBuffer,
    line: usize,
    text: &str,
    highlights: &[(Span, Style)],
    selection: &SelectionView,
    screen: &Screen,
    focused: bool,
) -> Vec<Style> {
    let theme = screen.theme;
    let base = theme.text;
    let line_start = buffer.line_start(line);
    let characters: Vec<char> = text.chars().collect();
    let mut styles = vec![base; characters.len()];
    if characters.is_empty() {
        return styles;
    }

    // Byte offset of every character, computed in one pass.
    let mut offsets = Vec::with_capacity(characters.len());
    let mut byte = 0usize;
    for character in &characters {
        offsets.push(line_start + byte);
        byte += character.len_utf8();
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
        let style = diagnostic_style(diagnostic.severity, theme);
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

/// Emit one visual row, expanding tabs and coalescing equal styles.
/// Where a line of text is drawn, and how far it may run.
struct LinePaint {
    x: u16,
    y: u16,
    /// Columns available before the edge of the pane.
    limit: usize,
    /// Spaces a tab advances to reach the next stop.
    tab_width: usize,
}

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

// ------------------------------------------------------------------- gutter

struct Gutter {
    width: usize,
    kind: LineNumber,
    cursor_line: usize,
}

impl Gutter {
    /// A gutter with `width == 0` means "no line numbers at all".
    fn new(config: &Config, total_lines: usize, cursor_line: usize, focused: bool) -> Self {
        let kind = match config.editor.line_number {
            LineNumber::None => LineNumber::None,
            // Relative numbers only make sense in the focused pane; others show
            // the absolute line so they stay readable.
            LineNumber::Relative if !focused => LineNumber::Absolute,
            other => other,
        };
        let width = if kind == LineNumber::None {
            0
        } else {
            digits(total_lines) + 1
        };
        Self {
            width,
            kind,
            cursor_line,
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
        if self.kind == LineNumber::None {
            return;
        }
        let Some(line) = line else {
            return;
        };
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
            self.width,
            style,
        );
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

fn diagnostic_style(severity: Severity, theme: &Theme) -> Style {
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
        buf.set_style(
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
            border_style,
        );
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
    view: &crate::layout::View,
    selection: &Selection,
    config: &Config,
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
    let tab_width = config.editor.tab_width.max(1) as usize;
    let line_count = buffer.display_line_count();
    let scroll = view.scroll_line.min(line_count.saturating_sub(1));
    let cursor_line = buffer.line_of(selection.cursor());
    let gutter = Gutter::new(config, line_count, cursor_line, true);
    let text_width = content.width.saturating_sub(gutter.width as u16).max(1) as usize;
    let line_row = usize::from(row.checked_sub(content.y)?);

    let rows = visual_rows(
        buffer,
        scroll,
        content.height as usize,
        text_width,
        tab_width,
        config.editor.wrap,
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
    let byte = buffer.line_start(line)
        + characters[slot.start.min(characters.len())..index]
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

fn command_line_cursor(area: Rect, line: &CommandLine) -> Position {
    let prompt_width = line.prompt.chars().count() as u16;
    Position {
        x: area.x + (prompt_width + line.caret as u16).min(area.width.saturating_sub(1)),
        y: area.y,
    }
}

/// Draw a fuzzy picker as a bordered box over the panes.
fn draw_picker(buf: &mut Buffer, screen: Rect, picker: &Picker, theme: &Theme) -> Option<Position> {
    if screen.width < 12 || screen.height < 5 {
        return None;
    }
    let width = (screen.width as usize * 3 / 4)
        .clamp(30.min(screen.width as usize), screen.width as usize)
        .min(screen.width as usize);
    // Leave room for the status and message rows at the bottom.
    let list_rows = picker
        .visible
        .len()
        .min((screen.height as usize).saturating_sub(8))
        .max(1);
    let height = (list_rows + 4).min(screen.height as usize - 2);
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width as u16) / 2,
        y: screen.y + screen.height.saturating_sub(height as u16 + 2),
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
    // the lines of the list.
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_symbol(" ").set_style(theme.overlay);
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

    // Input row.
    let input_y = area.y + 1;
    buf.set_stringn(area.x + 2, input_y, "> ", 2, theme.prompt);
    let text_x = area.x + 4;
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

    // Candidates.
    let list_top = area.y + 2;
    let list_bottom = area.bottom().saturating_sub(1);
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
        for column in (area.x + 2)..area.right().saturating_sub(1) {
            buf[(column, y)].set_style(if selected { theme.selection } else { style });
        }
        let label_limit = area.right().saturating_sub(area.x + 3) as usize;
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

/// Draw the completion popup hanging off the caret.
///
/// It opens below the caret when there is room and above it otherwise, and is
/// clipped to the pane area so it never covers the status or message rows.
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

    // A solid card, like the picker: text bleeding through is unreadable.
    for row in area.top()..area.bottom() {
        for column in area.left()..area.right() {
            buf[(column, row)].set_symbol(" ").set_style(theme.overlay);
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

#[allow(dead_code)]
fn unused_hl(hl: Hl, diagnostic: &Diagnostic) {
    let _ = (hl, diagnostic);
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
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let config = Config::default();
        let theme = Theme::default();
        let snapshot = Snapshot::collect(editor, None, std::path::Path::new("/tmp"));
        let screen = Screen {
            editor,
            config: &config,
            theme: &theme,
            status: snapshot,
            command_line: None,
            picker: None,
            completion: None,
        };
        let (status_area, pane_area, message_area) = areas(area, &config);
        buffer.set_style(area, theme.text);
        let cursor = draw_panes(&mut buffer, pane_area, &screen);
        status::render(
            status_area,
            &mut buffer,
            &config.status,
            &screen.status,
            &theme,
        );
        status::render_message(message_area, &mut buffer, &screen.status, None, &theme);
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

    #[test]
    fn areas_reserve_the_message_row_and_status_rows() {
        let config = Config::default();
        let area = Rect::new(0, 0, 80, 24);
        let (status_area, pane_area, message) = areas(area, &config);
        assert_eq!(status_area.height, config.status.height);
        assert_eq!(message.y, 23);
        assert_eq!(pane_area.height, 24 - 1 - status_area.height);
        assert_eq!(pane_area.y, 0, "status sits above the message line");

        let mut top = Config::default();
        top.status.position = crate::config::StatusPosition::Top;
        let (status_area, pane_area, message) = areas(area, &top);
        assert_eq!(status_area.y, 0);
        assert_eq!(pane_area.y, status_area.height);
        assert_eq!(message.y, 23);
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
    fn long_lines_wrap_into_several_rows() {
        let editor = editor_with(&"x".repeat(120));
        let mut config = Config::default();
        config.editor.wrap = true;
        let area = Rect::new(0, 0, 40, 8);
        let (_, pane_area, _) = areas(area, &config);
        let content_width = pane_area.width as usize - 2 - 3;
        let rows = visual_rows(
            editor.buffer(BufferId(1)).unwrap(),
            0,
            pane_area.height as usize - 2,
            content_width,
            4,
            true,
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
    fn picker_renders_title_and_candidates() {
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        let items = vec![
            prompt::Item::new("src/main.rs"),
            prompt::Item::new("src/lib.rs").detail("rust"),
        ];
        let picker = Picker {
            title: "find",
            input: "ma",
            caret: 2,
            suffix: "",
            visible: &[0, 1],
            items: &items,
            selected: 0,
        };
        let cursor = draw_picker(&mut buffer, area, &picker, &theme).expect("a cursor");
        let rendered = frame_text(&buffer);
        assert!(rendered.contains("find"), "{rendered}");
        assert!(rendered.contains("src/main.rs"), "{rendered}");
        assert!(rendered.contains("src/lib.rs"), "{rendered}");
        assert!(cursor.y < area.height);
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
            title: "grep",
            input: "vortex",
            caret: 6,
            suffix: "",
            visible: &visible,
            items: &items,
            selected: 499,
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
            title: "find",
            input: "",
            caret: 0,
            suffix: "",
            visible: &[0],
            items: &items,
            selected: 0,
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
            command_line: None,
            picker: None,
            completion: None,
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
    fn row_slots_cover_every_visible_row() {
        let buffer = TextBuffer::new(
            BufferId(1),
            None,
            "one\ntwo\nthree\n".into(),
            &Config::default().editor,
        );
        let rows = visual_rows(&buffer, 0, 5, 40, 4, false);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].line, Some(0));
        assert_eq!(rows[2].line, Some(2));
        assert_eq!(rows[3].line, None, "the empty line after the final newline");
        let _ = Pane::new(crate::layout::PaneId(0), BufferId(1));
    }
}
