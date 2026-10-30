//! The configurable status area.
//!
//! `[status]` decides which sections appear.  Sections are rendered left to right
//! and right to left; when the terminal is too narrow the least important sections
//! drop out from the inside, so the mode and file name always survive.
//!
//! The area is `status.height` rows tall, and the very last row of the terminal is
//! always the **message line** — the place where the mode indicator, key echoes,
//! messages and the command line are drawn.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::config::StatusConfig;
use crate::diagnostic::DiagnosticCount;
use crate::editor::{Editor, StatusLevel};
use crate::theme::Theme;

/// Everything the status area can show, gathered once per frame.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub mode: String,
    pub file: String,
    pub flags: Vec<&'static str>,
    pub language: String,
    pub position: Option<(usize, usize)>,
    pub percentage: Option<usize>,
    pub pane: Option<(usize, usize)>,
    pub split_mode: Option<&'static str>,
    pub diagnostics: DiagnosticCount,
    pub search: Option<String>,
    pub search_hits: usize,
    pub echo: String,
    pub message: Option<(StatusLevel, String)>,
    pub jobs: usize,
    pub cwd: Option<String>,
    pub read_only: bool,
    pub insert: bool,
}

impl Snapshot {
    /// Read the status picture out of the editor.
    pub fn collect(editor: &Editor, message: Option<(StatusLevel, String)>, cwd: &Path) -> Self {
        let mut snapshot = Snapshot {
            mode: editor.mode_label(),
            message,
            echo: editor.echo(),
            jobs: editor.pending_jobs.len(),
            cwd: Some(cwd.display().to_string()),
            search: editor.search_pattern().map(str::to_string),
            search_hits: editor.search_matches.len(),
            ..Snapshot::default()
        };
        let Some(buffer) = editor.current_buffer() else {
            return snapshot;
        };
        snapshot.file = buffer.display_name();
        snapshot.language = buffer.language.name.to_string();
        snapshot.read_only = buffer.read_only;
        snapshot.insert = editor.mode == crate::input::Mode::Insert;
        if buffer.is_dirty() {
            snapshot.flags.push("modified");
        }
        if buffer.read_only {
            snapshot.flags.push("read-only");
        }
        if !buffer.language.has_tree_sitter() {
            snapshot.flags.push("no-parser");
        }
        let cursor = editor.cursor();
        let line = buffer.line_of(cursor);
        let column = buffer.display_col(cursor);
        snapshot.position = Some((line + 1, column + 1));
        let total = buffer.display_line_count();
        snapshot.percentage = Some(if total <= 1 {
            100
        } else {
            ((line + 1) * 100) / total
        });
        snapshot.diagnostics = buffer.diagnostic_count();
        let panes = editor.layout.panes();
        if let Some(focused) = editor.layout.focused() {
            let index = panes.iter().position(|pane| *pane == focused).unwrap_or(0);
            snapshot.pane = Some((index + 1, panes.len()));
        }
        snapshot.split_mode = Some(crate::editor::split_mode_label(
            editor.config.layout.split_mode,
        ));
        snapshot
    }
}

/// Draw the status area (`area` covers `status.height` rows).
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    config: &StatusConfig,
    snapshot: &Snapshot,
    theme: &Theme,
) {
    for row in area.rows() {
        buf.set_style(row, theme.status);
    }
    // Row 0 carries the sections; a taller status area stays one blank block.
    let Some(row) = area.rows().next() else {
        return;
    };
    let left = left_sections(config, snapshot, theme);
    let right = right_sections(config, snapshot, theme);
    let left_end = write_group(row, buf, &left, 0, area.width);

    if right.is_empty() {
        return;
    }
    // Right-aligned, but never on top of the left hand sections.
    let right_width = measure(right.iter()) as u16;
    let start = area.width.saturating_sub(right_width).max(left_end);
    write_group(row, buf, &right, start, area.width);
}

fn left_sections<'a>(
    config: &StatusConfig,
    snapshot: &'a Snapshot,
    theme: &Theme,
) -> Vec<Section<'a>> {
    let mut sections = Vec::new();
    if config.show_mode {
        sections.push(Section {
            text: mode_label(snapshot).to_string().into(),
            style: mode_style(snapshot, theme),
            priority: 0,
        });
    }
    if config.show_pane_name {
        let mut label = snapshot.file.clone();
        if !label.is_empty() {
            if snapshot.flags.contains(&"modified") {
                label.push('+');
            }
            sections.push(Section {
                text: label.into(),
                style: theme.label,
                priority: 1,
            });
        }
    }
    if let Some((index, total)) = snapshot.pane {
        if total > 1 {
            sections.push(Section {
                text: format!("pane {index}/{total}").into(),
                style: theme.status_inactive,
                priority: 4,
            });
        }
    }
    sections
}

fn right_sections<'a>(
    config: &StatusConfig,
    snapshot: &'a Snapshot,
    theme: &Theme,
) -> Vec<Section<'a>> {
    let mut sections = Vec::new();
    // Right group is built from the right edge inwards.
    if let Some(split_mode) = snapshot.split_mode {
        sections.push(Section {
            text: split_mode.to_string().into(),
            style: theme.status_inactive,
            priority: 6,
        });
    }
    if config.show_percentage {
        if let Some(percentage) = snapshot.percentage {
            sections.push(Section {
                text: format!("{percentage}%").into(),
                style: theme.label,
                priority: 2,
            });
        }
    }
    if config.show_position {
        if let Some((line, column)) = snapshot.position {
            sections.push(Section {
                text: format!("{line}:{column}").into(),
                style: theme.label,
                priority: 2,
            });
        }
    }
    if config.show_language && !snapshot.language.is_empty() {
        sections.push(Section {
            text: snapshot.language.clone().into(),
            style: theme.status_inactive,
            priority: 4,
        });
    }
    if config.show_diagnostics && !snapshot.diagnostics.is_empty() {
        sections.push(Section {
            text: snapshot.diagnostics.summary().into(),
            style: theme.status_inactive,
            priority: 3,
        });
    }
    if let Some(term) = &snapshot.search {
        sections.push(Section {
            text: format!("/{term} ({} match)", snapshot.search_hits).into(),
            style: theme.search_match,
            priority: 5,
        });
    }
    if snapshot.jobs > 0 {
        sections.push(Section {
            text: format!("{} job(s)", snapshot.jobs).into(),
            style: theme.status_inactive,
            priority: 7,
        });
    }
    sections
}

fn mode_label(snapshot: &Snapshot) -> &'static str {
    match snapshot.mode.as_str() {
        "insert" => "INSERT",
        "visual" => "VISUAL",
        "visual-line" => "V-LINE",
        "visual-block" => "V-BLOCK",
        "command" => "COMMAND",
        _ => "NORMAL",
    }
}

fn mode_style(snapshot: &Snapshot, theme: &Theme) -> Style {
    match snapshot.mode.as_str() {
        "insert" => theme.mode_insert,
        "visual" | "visual-line" | "visual-block" => theme.mode_visual,
        "command" => theme.mode_command,
        _ => theme.mode_normal,
    }
}

struct Section<'a> {
    text: std::borrow::Cow<'a, str>,
    style: Style,
    /// Lower survives truncation first.
    priority: u8,
}

fn measure<'a>(sections: impl Iterator<Item = &'a Section<'a>>) -> usize {
    sections
        .map(|section| section.text.chars().count() + 1)
        .sum()
}

/// Write sections into a row, dropping the least important ones that do not fit.
/// Write sections from `start` up to `limit`, dropping the least important ones
/// that do not fit.  Returns the column after the last section written.
fn write_group(
    row: Rect,
    buf: &mut Buffer,
    sections: &[Section<'_>],
    start: u16,
    limit: u16,
) -> u16 {
    let available = limit.saturating_sub(start) as usize;
    let mut keep: Vec<bool> = sections.iter().map(|_| true).collect();
    let mut width = measure(
        sections
            .iter()
            .enumerate()
            .filter(|(index, _)| keep[*index])
            .map(|(_, section)| section),
    );
    // Drop the highest-priority-number (least important) sections until it fits.
    while width > available {
        let Some((index, _)) = sections
            .iter()
            .enumerate()
            .filter(|(index, _)| keep[*index])
            .max_by_key(|(_, section)| section.priority)
        else {
            break;
        };
        keep[index] = false;
        width = measure(
            sections
                .iter()
                .enumerate()
                .filter(|(index, _)| keep[*index])
                .map(|(_, section)| section),
        );
    }

    let mut x = start;
    for (index, section) in sections.iter().enumerate() {
        if !keep[index] {
            continue;
        }
        let remaining = limit.saturating_sub(x) as usize;
        if remaining < 2 {
            break;
        }
        let text = truncate(&section.text, remaining.saturating_sub(1));
        let span_width = text.chars().count() as u16;
        buf.set_stringn(
            x,
            row.y,
            format!("{text} "),
            (span_width + 1) as usize,
            section.style,
        );
        x += span_width + 1;
    }
    x
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut shortened: String = text.chars().take(width - 1).collect();
    shortened.push('…');
    shortened
}

/// Draw the message line: mode echo, pending keys, messages or the command line.
pub fn render_message(
    area: Rect,
    buf: &mut Buffer,
    snapshot: &Snapshot,
    command_line: Option<(&str, &str, usize)>,
    theme: &Theme,
) {
    for row in area.rows() {
        // Blank the row as well as restyling it, or a shorter message leaves the
        // previous one's characters behind.
        for x in row.left()..row.right() {
            buf[(x, row.y)].set_symbol(" ").set_style(theme.status);
        }
    }
    let Some(y) = area.rows().next().map(|row| row.y) else {
        return;
    };
    // The command line wins over everything else, vim-style.
    if let Some((prompt, text, caret)) = command_line {
        let mut spans = vec![Span::styled(prompt.to_string(), theme.prompt)];
        spans.push(Span::styled(text.to_string(), theme.text));
        // `caret` is a character index; the cursor is placed by the caller.
        let _ = caret;
        let _ = buf.set_line(0, y, &Line::from(spans), area.width);
        return;
    }
    if !snapshot.echo.is_empty() {
        let _ = buf.set_stringn(
            0,
            y,
            snapshot.echo.clone(),
            area.width as usize,
            theme.prompt,
        );
        return;
    }
    if let Some((level, message)) = &snapshot.message {
        let style = match level {
            StatusLevel::Info => theme.status,
            StatusLevel::Warn => theme.diagnostic_warning,
            StatusLevel::Error => theme.diagnostic_error,
        };
        let prefix = match level {
            StatusLevel::Info => "",
            StatusLevel::Warn => "warning: ",
            StatusLevel::Error => "error: ",
        };
        let _ = buf.set_stringn(
            0,
            y,
            format!("{prefix}{message}"),
            area.width as usize,
            style,
        );
        return;
    }
    let hint = if snapshot.insert {
        "esc to normal".to_string()
    } else {
        ":  commands   /  search   ctrl-p  palette".to_string()
    };
    let _ = buf.set_stringn(0, y, hint, area.width as usize, theme.status_inactive);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            mode: "normal".into(),
            file: "main.rs".into(),
            flags: vec!["modified"],
            language: "rust".into(),
            position: Some((12, 4)),
            percentage: Some(30),
            pane: Some((1, 2)),
            split_mode: Some("equal"),
            diagnostics: DiagnosticCount::default(),
            search: None,
            search_hits: 0,
            echo: String::new(),
            message: None,
            jobs: 0,
            cwd: None,
            read_only: false,
            insert: false,
        }
    }

    #[test]
    fn renders_sections_into_the_status_row() {
        let area = Rect::new(0, 0, 60, 1);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        render(
            area,
            &mut buffer,
            &StatusConfig::default(),
            &snapshot(),
            &theme,
        );
        let text = buffer.content();
        let rendered: String = text.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("NORMAL"), "{rendered}");
        assert!(rendered.contains("main.rs+"), "{rendered}");
        assert!(rendered.contains("12:4"), "{rendered}");
        assert!(rendered.contains("30%"), "{rendered}");
    }

    #[test]
    fn narrow_widths_drop_the_least_important_sections() {
        let area = Rect::new(0, 0, 22, 1);
        let mut buffer = Buffer::empty(area);
        render(
            area,
            &mut buffer,
            &StatusConfig::default(),
            &snapshot(),
            &Theme::default(),
        );
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("NORMAL"), "{rendered}");
        assert!(rendered.contains("main.rs+"), "{rendered}");
        assert!(!rendered.contains("rust"), "{rendered}");
    }

    #[test]
    fn disabled_sections_disappear() {
        let config = StatusConfig {
            show_position: false,
            show_percentage: false,
            show_language: false,
            ..StatusConfig::default()
        };
        let area = Rect::new(0, 0, 60, 1);
        let mut buffer = Buffer::empty(area);
        render(area, &mut buffer, &config, &snapshot(), &Theme::default());
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(!rendered.contains("12:4"), "{rendered}");
        assert!(!rendered.contains("30%"), "{rendered}");
    }

    #[test]
    fn message_line_prefers_the_command_line_then_messages() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        let mut snapshot = snapshot();
        snapshot.message = Some((StatusLevel::Error, "nope".into()));
        render_message(
            area,
            &mut buffer,
            &snapshot,
            Some(("/", "vortex", 6)),
            &Theme::default(),
        );
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("/vortex"), "{rendered}");
        assert!(!rendered.contains("nope"), "{rendered}");

        let mut buffer = Buffer::empty(area);
        render_message(area, &mut buffer, &snapshot, None, &Theme::default());
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("error: nope"), "{rendered}");
    }
}
