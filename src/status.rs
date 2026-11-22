//! The configurable status row.
//!
//! `[status]` decides which sections appear.  Sections are rendered left to right
//! and right to left; when the terminal is too narrow the least important sections
//! drop out from the inside, so the mode and file name always survive.
//!
//! With `separators = "angled"` (the default) each section is a filled chip
//! and neighbours are joined by an angled powerline separator — `` on the
//! left hand group, `` on the right.  Chip backgrounds cycle through the two
//! status fills so no two neighbours match and every separator reads, and each
//! group is capped where it meets the bare bar.  Segment colours come from the
//! theme (`status_bar`, `status_segment`, `status_soft`, `status_mark` plus
//! the mode styles); with `separators = "plain"` the row falls back to
//! space-separated text in the terminal's own colours.
//!
//! It is one row, and it says only what is true of the buffer: messages, key
//! echoes and prompts are transient, and they float on a card over the panes
//! (see [`crate::ui`]) instead of borrowing this row for a moment.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::config::{StatusConfig, StatusSeparators};
use crate::diagnostic::DiagnosticCount;
use crate::editor::{Editor, StatusLevel};
use crate::theme::Theme;

/// Angled powerline separators, pointing the way the group grows.
const SEPARATOR_RIGHT: &str = "\u{e0b0}";
const SEPARATOR_LEFT: &str = "\u{e0b2}";

/// Everything the status area can show, gathered once per frame.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub mode: String,
    pub file: String,
    pub flags: Vec<&'static str>,
    pub language: String,
    /// Text encoding label of the focused buffer (`utf-8`, `binary`).
    pub encoding: String,
    /// Git branch of the focused buffer's project, once probed.  Filled by the
    /// app — [`Snapshot::collect`] cannot run the probe without blocking.
    pub git_branch: Option<String>,
    /// The language server attached to the focused buffer, if any (also
    /// filled by the app, which owns the clients).
    pub lsp: Option<String>,
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
        snapshot.encoding = if buffer.had_invalid_utf8 {
            "binary".to_string()
        } else {
            "utf-8".to_string()
        };
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

/// Draw the status row.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    config: &StatusConfig,
    snapshot: &Snapshot,
    theme: &Theme,
) {
    let angled = config.separators == StatusSeparators::Angled;
    let base = if angled {
        theme.status_bar
    } else {
        theme.status
    };
    for row in area.rows() {
        buf.set_style(row, base);
    }
    // The sections all live on the one row.
    let Some(row) = area.rows().next() else {
        return;
    };
    let left = left_sections(config, snapshot, theme, angled);
    let right = right_sections(config, snapshot, theme, angled);
    // Angled mode paints every segment from a two-colour fill cycle (the
    // `status_segment` / `status_soft` backgrounds), so neighbouring chips —
    // and therefore the separators between them — always read as distinct.
    let fills = angled.then(|| matched_fills(theme)).flatten();
    let bar_bg = angled.then_some(theme.status_bar.bg).flatten();
    let paint = Paint { angled, fills };
    let (left_end, _, left_last) =
        write_group(row, buf, &left, 0, area.width, SEPARATOR_RIGHT, &paint);

    // One spare cell at the head of the right group holds its leading cap.
    let cap: u16 = if bar_bg.is_some() { 1 } else { 0 };
    let right_width = measure(&right, None, angled) as u16 + cap;
    let start = area.width.saturating_sub(right_width).max(left_end);

    // The left group tapers into the gap when there is room for its cap cell
    // without touching the right group's leading cap.
    if let (Some(bar), Some(last)) = (bar_bg, left_last) {
        if left_end.saturating_add(cap + 1) <= start && left_end < area.width {
            draw_cap(buf, row.y, left_end, SEPARATOR_RIGHT, last, bar);
        }
    }

    if right.is_empty() {
        return;
    }
    // Right-aligned, but never on top of the left hand sections.
    let (_, right_first, _) = write_group(
        row,
        buf,
        &right,
        start + cap,
        area.width,
        SEPARATOR_LEFT,
        &paint,
    );
    // The gap cuts into the first chip of the right group; the cap owns the
    // reserved cell at `start` itself, and '' carries the incoming chip's
    // fill over the bar's colour.
    if let (Some(bar), Some(first)) = (bar_bg, right_first) {
        if start > left_end && start < area.width {
            draw_cap(buf, row.y, start, SEPARATOR_LEFT, first, bar);
        }
    }
}

/// The two fills the angled bar cycles through, when both are known colours.
fn matched_fills(theme: &Theme) -> Option<[Color; 2]> {
    match (theme.status_segment.bg, theme.status_soft.bg) {
        (Some(primary), Some(secondary)) => Some([primary, secondary]),
        _ => None,
    }
}

/// A cap separator: the joint between a chip and the bare bar.
fn draw_cap(buf: &mut Buffer, y: u16, x: u16, glyph: &str, fg: Color, bg: Color) {
    buf.set_stringn(x, y, glyph, 1, Style::default().fg(fg).bg(bg));
}

fn left_sections<'a>(
    config: &StatusConfig,
    snapshot: &'a Snapshot,
    theme: &Theme,
    angled: bool,
) -> Vec<Section<'a>> {
    let mut sections = Vec::new();
    if config.show_mode {
        let style = mode_style(snapshot, theme);
        sections.push(Section {
            text: mode_label(snapshot).to_string().into(),
            style: if angled {
                mode_chip(style, theme)
            } else {
                style
            },
            priority: 0,
            keep_fill: true,
        });
    }
    if config.show_git_branch {
        if let Some(branch) = &snapshot.git_branch {
            sections.push(Section {
                text: branch.clone().into(),
                style: if angled {
                    theme.status_mark
                } else {
                    theme.label_focused.patch(theme.status)
                },
                priority: 2,
                keep_fill: false,
            });
        }
    }
    if config.show_pane_name {
        let mut label = snapshot.file.clone();
        if !label.is_empty() {
            if snapshot.flags.contains(&"modified") {
                label.push('+');
            }
            sections.push(Section {
                text: label.into(),
                style: pick(angled, theme.status_segment, theme.label),
                priority: 1,
                keep_fill: false,
            });
        }
    }
    if let Some((index, total)) = snapshot.pane {
        if total > 1 {
            sections.push(Section {
                text: format!("pane {index}/{total}").into(),
                style: pick(angled, theme.status_soft, theme.status_inactive),
                priority: 4,
                keep_fill: false,
            });
        }
    }
    sections
}

fn right_sections<'a>(
    config: &StatusConfig,
    snapshot: &'a Snapshot,
    theme: &Theme,
    angled: bool,
) -> Vec<Section<'a>> {
    let mut sections = Vec::new();
    // Right group is built left to right.  The cursor position owns the far
    // corner; the language server and encoding sit beside it, and the
    // transient notices (search, jobs) tuck in closest to the panes' middle.
    if let Some(term) = &snapshot.search {
        sections.push(Section {
            text: format!("/{term} ({} match)", snapshot.search_hits).into(),
            style: if angled {
                // The buffer highlight is reversed to stand out over syntax
                // colours, and the soft chips have their own ink.  Neither
                // belongs on a filled chip: keep the search colour as the ink
                // and borrow only the fill, so the angled separators — which
                // carry the fills — match the block they join.
                let mut chip = theme.search_match.remove_modifier(Modifier::REVERSED);
                if let Some(bg) = theme.status_soft.bg {
                    chip = chip.bg(bg);
                }
                chip
            } else {
                theme.search_match
            },
            priority: 5,
            keep_fill: false,
        });
    }
    if snapshot.jobs > 0 {
        sections.push(Section {
            text: format!("{} job(s)", snapshot.jobs).into(),
            style: pick(angled, theme.status_soft, theme.status_inactive),
            priority: 7,
            keep_fill: false,
        });
    }
    if config.show_diagnostics && !snapshot.diagnostics.is_empty() {
        sections.push(Section {
            text: snapshot.diagnostics.summary().into(),
            style: pick(angled, theme.status_soft, theme.status_inactive),
            priority: 3,
            keep_fill: false,
        });
    }
    if let Some(split_mode) = snapshot.split_mode {
        sections.push(Section {
            text: split_mode.to_string().into(),
            style: pick(angled, theme.status_soft, theme.status_inactive),
            priority: 6,
            keep_fill: false,
        });
    }
    if config.show_language && !snapshot.language.is_empty() {
        sections.push(Section {
            text: snapshot.language.clone().into(),
            style: pick(angled, theme.status_soft, theme.status_inactive),
            priority: 4,
            keep_fill: false,
        });
    }
    if config.show_encoding && !snapshot.encoding.is_empty() {
        sections.push(Section {
            text: snapshot.encoding.clone().into(),
            style: pick(angled, theme.status_soft, theme.status_inactive),
            priority: 4,
            keep_fill: false,
        });
    }
    if config.show_lsp {
        if let Some(server) = &snapshot.lsp {
            sections.push(Section {
                text: server.clone().into(),
                style: pick(angled, theme.status_mark, theme.label_focused),
                priority: 3,
                keep_fill: false,
            });
        }
    }
    if config.show_percentage {
        if let Some(percentage) = snapshot.percentage {
            sections.push(Section {
                text: format!("{percentage}%").into(),
                style: pick(angled, theme.status_soft, theme.label),
                priority: 2,
                keep_fill: false,
            });
        }
    }
    if config.show_position {
        if let Some((line, column)) = snapshot.position {
            sections.push(Section {
                text: format!("{line}:{column}").into(),
                style: pick(angled, theme.status_segment, theme.label),
                priority: 2,
                keep_fill: false,
            });
        }
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

/// Turn a mode's text style into a solid chip: the mode colour becomes the
/// fill and the text flips to whichever ink reads on it.  A style that already
/// carries its own background (a `[theme]` override like `"black on green"`)
/// is used as given.
fn mode_chip(style: Style, theme: &Theme) -> Style {
    if style.bg.is_some() {
        return style.add_modifier(Modifier::BOLD);
    }
    let ink = match theme.scheme {
        crate::theme::Scheme::Dark => Color::Black,
        crate::theme::Scheme::Light => Color::White,
    };
    Style::default()
        .fg(ink)
        .bg(style.fg.unwrap_or(theme.scheme.accent()))
        .add_modifier(Modifier::BOLD)
}

/// In plain mode segments keep the theme's own (background-free) styles; only
/// angled rendering paints them.  `pick` papers over that difference.
fn pick(angled: bool, filled: Style, plain: Style) -> Style {
    if angled { filled } else { plain }
}

struct Section<'a> {
    text: std::borrow::Cow<'a, str>,
    style: Style,
    /// Lower survives truncation first.
    priority: u8,
    /// Keep this section's own background instead of joining the fill cycle —
    /// the mode chip's colour is its whole message.
    keep_fill: bool,
}

impl<'a> Section<'a> {
    /// The effective paint in angled mode: the fill cycle gives every
    /// neighbour a different background; plain mode keeps the semantic style.
    fn painted(&self, fills: Option<[Color; 2]>, index: usize) -> Style {
        match fills {
            Some(fills) if !self.keep_fill => self
                .style
                .patch(Style::default().bg(fills[index % fills.len()])),
            _ => self.style,
        }
    }
}

/// Cells a group of kept sections occupies.  Angled segments carry one space
/// of padding on each side and are joined by a separator cell; plain sections
/// carry their one trailing space.
fn measure(sections: &[Section<'_>], keep: Option<&[bool]>, angled: bool) -> usize {
    let pad = if angled { 2 } else { 1 };
    let kept = sections
        .iter()
        .enumerate()
        .filter(|(index, _)| keep.is_none_or(|keep| keep[*index]))
        .map(|(_, section)| section.text.chars().count() + pad);
    let total: usize = kept.sum();
    if total == 0 {
        return 0;
    }
    let separators = if angled {
        sections
            .iter()
            .enumerate()
            .filter(|(index, _)| keep.is_none_or(|keep| keep[*index]))
            .count()
            - 1
    } else {
        0
    };
    total + separators
}

/// How one group of sections is painted this frame.
struct Paint {
    angled: bool,
    /// The two fills the segments cycle through in angled mode; `None` keeps
    /// each section's own background (plain mode, or a theme without fills).
    fills: Option<[Color; 2]>,
}

/// Write sections from `start` up to `limit`, dropping the least important ones
/// that do not fit.  In angled mode each section takes its background from the
/// two-colour fill cycle (unless it is anchored, like the mode chip), so every
/// separator lands between visibly different fills.
///
/// Returns the column after the last section written together with the first
/// and last fills used — the ends a cap separator joins to elsewhere on the
/// row.
fn write_group(
    row: Rect,
    buf: &mut Buffer,
    sections: &[Section<'_>],
    start: u16,
    limit: u16,
    separator: &str,
    paint: &Paint,
) -> (u16, Option<Color>, Option<Color>) {
    let angled = paint.angled;
    let fills = paint.fills;
    let available = limit.saturating_sub(start) as usize;
    let mut keep: Vec<bool> = sections.iter().map(|_| true).collect();
    let mut width = measure(sections, Some(&keep), angled);
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
        width = measure(sections, Some(&keep), angled);
    }

    let mut x = start;
    let mut previous_bg: Option<Color> = None;
    let mut first_bg: Option<Color> = None;
    let mut drawn = 0usize;
    for (index, section) in sections.iter().enumerate() {
        if !keep[index] {
            continue;
        }
        let remaining = limit.saturating_sub(x) as usize;
        if remaining < 2 {
            break;
        }
        // The separator borrows its colours from the segments it joins, and
        // which side it carries depends on where the triangle's base sits:
        // '' is filled against its left edge (the previous chip), ''
        // against its right edge (the incoming chip).  Either way each chip
        // flows into the next without a seam.
        let style = section.painted(fills, drawn);
        if angled {
            if let Some(previous) = previous_bg {
                let incoming = style.bg.unwrap_or(Color::Reset);
                let (fg, bg) = if separator == SEPARATOR_LEFT {
                    (incoming, previous)
                } else {
                    (previous, incoming)
                };
                buf.set_stringn(x, row.y, separator, 1, Style::default().fg(fg).bg(bg));
                x += 1;
            }
        }
        let remaining = limit.saturating_sub(x) as usize;
        let pad = if angled { 2 } else { 1 };
        if remaining <= pad {
            break;
        }
        let text = truncate(&section.text, remaining.saturating_sub(pad));
        let span_width = text.chars().count() as u16;
        let painted = if angled {
            format!(" {text} ")
        } else {
            format!("{text} ")
        };
        buf.set_stringn(x, row.y, painted, (span_width + pad as u16) as usize, style);
        x += span_width + pad as u16;
        previous_bg = Some(style.bg.unwrap_or(Color::Reset));
        if first_bg.is_none() {
            first_bg = previous_bg;
        }
        drawn += 1;
    }
    (x, first_bg, previous_bg)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            mode: "normal".into(),
            file: "main.rs".into(),
            flags: vec!["modified"],
            language: "rust".into(),
            encoding: "utf-8".into(),
            git_branch: Some("main".into()),
            lsp: Some("rust-analyzer".into()),
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
        }
    }

    fn rendered(area: Rect, config: &StatusConfig, snap: &Snapshot, theme: &Theme) -> String {
        let mut buffer = Buffer::empty(area);
        render(area, &mut buffer, config, snap, theme);
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn renders_sections_into_the_status_row() {
        let area = Rect::new(0, 0, 100, 1);
        let rendered = rendered(
            area,
            &StatusConfig::default(),
            &snapshot(),
            &Theme::default(),
        );
        assert!(rendered.contains("NORMAL"), "{rendered}");
        assert!(rendered.contains("main.rs+"), "{rendered}");
        assert!(rendered.contains("12:4"), "{rendered}");
        assert!(rendered.contains("30%"), "{rendered}");
    }

    #[test]
    fn angled_mode_draws_branch_encoding_and_server() {
        let area = Rect::new(0, 0, 100, 1);
        let rendered = rendered(
            area,
            &StatusConfig::default(),
            &snapshot(),
            &Theme::default(),
        );
        assert!(rendered.contains("main"), "{rendered}"); // git branch
        assert!(rendered.contains("utf-8"), "{rendered}");
        assert!(rendered.contains("rust-analyzer"), "{rendered}");
        assert!(rendered.contains(SEPARATOR_RIGHT), "{rendered}");
        assert!(rendered.contains(SEPARATOR_LEFT), "{rendered}");
    }

    #[test]
    fn separators_continue_their_segments_fill() {
        let area = Rect::new(0, 0, 100, 1);
        let mut buffer = Buffer::empty(area);
        render(
            area,
            &mut buffer,
            &StatusConfig::default(),
            &snapshot(),
            &Theme::default(),
        );
        // Every separator cell must carry the fills of both segments it joins,
        // with the triangle's colour on the side its base sits: '' is filled
        // from the left (fg = left neighbour), '' from the right (fg = right
        // neighbour).  Otherwise the chips do not join seamlessly.
        let row: Vec<Option<ratatui::buffer::Cell>> = (0..area.width)
            .map(|x| buffer.cell((x, 0)).cloned())
            .collect();
        let mut seen = 0;
        for (x, cell) in row.iter().enumerate() {
            let cell = cell.as_ref().expect("cell inside the area");
            let points_left = cell.symbol() == SEPARATOR_LEFT;
            if cell.symbol() != SEPARATOR_RIGHT && !points_left {
                continue;
            }
            seen += 1;
            let left = row[x - 1].as_ref().unwrap().style().bg;
            let right = row[x + 1].as_ref().unwrap().style().bg;
            let (want_fg, want_bg) = if points_left {
                (right, left)
            } else {
                (left, right)
            };
            assert_eq!(cell.style().fg, want_fg, "separator at {x} loses a fill");
            assert_eq!(cell.style().bg, want_bg, "separator at {x} loses a fill");
            // Neighbouring segments differ in fill, so every separator shows.
            assert_ne!(
                cell.style().fg,
                cell.style().bg,
                "separator at {x} joins two identical fills"
            );
        }
        assert!(seen >= 4, "expected several separators, saw {seen}");
    }

    #[test]
    fn search_chip_separators_match_its_fill() {
        let area = Rect::new(0, 0, 120, 1);
        let mut snap = snapshot();
        snap.search = Some("foo".into());
        snap.search_hits = 1;
        let theme = Theme::default();
        let mut buffer = Buffer::empty(area);
        render(area, &mut buffer, &StatusConfig::default(), &snap, &theme);

        let row: Vec<&ratatui::buffer::Cell> = (0..area.width)
            .map(|x| buffer.cell((x, 0)).expect("cell inside the area"))
            .collect();
        let label = "/foo (1 match)";
        let width = label.chars().count();
        let start = row
            .windows(width)
            .position(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == label)
            .expect("search chip is drawn");
        let chip = &row[start..start + width];
        // The buffer highlight is reversed so it stands out over syntax colours;
        // a filled chip must not be, or its text flips to the fill colour while
        // the separators still carry the fills.
        assert!(
            chip.iter()
                .all(|cell| !cell.style().has_modifier(Modifier::REVERSED)),
            "search chip inverts its colours"
        );
        assert!(
            chip.iter()
                .all(|cell| cell.style().fg == theme.search_match.fg)
        );
        let fill = chip[0].style().bg.expect("search chip has a fill");
        assert_ne!(
            Some(fill),
            theme.search_match.fg,
            "search ink must not be the chip fill"
        );
        let separator = row[start + width + 1]; // one trailing space of padding
        assert_eq!(separator.symbol(), SEPARATOR_LEFT);
        assert_eq!(
            separator.style().bg,
            Some(fill),
            "separator does not carry the search chip's fill"
        );
        assert_ne!(separator.style().fg, separator.style().bg);
    }

    #[test]
    fn groups_are_capped_against_the_bare_bar() {
        let area = Rect::new(0, 0, 100, 1);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        render(
            area,
            &mut buffer,
            &StatusConfig::default(),
            &snapshot(),
            &theme,
        );
        let bar = theme.status_bar.bg.expect("the angled bar has a fill");
        let row: Vec<Option<ratatui::buffer::Cell>> = (0..area.width)
            .map(|x| buffer.cell((x, 0)).cloned())
            .collect();
        let cells: Vec<&ratatui::buffer::Cell> =
            row.iter().map(|cell| cell.as_ref().unwrap()).collect();
        // The left group tapers into the gap ('' carrying the bar colour as
        // its background) and the gap cuts into the right group ('' carrying
        // it as its foreground).
        assert!(
            cells
                .iter()
                .any(|cell| cell.symbol() == SEPARATOR_RIGHT && cell.style().bg == Some(bar)),
            "no trailing cap fading the left group into the bar"
        );
        assert!(
            cells.iter().any(|cell| cell.symbol() == SEPARATOR_LEFT
                && cell.style().bg == Some(bar)
                && cell.style().fg != Some(bar)),
            "no leading cap cutting the bar into the right group"
        );
    }

    #[test]
    fn plain_mode_has_no_separators() {
        let area = Rect::new(0, 0, 100, 1);
        let config = StatusConfig {
            separators: StatusSeparators::Plain,
            ..StatusConfig::default()
        };
        let rendered = rendered(area, &config, &snapshot(), &Theme::default());
        assert!(rendered.contains("NORMAL"), "{rendered}");
        assert!(!rendered.contains(SEPARATOR_RIGHT), "{rendered}");
        assert!(!rendered.contains(SEPARATOR_LEFT), "{rendered}");
    }

    #[test]
    fn narrow_widths_drop_the_least_important_sections() {
        let area = Rect::new(0, 0, 26, 1);
        let rendered = rendered(
            area,
            &StatusConfig::default(),
            &snapshot(),
            &Theme::default(),
        );
        assert!(rendered.contains("NORMAL"), "{rendered}");
        assert!(rendered.contains("main.rs+"), "{rendered}");
        assert!(!rendered.contains("rust-analyzer"), "{rendered}");
    }

    #[test]
    fn disabled_sections_disappear() {
        let config = StatusConfig {
            show_position: false,
            show_percentage: false,
            show_language: false,
            show_encoding: false,
            show_lsp: false,
            show_git_branch: false,
            ..StatusConfig::default()
        };
        let area = Rect::new(0, 0, 100, 1);
        let rendered = rendered(area, &config, &snapshot(), &Theme::default());
        assert!(!rendered.contains("12:4"), "{rendered}");
        assert!(!rendered.contains("30%"), "{rendered}");
        assert!(!rendered.contains("utf-8"), "{rendered}");
        assert!(!rendered.contains("rust-analyzer"), "{rendered}");
    }

    #[test]
    fn missing_branch_and_server_leave_their_segments_out() {
        let mut snap = snapshot();
        snap.git_branch = None;
        snap.lsp = None;
        let area = Rect::new(0, 0, 100, 1);
        let rendered = rendered(area, &StatusConfig::default(), &snap, &Theme::default());
        // "main" is the branch; with it gone only "main.rs+" remains.
        assert!(rendered.contains("main.rs+"), "{rendered}");
        assert!(!rendered.contains("rust-analyzer"), "{rendered}");
    }
}
