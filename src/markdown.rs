//! In-buffer markdown decoration, after the fashion of *render-markdown.nvim*.
//!
//! Nothing outside tables is moved or reflowed: a line keeps its character count
//! and every substituted glyph is exactly one column wide, so cell `i` still
//! stands for buffer character `i`.  That single invariant is what lets
//! decoration ride along with the rest of the editor — cursor arithmetic, soft
//! wrap, selection ranges and click hit-testing all keep working in buffer
//! coordinates, because none of the geometry changes.  Only how a character is
//! *painted* does:
//!
//! * markup is **concealed**, not deleted — `**bold**` paints spaces over the
//!   asterisks and bold between them;
//! * block markers are **substituted** for same-width glyphs: a bullet becomes `•`,
//!   a quote's `>` becomes `│`, a rule's dashes become `─────`;
//! * a table is **reflowed at draw time**: each column widens to its widest cell,
//!   the pipes land in one column of the screen on every row, and alignment
//!   follows the delimiter row's colons.  This is the one place where a cell is
//!   not character-for-character, which is exactly why [`LineRender::columns`]
//!   exists — painted geometry resolves through it (or its inverse, owners) so
//!   even table rows keep caret and clicks honest; saved text stays as written.
//! * the line under the cursor **reveals** its markup: you edit characters you
//!   can see (see [`LineRender::reveal`]).
//!
//! Two consequences worth stating plainly.  Decoration is a property of how a
//! buffer is *shown*, never of what is stored: writing still writes plain
//! markdown, and turning `editor.markdown-render` off leaves not a byte changed.
//! And it needs no tree-sitter grammar — the rules are line analysis on text
//! that is being drawn anyway, which is why markdown has no grammar wired up in
//! [`crate::lang`].

use ratatui::style::{Modifier, Style};

/// The styles decoration paints with.
///
/// A plain data bag so this module depends on nothing else in the editor: the UI
/// builds one from the active theme, tests use [`MarkdownStyles::default`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkdownStyles {
    /// Body text.
    pub text: Style,
    /// `# Heading`.
    pub heading1: Style,
    /// `## Heading`.
    pub heading2: Style,
    /// `### Heading` and deeper.
    pub heading3: Style,
    /// `**strong**`.
    pub bold: Style,
    /// `*emphasis*`.
    pub italic: Style,
    /// `~~struck~~`.
    pub strike: Style,
    /// `` `code` `` and fenced code blocks.
    pub code: Style,
    /// A link's visible text.
    pub link: Style,
    /// Concealed markup — the marker characters painted over with spaces.
    pub marker: Style,
    /// A `> quote` line.
    pub quote: Style,
    /// The substituted bullet of an unordered list item.
    pub bullet: Style,
    /// The `1.` of an ordered list item.
    pub list_number: Style,
    /// A task list's `[ ]` / `[x]` box.
    pub checkbox: Style,
    /// A thematic break (`---`) and a setext underline.
    pub rule: Style,
    /// Table borders drawn from the row's own `|` and `-`.
    pub table_border: Style,
    /// A table's header row.
    pub table_header: Style,
}

impl Default for MarkdownStyles {
    fn default() -> Self {
        let text = Style::default();
        let bold = text.add_modifier(Modifier::BOLD);
        Self {
            text,
            heading1: bold,
            heading2: bold,
            heading3: bold.add_modifier(Modifier::DIM),
            bold,
            italic: text.add_modifier(Modifier::ITALIC),
            strike: text.add_modifier(Modifier::CROSSED_OUT),
            code: text.add_modifier(Modifier::DIM),
            link: text.add_modifier(Modifier::UNDERLINED),
            marker: text.add_modifier(Modifier::DIM),
            quote: text.add_modifier(Modifier::ITALIC),
            bullet: bold,
            list_number: text.add_modifier(Modifier::DIM),
            checkbox: bold,
            rule: text.add_modifier(Modifier::DIM),
            table_border: text.add_modifier(Modifier::DIM),
            table_header: bold,
        }
    }
}

/// What a line is, as far as painting it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// Prose: inline markup only.
    Text,
    /// An ATX or setext heading.
    Heading { level: u8 },
    /// Inside a fenced block; `delimiter` marks the opening and closing lines.
    Code { delimiter: bool },
    /// A run of dashes rendered as one unbroken rule.
    Rule,
    /// A `>` quote, with its number of leading markers.
    Quote { marks: usize },
    /// A `-` / `*` / `+` item; the value is the marker's character index.
    Bullet { at: usize },
    /// An `N.` item: where its digits start and how many there are.
    Ordered { at: usize, digits: usize },
    /// A table row, in its role within the grid.
    Table(Role),
}

/// A row's part in a markdown table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Header,
    Border,
    Body,
}

/// How one source line is to be painted.
#[derive(Debug, Clone, PartialEq)]
pub struct LineRender {
    /// The glyph for each character of the source line.
    ///
    /// Same count and order as the source; differs only where decoration hid a
    /// marker (a space) or substituted a symbol — unless [`LineRender::reveal`]
    /// asked for this line's markup to stay visible, or [`LineRender::reflowed`]
    /// says table columns were widened.
    pub glyphs: Vec<char>,
    /// One style per character, indexed exactly like [`LineRender::glyphs`].
    pub styles: Vec<Style>,
    /// Whether the source characters of this line are shown as written.
    ///
    /// The line holding the cursor reveals its markup — the hashes, asterisks and
    /// pipes that decoration would otherwise conceal or substitute are painted in
    /// place — while still taking the decoration's colours.  A character you can
    /// only edit by guessing at is a character you should be able to see, but the
    /// reader of every other line keeps the clean rendering.
    pub reveal: bool,
    /// The display column each source character starts at, plus a final entry
    /// for the total width — `n + 1` entries in all, monotonic.
    ///
    /// Empty means the identity: character `i` occupies cell `i`, which is what
    /// every non-table line and every revealed or un-uniform table row does.
    /// A reflowed table row widens padded cells so a column's pipes land in the
    /// same cells on every row of the grid; that breaks one-cell-per-character
    /// *in the painting only*, which is why everything downstream — caret,
    /// selection, click hit-testing — must go through this map instead of
    /// assuming cell equals character.  Characters swallowed by padding (spaces
    /// trimmed out of an over-padded cell) keep a zero-width entry, so offsets
    /// stay monotonic and the caret simply lands on the next real column.
    pub columns: Vec<u32>,
    /// The source character owning each display cell, `glyphs.len()` entries.
    ///
    /// The inverse of [`LineRender::columns`] at cell granularity — padding
    /// belongs to whichever real character stands beside it, so a style layer or
    /// a click in blank space still resolves to a character that exists.
    pub owners: Vec<u32>,
}

impl LineRender {
    fn new(source: &str, base: Style) -> Self {
        Self {
            glyphs: source.chars().collect(),
            styles: vec![base; source.chars().count()],
            reveal: false,
            columns: Vec::new(),
            owners: Vec::new(),
        }
    }

    /// Whether this row's display geometry differs from its character count.
    pub fn reflowed(&self) -> bool {
        !self.columns.is_empty()
    }

    /// The display column where source character `index` is drawn (the line's
    /// width for an index past the end).
    pub fn column_of(&self, index: usize) -> usize {
        if self.columns.is_empty() {
            return index;
        }
        // A zero-width character was absorbed into padding behind it, and the
        // backward fill gives it the next real column anyway.
        let at = index.min(self.columns.len() - 1);
        self.columns[at] as usize
    }

    /// The source character whose drawn cell sits at display column `column`.
    pub fn character_at(&self, column: usize) -> usize {
        if self.columns.is_empty() {
            return column;
        }
        let cells = self.owners.len();
        self.owners[column.min(cells.saturating_sub(1))] as usize
    }

    /// The text to paint on this line's row.
    pub fn display(&self) -> String {
        self.glyphs.iter().collect()
    }
}

/// What every line of a document is, with no per-character storage yet.
///
/// A table row and a line inside an open fence are facts about *other* lines, so
/// classification has to read the whole document; painting does not, and this type
/// is what lets the two be separated — the UI plans once per frame and renders only
/// the rows it can actually see.
#[derive(Debug, Clone)]
pub struct Plan {
    blocks: Vec<Block>,
    /// Per line: which table run it belongs to, indexing `regions`.  A line in a
    /// reflowable run paints from the run's normalised grid so every column has
    /// one width; runs that cannot be made uniform (a row without outer pipes,
    /// a tab inside a cell) are absent and draw character-for-character.
    region_of: Vec<Option<usize>>,
    regions: Vec<Region>,
}

/// Column widths and alignments shared by every row of one table.
#[derive(Debug, Clone, PartialEq)]
struct Region {
    /// Display width of each column's content; a cell is drawn as one space,
    /// the padded content, one space — so a column is `widths[k] + 2` wide
    /// between its pipes.
    widths: Vec<usize>,
    aligns: Vec<Align>,
}

/// How a column's cells sit inside their field, read off the delimiter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

impl Plan {
    /// Classify every line of `lines`.
    pub fn new(lines: &[&str]) -> Self {
        let fences = fence_map(lines);
        let mut blocks: Vec<Block> = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if fences[index].inside {
                    Block::Code {
                        delimiter: fences[index].delimiter,
                    }
                } else if let Some(role) = table_role(lines, index, &fences) {
                    Block::Table(role)
                } else if let Some(level) = atx_level(line) {
                    Block::Heading { level }
                } else if is_rule(line) {
                    Block::Rule
                } else if let Some(marks) = quote_marks(line) {
                    Block::Quote { marks }
                } else if let Some(at) = bullet_at(line) {
                    Block::Bullet { at }
                } else if let Some((at, digits)) = ordered_at(line) {
                    Block::Ordered { at, digits }
                } else {
                    Block::Text
                }
            })
            .collect();

        // Setext headings.  An `===` or `---` line under prose titles it: the marker
        // line paints as a rule and the text above is promoted to a heading.  The
        // same dashes with nothing above are an ordinary thematic break, which is why
        // this needs the neighbouring line rather than the shape of this one alone.
        for index in 1..lines.len() {
            if fences[index].inside || matches!(blocks[index], Block::Table(_)) {
                continue;
            }
            let Some(level) = setext_level(lines[index]) else {
                continue;
            };
            if !matches!(blocks[index], Block::Rule | Block::Text) {
                continue;
            }
            let titled = matches!(blocks[index - 1], Block::Text)
                && !lines[index - 1].trim().is_empty()
                && !fences[index - 1].inside;
            if titled {
                blocks[index - 1] = Block::Heading { level };
                blocks[index] = Block::Rule;
            }
        }

        let (region_of, regions) = table_regions(lines, &fences, &blocks);
        Self {
            blocks,
            region_of,
            regions,
        }
    }

    /// The shared grid of the table containing `index`, if it reflows.
    fn region(&self, index: usize) -> Option<&Region> {
        self.regions.get(*self.region_of.get(index)?.as_ref()?)
    }

    /// How many lines were classified.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Paint `lines[index]`.  `None` when the index is outside the plan, which is
    /// how a document that gained lines between frames stays drawable.
    ///
    /// `reveal` shows that line's markup as written (see [`LineRender::reveal`]);
    /// the UI passes it for the line under the cursor.  A table row that does not
    /// reveal comes back reflowed to its region's grid, mapping and all — see
    /// [`LineRender::columns`].
    pub fn line(
        &self,
        lines: &[&str],
        index: usize,
        reveal: bool,
        styles: &MarkdownStyles,
    ) -> Option<LineRender> {
        let block = *self.blocks.get(index)?;
        let source = *lines.get(index)?;
        if !reveal {
            if let Block::Table(role) = block {
                if let Some(grid) = self
                    .region(index)
                    .and_then(|r| grid_row(source, role, r, styles))
                {
                    return Some(grid);
                }
            }
        }
        Some(render(source, block, reveal, styles))
    }

    /// The classification of one line, for callers that only need to know whether
    /// anything would change.
    pub fn block(&self, index: usize) -> Option<Block> {
        self.blocks.get(index).copied()
    }
}

/// Plan and paint a whole document at once — the convenience path tests and any
/// caller without a viewport uses.
pub fn decorate(lines: &[&str], styles: &MarkdownStyles) -> Vec<LineRender> {
    let plan = Plan::new(lines);
    (0..lines.len())
        .filter_map(|index| plan.line(lines, index, false, styles))
        .collect()
}

/// The decorated rows of one viewport.
#[derive(Debug, Clone)]
pub struct Decorated {
    first_line: usize,
    /// One entry per line of the requested window, in order.  `None` where the text
    /// had no such line keeps the indices aligned without offset arithmetic.
    renders: Vec<Option<LineRender>>,
}

impl Decorated {
    /// The decoration of buffer line `line`, when this window covers it.
    pub fn get(&self, line: usize) -> Option<&LineRender> {
        self.renders
            .get(line.checked_sub(self.first_line)?)?
            .as_ref()
    }
}

/// Plan over `text` and paint only the lines in `window`.
///
/// Classification reads every line — an open fence or a table can start anywhere
/// above the viewport — but glyph and style storage is allocated for the drawn rows
/// alone, so what decoration costs per frame follows the window rather than the
/// file.  Lines past the end of the text come back as `None`, so a document edited
/// between planning and painting simply loses its decoration on that row until the
/// next frame.
pub fn decorate_window(
    text: &str,
    window: std::ops::Range<usize>,
    active_line: Option<usize>,
    styles: &MarkdownStyles,
) -> Decorated {
    let lines: Vec<&str> = text.lines().collect();
    let plan = Plan::new(&lines);
    let renders = window
        .clone()
        .map(|index| plan.line(&lines, index, Some(index) == active_line, styles))
        .collect();
    Decorated {
        first_line: window.start,
        renders,
    }
}

// ------------------------------------------------------------------- painting

/// Paint one line according to its classification.
///
/// `reveal` keeps the source characters in place — concealment and glyph
/// substitution are skipped, colours are not — which is how the cursor's own line
/// shows you the markup you would otherwise be editing blind (see
/// [`LineRender::reveal`]).
fn render(source: &str, block: Block, reveal: bool, styles: &MarkdownStyles) -> LineRender {
    let base = match block {
        Block::Text | Block::Bullet { .. } | Block::Ordered { .. } => styles.text,
        Block::Heading { level } => heading_style(level, styles),
        // A fence line is markup wearing a code style.
        Block::Code { delimiter: true } => styles.code.patch(styles.marker),
        Block::Code { delimiter: false } => styles.code,
        Block::Rule => styles.rule,
        Block::Quote { .. } => styles.quote,
        Block::Table(Role::Header) => styles.table_header,
        Block::Table(_) => styles.text,
    };
    let mut out = LineRender::new(source, base);
    out.reveal = reveal;

    match block {
        // Prose and code bodies have no block marker to substitute; a table row's
        // markers *are* its grid and get drawn in the pass below.
        Block::Code { .. } | Block::Text | Block::Table(_) => {}
        Block::Heading { .. } if !reveal => heading(&mut out, styles),
        Block::Rule if !reveal => rule(&mut out),
        // Revealed, these keep their markers exactly as written.
        Block::Heading { .. } | Block::Rule => {}
        Block::Quote { marks } => quote(source, marks, &mut out, styles),
        Block::Bullet { at } => bullet(at, &mut out, styles),
        Block::Ordered { at, digits } => ordered(at, digits, &mut out, styles),
    }

    match block {
        // Table borders are drawn over the row's pipes and dashes; there is no prose
        // markup to look for inside a cell.
        Block::Table(role) => table(&mut out, role, styles),
        // A fenced body is verbatim: `# not a heading` must stay exactly that.
        Block::Code { .. } => {}
        other => inline(source, prose_start(other, source), &mut out, base, styles),
    }

    out
}

/// Where a line's decorated prose begins, past whatever block marker opened it.
fn prose_start(block: Block, source: &str) -> usize {
    match block {
        // The marker occupies its own cell plus the space after it; anything further
        // — including a task list's `[ ]` box — is prose.
        Block::Bullet { at } => at + 2,
        Block::Ordered { at, digits } => at + digits + 2,
        Block::Quote { marks } => leading_width(source) + marks + space_after(source, marks),
        // A heading's concealed hashes are prose too far; the walk starts after them.
        Block::Heading { .. } => source.chars().take_while(|&c| c == '#').count(),
        _ => 0,
    }
}

/// One optional space following a block marker run.
fn space_after(source: &str, marks: usize) -> usize {
    let index = leading_width(source) + marks;
    usize::from(source.chars().nth(index).is_some_and(|c| c == ' '))
}

fn heading_style(level: u8, styles: &MarkdownStyles) -> Style {
    match level {
        0 | 1 => styles.heading1,
        2 => styles.heading2,
        _ => styles.heading3,
    }
}

/// Conceal a heading's `#` markers; its title keeps the column it was written in.
fn heading(out: &mut LineRender, styles: &MarkdownStyles) {
    let hashes = out.glyphs.iter().take_while(|&&c| c == '#').count();
    hide(out, 0..hashes, styles.marker);

    // A closing run is markup only when a space separates it from the title; the
    // `#` of `C# rocks` has to survive.
    if out.glyphs.get(hashes) == Some(&' ') {
        let mut start = out.glyphs.len();
        while start > hashes + 1 && out.glyphs[start - 1] == '#' {
            start -= 1;
        }
        if start < out.glyphs.len() && out.glyphs[start - 1] == ' ' {
            hide(out, start..out.glyphs.len(), styles.marker);
        }
    }
}

/// Render a thematic break as one unbroken line.
fn rule(out: &mut LineRender) {
    for glyph in out.glyphs.iter_mut() {
        if matches!(glyph, '-' | '=' | '*' | '_') {
            *glyph = '─';
        }
    }
}

/// Substitute a quote's `>` markers with vertical rules.
fn quote(source: &str, marks: usize, out: &mut LineRender, styles: &MarkdownStyles) {
    let indent = leading_width(source);
    for offset in 0..marks {
        let index = indent + offset;
        if out.glyphs.get(index) == Some(&'>') {
            if !out.reveal {
                out.glyphs[index] = '│';
            }
            out.styles[index] = styles.quote;
        }
    }
}

/// Substitute a bullet for the typographic one.
fn bullet(at: usize, out: &mut LineRender, styles: &MarkdownStyles) {
    if let Some(glyph) = out.glyphs.get_mut(at) {
        if matches!(glyph, '-' | '*' | '+') {
            if !out.reveal {
                *glyph = '•';
            }
            out.styles[at] = styles.bullet;
        }
    }
}

/// Dim an ordered item's number.
fn ordered(at: usize, digits: usize, out: &mut LineRender, styles: &MarkdownStyles) {
    for index in at..(at + digits + 1).min(out.styles.len()) {
        out.styles[index] = styles.list_number;
    }
}

/// Draw a table row's borders from the pipes and dashes it already contains.
fn table(out: &mut LineRender, role: Role, styles: &MarkdownStyles) {
    let reveal = out.reveal;
    for index in 0..out.glyphs.len() {
        match out.glyphs[index] {
            '|' => {
                if !reveal {
                    out.glyphs[index] = border_glyph(&out.glyphs, index, role);
                }
                out.styles[index] = styles.table_border;
            }
            '-' if role == Role::Border && !reveal => {
                out.glyphs[index] = '─';
                out.styles[index] = styles.table_border;
            }
            // A colon says how the column aligns, so it stays visible on the border.
            ':' if role == Role::Border => out.styles[index] = styles.table_border,
            _ => {}
        }
    }
    if role == Role::Border {
        unpad_border_rule(out, styles);
    }
}

/// Join a border row into one continuous rule.
///
/// Cells are padded with spaces, and a rule broken by blanks reads as separate
/// strokes rather than one line.  Filling those padding columns is still one glyph
/// per character — a space becomes `─` in its own cell — so nothing moves; only the
/// run between the row's outermost marks is touched, leaving any indentation alone.
fn unpad_border_rule(out: &mut LineRender, styles: &MarkdownStyles) {
    if out.reveal {
        return;
    }
    let is_mark = |c: char| matches!(c, '│' | '┼' | '├' | '┤' | '─' | ':');
    let Some(first) = out.glyphs.iter().position(|&c| is_mark(c)) else {
        return;
    };
    let Some(last) = out.glyphs.iter().rposition(|&c| is_mark(c)) else {
        return;
    };
    for index in first..=last {
        if out.glyphs[index] == ' ' {
            out.glyphs[index] = '─';
            out.styles[index] = styles.table_border;
        }
    }
}

/// The box-drawing glyph for a `|` in the grid.
///
/// A pipe crossing the dash run both ways is `┼`; one at either end of the run is an
/// elbow; anywhere else it is a plain vertical rule.  Cells are usually padded, so
/// the dashes do not touch the pipe — the search steps over the spaces that separate
/// them, which is what makes a written table look like a drawn box rather than a row
/// of unrelated pipes.
fn border_glyph(row: &[char], index: usize, role: Role) -> char {
    if role != Role::Border {
        return '│';
    }
    let left = neighbour_past_spaces(row, index, -1).is_some_and(is_border_run);
    let right = neighbour_past_spaces(row, index, 1).is_some_and(is_border_run);
    match (left, right) {
        (true, true) => '┼',
        (false, true) => '├',
        (true, false) => '┤',
        (false, false) => '│',
    }
}

/// The first non-space character `direction` cells away, if the row has one.
fn neighbour_past_spaces(row: &[char], index: usize, direction: isize) -> Option<char> {
    let mut offset = index as isize + direction;
    while offset >= 0 && (offset as usize) < row.len() {
        let c = row[offset as usize];
        if c != ' ' {
            return Some(c);
        }
        offset += direction;
    }
    None
}

/// A character belonging to a border run rather than to text.
fn is_border_run(c: char) -> bool {
    matches!(c, '-' | '─' | ':' | '=')
}

// ---------------------------------------------------------------- table grids
//
// A written table is only as straight as its author padded it.  Decoration
// therefore sizes each column to the widest cell in the run — including the
// delimiter row's dashes, so a rule never outstays its cells — and re-emits
// every row over that shared layout: one space of padding, the content placed
// by the column's alignment, the rest filled with spaces.  Alignment comes
// from the delimiter row's colons (`:-` left, `-:` right, `:-:` centre), the
// same hints the CommonMark spec reads.
//
// The re-emitted row is *not* one glyph per source character any more, which
// is exactly why every drawn cell records its owning character and every
// character its display column (`LineRender::columns`).  Everything that
// counts positions — caret, selection, click hit-testing — resolves through
// those two arrays instead of assuming the old identity.

/// A table row split into its pipes and the cells between them.
struct RowCells {
    /// Character index of every unescaped `|`, outer ones included.
    pipes: Vec<usize>,
    /// Cell k runs from `pipes[k] + 1` to `pipes[k + 1]`.
    cells: Vec<std::ops::Range<usize>>,
}

/// Which source characters may be dropped to padding without breaking a grid.
fn table_regions(
    lines: &[&str],
    _fences: &[Fence],
    blocks: &[Block],
) -> (Vec<Option<usize>>, Vec<Region>) {
    let mut region_of = vec![None; blocks.len()];
    let mut regions: Vec<Region> = Vec::new();
    let mut start = 0usize;
    while start < blocks.len() {
        if !matches!(blocks[start], Block::Table(_)) {
            start += 1;
            continue;
        }
        let end = (start..blocks.len())
            .find(|&i| !matches!(blocks[i], Block::Table(_)))
            .unwrap_or(blocks.len());
        if let Some(region) = measure_region(lines, blocks, start..end) {
            let id = regions.len();
            regions.push(region);
            for slot in &mut region_of[start..end] {
                *slot = Some(id);
            }
        }
        start = end;
    }
    (region_of, regions)
}

/// Size one run of table rows, if the run can form a single grid.
///
/// A row without both outer pipes, a tab inside a cell (padding arithmetic has
/// no notion of tab stops), or a differing pipe count between rows makes the
/// run refused: every row then draws character-for-character as before rather
/// than aligning with half its neighbours and not the rest.
fn measure_region(lines: &[&str], blocks: &[Block], run: std::ops::Range<usize>) -> Option<Region> {
    let mut widths: Option<Vec<usize>> = None;
    let mut aligns: Option<Vec<Align>> = None;
    for index in run {
        let chars: Vec<char> = lines[index].chars().collect();
        let row = split_row(&chars)?;
        let columns = row.cells.len();
        if columns == 0 {
            return None;
        }
        let w = widths.get_or_insert_with(|| vec![0; columns]);
        if w.len() != columns {
            return None; // ragged pipe counts refuse the run
        }
        for (k, cell) in row.cells.iter().enumerate() {
            let trimmed = trim_range(&chars, cell.clone());
            w[k] = w[k].max(trimmed.len());
        }
        // The delimiter row is the column's alignment declaration.  Keep the
        // first one: a second border would be another table wedged against
        // this one, and its row is still content width-wise.
        if aligns.is_none() && blocks[index] == Block::Table(Role::Border) {
            let mut found = Vec::with_capacity(columns);
            for cell in &row.cells {
                let trimmed: String = chars[cell.clone()].iter().collect();
                let t = trimmed.trim_matches(' ');
                let left = t.starts_with(':');
                let right = t.ends_with(':') && t.len() > 1;
                found.push(match (left, right) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                });
            }
            aligns = Some(found);
        }
    }
    let widths = widths?;
    Some(Region {
        aligns: aligns.unwrap_or_else(|| vec![Align::Left; widths.len()]),
        widths,
    })
}

/// Split a line at its unescaped pipes.  `None` refuses the row — see
/// [`measure_region`] for what is refused and why.
fn split_row(chars: &[char]) -> Option<RowCells> {
    if chars.contains(&'\t') {
        return None;
    }
    let pipes: Vec<usize> = (0..chars.len())
        .filter(|&i| chars[i] == '|' && unescaped(chars, i))
        .collect();
    if pipes.len() < 2 {
        return None;
    }
    // Only indentation may precede the opening pipe, and only spaces follow
    // the closing one — otherwise the row's edges are not where the grid puts
    // them and the row is refused rather than painted over someone's prose.
    if !chars[..pipes[0]].iter().all(|&c| c == ' ') {
        return None;
    }
    if !chars[pipes[pipes.len() - 1] + 1..]
        .iter()
        .all(|&c| c == ' ')
    {
        return None;
    }
    let cells = pipes[..pipes.len() - 1]
        .iter()
        .zip(&pipes[1..])
        .map(|(&open, &close)| open + 1..close)
        .collect();
    Some(RowCells { pipes, cells })
}

/// Whether `chars[i]` stands free — not escaped by a run of backslashes.
fn unescaped(chars: &[char], i: usize) -> bool {
    let mut backs = 0usize;
    let mut before = i;
    while before > 0 && chars[before - 1] == '\\' {
        backs += 1;
        before -= 1;
    }
    backs % 2 == 0
}

/// `range` with its leading and trailing spaces removed.
fn trim_range(chars: &[char], range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let mut start = range.start;
    let mut end = range.end;
    while start < end && chars[start] == ' ' {
        start += 1;
    }
    while end > start && chars[end - 1] == ' ' {
        end -= 1;
    }
    start..end
}

/// Re-emit `source` over its region's grid, mapping every character.
///
/// `None` when this particular row cannot take the grid (a pipe count that
/// disagrees with the run); the caller falls back to painting it verbatim, so
/// a table edited mid-frame never fails to draw — at worst one row is briefly
/// crooked until the next keystroke resizes nothing, because measurement and
/// painting read the same lines.
fn grid_row(
    source: &str,
    role: Role,
    region: &Region,
    styles: &MarkdownStyles,
) -> Option<LineRender> {
    let chars: Vec<char> = source.chars().collect();
    let row = split_row(&chars)?;
    if row.cells.len() != region.widths.len() {
        return None;
    }
    let base = match role {
        Role::Header => styles.table_header,
        _ => styles.text,
    };

    // Cell-by-cell construction: each painted cell records the source character
    // that owns it.  Padding a cell needs but its source never wrote is absorbed
    // by the neighbouring mark (the pipe before a left pad, the one after a
    // right pad), so no display cell is ever ownerless.
    let mut glyphs: Vec<char> = Vec::with_capacity(chars.len() + 8);
    let mut owners: Vec<u32> = Vec::with_capacity(chars.len() + 8);
    for index in 0..row.pipes[0] {
        glyphs.push(' '); // the row's own indentation, untouched
        owners.push(index as u32);
    }
    let columns = region.widths.len();
    for k in 0..columns {
        glyphs.push('|');
        owners.push(row.pipes[k] as u32);
        let cell = &row.cells[k];
        let trimmed = trim_range(&chars, cell.clone());
        let width = region.widths[k].max(trimmed.len());
        let content = trimmed.len();
        let extra_left = match region.aligns.get(k).copied().unwrap_or(Align::Left) {
            Align::Left => 0,
            Align::Center => (width - content) / 2,
            Align::Right => width - content,
        };
        for j in 0..=extra_left {
            // One guaranteed space of breathing room plus the alignment pad;
            // the row's own leading spaces fill these cells first.
            let lead = trimmed.start - cell.start;
            let owner = if j < lead {
                cell.start + j
            } else {
                row.pipes[k] // written padding this column does not need
            };
            glyphs.push(' ');
            owners.push(owner as u32);
        }
        for index in trimmed.clone() {
            glyphs.push(chars[index]);
            owners.push(index as u32);
        }
        let right = width - content - extra_left;
        for j in 0..=right {
            // Same trick on the closing side: real trailing spaces take cells
            // first, surplus padding waits at the next pipe.
            let tail_owner = if cell.end - trimmed.end > j {
                trimmed.end + j
            } else {
                row.pipes[k + 1]
            };
            glyphs.push(' ');
            owners.push(tail_owner as u32);
        }
    }
    // The closing pipe.  A cell-less tail still has this last mark to own any
    // surplus padding the row gained.
    glyphs.push('|');
    owners.push(row.pipes[columns] as u32);
    for index in row.pipes[columns] + 1..chars.len() {
        glyphs.push(' '); // trailing spaces after the closing pipe
        owners.push(index as u32);
    }

    let mut out = LineRender {
        glyphs,
        styles: vec![base; owners.len()],
        reveal: false,
        columns: Vec::new(),
        owners,
    };
    // The shared border pass does the rest exactly as it did character-for-
    // character: pipes become rules and crossings, dashes fill their fields.
    table(&mut out, role, styles);

    // Fill the character → column map from first-owned cells; owners advance
    // monotonically along the row, so the first cell an owner appears in marks
    // its start.  A space this grid trimmed away owns no cell and collapses onto
    // whatever follows it: zero-width, monotonic, and a click near one still
    // resolves to a real neighbour.
    let width = out.glyphs.len() as u32;
    let mut columns_map = vec![u32::MAX; chars.len() + 1];
    for (cell, owner) in out.owners.iter().enumerate() {
        let owner = *owner as usize;
        if columns_map[owner] == u32::MAX {
            columns_map[owner] = cell as u32;
        }
    }
    for index in (0..chars.len()).rev() {
        if columns_map[index] == u32::MAX {
            columns_map[index] = columns_map[index + 1];
        }
    }
    columns_map[chars.len()] = width;
    out.columns = columns_map;
    Some(out)
}

// --------------------------------------------------------------------- inline

/// Decorate the inline markup in `source`, starting at character index `from`.
///
/// The walk reads the untouched source characters and writes only styles and glyph
/// substitutions, so nested constructs resolve by recursing into the enclosed range:
/// nothing is inserted or removed, so nothing needs re-indexing.
fn inline(source: &str, from: usize, out: &mut LineRender, base: Style, styles: &MarkdownStyles) {
    let source: Vec<char> = source.chars().collect();
    walk(&source, out, from.min(out.glyphs.len()), base, styles);
}

/// Walk one line's characters to its end, styling and concealing markup.
fn walk(
    src: &[char],
    out: &mut LineRender,
    mut index: usize,
    base: Style,
    styles: &MarkdownStyles,
) {
    while index < src.len() {
        let c = src[index];

        // A backslash escapes the next character; only the backslash is hidden.
        if c == '\\' && index + 1 < src.len() {
            hide(out, index..index + 1, styles.marker);
            out.styles[index + 1] = base;
            index += 2;
            continue;
        }

        // Inline code: verbatim content between the backticks.
        if c == '`' {
            let run = run_length(src, index, '`');
            if let Some(close) = find_run(src, index + run, '`', run) {
                hide(out, index..index + run, styles.marker);
                hide(out, close..close + run, styles.marker);
                patch(&mut out.styles, index + run..close, base.patch(styles.code));
                index = close + run;
                continue;
            }
        }

        // A task box reads as one control rather than three characters of markup.
        if c == '[' && is_checkbox(src, index) {
            patch(&mut out.styles, index..index + 3, styles.checkbox);
            index += 3;
            continue;
        }

        // `[text](target)` and `![alt](src)`.  The text is what a link *means*, so it
        // alone takes the link colour; brackets, target and the image's `!` are dimmed
        // but kept, because hiding the target would hide information the reader may
        // need and a writer has to edit.
        if matches!(c, '[' | '!') {
            let image = c == '!' && src.get(index + 1) == Some(&'[');
            let bracket = index + usize::from(image);
            if src.get(bracket) == Some(&'[') {
                if let Some((label, target)) = link_parts(src, bracket) {
                    patch(&mut out.styles, bracket..label.start, styles.marker);
                    patch(&mut out.styles, label.clone(), base.patch(styles.link));
                    patch(&mut out.styles, label.end..target.end, styles.marker);
                    index = target.end;
                    continue;
                }
            }
        }

        // Emphasis.  The whole delimiter run is taken at once, so `***x***` reads as
        // one bold-italic pair rather than as `**` followed by a stray asterisk.
        if let Some((marker, run)) = emphasis_run(src, index) {
            if let Some(close) = find_run(src, index + run, marker, run) {
                hide(out, index..index + run, styles.marker);
                hide(out, close..close + run, styles.marker);
                let emphasis = emphasis_style(marker, run, base, styles);
                patch(&mut out.styles, index + run..close, emphasis);
                // Nested markup inside the pair, at the style this pair established.
                walk(&src[..close], out, index + run, emphasis, styles);
                index = close + run;
                continue;
            }
        }

        index += 1;
    }
}

/// What a delimiter run means: `~` always strikes, while `*` and `_` grow heavier
/// the longer the run is.
fn emphasis_style(marker: char, run: usize, base: Style, styles: &MarkdownStyles) -> Style {
    if marker == '~' {
        return base.patch(styles.strike);
    }
    match run {
        1 => base.patch(styles.italic),
        2 => base.patch(styles.bold),
        _ => base.patch(styles.bold).patch(styles.italic),
    }
}

/// The emphasis delimiter opening at `index`, and its run length.
fn emphasis_run(src: &[char], index: usize) -> Option<(char, usize)> {
    let c = src[index];
    if !matches!(c, '*' | '_' | '~') {
        return None;
    }
    let run = run_length(src, index, c);
    // An opener has to be followed by content.  `~` only strikes in pairs, and `_`
    // may not open inside a word — which is what keeps `snake_case` upright.
    if !src
        .get(index + run)
        .is_some_and(|next| !next.is_whitespace())
    {
        return None;
    }
    match c {
        '~' if run < 2 => None,
        '_' if index > 0 && src[index - 1].is_alphanumeric() => None,
        _ => Some((c, run)),
    }
}

/// Length of the run of `needle` starting at `index`.
fn run_length(src: &[char], index: usize, needle: char) -> usize {
    src[index..].iter().take_while(|&&c| c == needle).count()
}

/// The next run of at least `length` copies of `needle`, at or after `from`.
///
/// A closer has to sit against content on its left, which is what keeps the
/// arithmetic in `2 * 3 * 4` from italicising the `3`.
fn find_run(src: &[char], mut index: usize, needle: char, length: usize) -> Option<usize> {
    while index + length <= src.len() {
        let opens_here = index > 0 && !src[index - 1].is_whitespace();
        if src[index] == needle && run_length(src, index, needle) >= length && opens_here {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// The `[label](target)` opening at `bracket`, as character ranges.
fn link_parts(
    src: &[char],
    bracket: usize,
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let label_start = bracket + 1;
    let label_end = (label_start..src.len()).find(|&k| src[k] == ']')?;
    if src.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let target_start = label_end + 2;
    let target_end = (target_start..src.len()).find(|&k| src[k] == ')')?;
    Some((label_start..label_end, label_end..target_end + 1))
}

/// A `[ ]` or `[x]` task box.
fn is_checkbox(src: &[char], index: usize) -> bool {
    matches!(src.get(index + 1), Some(' ' | 'x' | 'X')) && src.get(index + 2) == Some(&']')
}

/// Paint a range over with spaces: concealment that keeps every column.
///
/// On a revealed line the characters stay and only take the marker style — the
/// markup has to remain legible where the cursor is working on it.
fn hide(out: &mut LineRender, range: std::ops::Range<usize>, style: Style) {
    for index in range {
        if let Some(glyph) = out.glyphs.get_mut(index) {
            if !out.reveal {
                *glyph = ' ';
            }
        }
        if let Some(entry) = out.styles.get_mut(index) {
            *entry = entry.patch(style);
        }
    }
}

/// Layer a style over a range, keeping whatever it already carried.
fn patch(styles: &mut [Style], range: std::ops::Range<usize>, style: Style) {
    for index in range {
        if let Some(entry) = styles.get_mut(index) {
            *entry = entry.patch(style);
        }
    }
}

// -------------------------------------------------------------- line analysis

/// Fence state of one line.
#[derive(Clone, Copy)]
struct Fence {
    /// Inside a fenced block, including the fence lines themselves.
    inside: bool,
    /// This line opens or closes the block.
    delimiter: bool,
}

fn fence_map(lines: &[&str]) -> Vec<Fence> {
    let mut out = vec![
        Fence {
            inside: false,
            delimiter: false
        };
        lines.len()
    ];
    // The open fence's character and run length; a closer must use the same
    // character and be at least as long.
    let mut open: Option<(char, usize)> = None;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        match open {
            None => {
                if let Some((marker, run)) = fence_run(trimmed) {
                    if run >= 3 {
                        out[index] = Fence {
                            inside: true,
                            delimiter: true,
                        };
                        // The info string of ```` ```rust ```` is part of the opening
                        // fence, so the block stays open either way.
                        open = Some((marker, run));
                    }
                }
            }
            Some((marker, run)) => {
                out[index] = Fence {
                    inside: true,
                    delimiter: false,
                };
                let closing =
                    fence_run(trimmed).is_some_and(|(c, length)| c == marker && length >= run);
                // Only a line of nothing but fence characters closes the block.  A
                // longer run inside the block is content, not a closer: erring this
                // way loses one border rather than swallowing the rest of the file.
                let bare =
                    trimmed.chars().count() == run.max(fence_run(trimmed).unwrap_or((marker, 0)).1);
                if closing && bare {
                    out[index].delimiter = true;
                    open = None;
                }
            }
        }
    }

    out
}

/// The leading fence run of a line that opens with backticks or tildes.
fn fence_run(trimmed: &str) -> Option<(char, usize)> {
    let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    Some((marker, trimmed.chars().take_while(|&c| c == marker).count()))
}

/// The row's role in a table, if it is one.
///
/// A markdown table is defined by its delimiter row: pipes alone do not make a grid,
/// which is what keeps `a | b` in prose from being boxed.  Once that row is found the
/// line above is the header and every following pipe-bearing line is body until
/// something else breaks the run.
fn table_role(lines: &[&str], index: usize, fences: &[Fence]) -> Option<Role> {
    let trimmed = lines[index].trim();
    if fences[index].inside || !trimmed.contains('|') || leading_width(lines[index]) > 3 {
        return None;
    }
    if is_border_row(trimmed) {
        // A border needs a header above it, or it is just a line of dashes.
        let header_above = index > 0 && lines[index - 1].contains('|');
        return header_above.then_some(Role::Border);
    }
    if index + 1 < lines.len() && is_border_row(lines[index + 1].trim()) {
        return Some(Role::Header);
    }
    body_below_a_border(lines, index, fences)
}

/// Whether `index` continues the body of a table whose border sits above it.
fn body_below_a_border(lines: &[&str], index: usize, fences: &[Fence]) -> Option<Role> {
    let mut above = index;
    while above > 0 {
        above -= 1;
        if fences[above].inside || !lines[above].contains('|') {
            return None;
        }
        if is_border_row(lines[above].trim()) {
            return Some(Role::Body);
        }
    }
    None
}

/// The `| --- | :--: |` row under a table header.
fn is_border_row(trimmed: &str) -> bool {
    if !trimmed.contains('-') || !trimmed.contains('|') {
        return false;
    }
    // Drop the outer pipes, then every cell must be dashes with optional colons at
    // its ends — `-` plain, `:-:` centred, `--:` right-aligned.  A blank between two
    // pipes (`| |`) has no rule in it and so is not a border.
    let body = trimmed.trim_matches('|').trim();
    if body.is_empty() {
        return false;
    }
    body.split('|').all(|cell| {
        let cell = cell.trim();
        !cell.is_empty() && cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':'))
    })
}

/// Character columns of leading spaces (markdown opens a block at up to three).
fn leading_width(source: &str) -> usize {
    source.chars().take_while(|&c| c == ' ').count()
}

/// ATX heading level `1..=6`, for a line indented no more than three columns.
fn atx_level(source: &str) -> Option<u8> {
    if leading_width(source) > 3 {
        return None;
    }
    let trimmed = source.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    // The space is what separates a heading from `#hashtag`.
    let titled = (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ');
    titled.then_some(hashes as u8)
}

/// Level of a setext underline: all `=` titles level 1, all `-` level 2.
fn setext_level(source: &str) -> Option<u8> {
    let compact = compact(source);
    if compact.is_empty() {
        return None;
    }
    if compact.chars().all(|c| c == '=') {
        Some(1)
    } else if compact.chars().all(|c| c == '-') && compact.len() >= 2 {
        Some(2)
    } else {
        None
    }
}

/// A thematic break: three or more of one single `-`, `*` or `_`.
fn is_rule(source: &str) -> bool {
    if leading_width(source) > 3 {
        return false;
    }
    let compact = compact(source);
    // One character repeated, so `-=-` and `--x--` stay prose.
    let Some(marker) = compact.chars().next() else {
        return false;
    };
    compact.len() >= 3 && matches!(marker, '-' | '*' | '_') && compact.chars().all(|c| c == marker)
}

/// A line's characters with every space removed, for marker tests.
fn compact(source: &str) -> String {
    source
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Number of `>` markers opening a quote line.
fn quote_marks(source: &str) -> Option<usize> {
    if leading_width(source) > 3 {
        return None;
    }
    let trimmed = source.trim_start();
    // `>` alone (an empty quoted line) and `> text` are both quotes; `->x` is not.
    let marker = trimmed.chars().next()?;
    if marker != '>' {
        return None;
    }
    let marks = trimmed.chars().take_while(|&c| c == '>').count();
    let after = trimmed.chars().nth(marks);
    matches!(after, None | Some(' ')).then_some(marks)
}

/// The bullet of an unordered item and the character index it sits at.
fn bullet_at(source: &str) -> Option<usize> {
    if leading_width(source) > 3 {
        return None;
    }
    let at = leading_width(source);
    let rest = source.get(at..)?;
    // The space is what separates `- item` from `--` or `***bold***`.
    let opens = matches!(rest.chars().next(), Some('-' | '*' | '+'))
        && rest.chars().nth(1).is_some_and(|c| c == ' ');
    opens.then_some(at)
}

/// An ordered item's first digit and how many digits its number has.
fn ordered_at(source: &str) -> Option<(usize, usize)> {
    if leading_width(source) > 3 {
        return None;
    }
    let at = leading_width(source);
    let rest = source.get(at..)?;
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let numbered = rest[digits..].starts_with(". ") || rest[digits..].starts_with(") ");
    numbered.then_some((at, digits))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles() -> MarkdownStyles {
        MarkdownStyles::default()
    }

    /// Paint one line of a document.
    fn render_line(lines: &[&str], index: usize) -> LineRender {
        decorate(lines, &styles())[index].clone()
    }

    fn display(lines: &[&str], index: usize) -> String {
        render_line(lines, index).display()
    }

    /// The style of one character of one decorated line.
    fn style_of(lines: &[&str], line: usize, character: usize) -> Style {
        render_line(lines, line).styles[character]
    }

    #[test]
    fn prose_is_left_as_written() {
        assert_eq!(
            display(&["an ordinary sentence"], 0),
            "an ordinary sentence"
        );
        assert_eq!(style_of(&["plain"], 0, 0), styles().text);
    }

    #[test]
    fn a_heading_hides_its_hashes_and_keeps_the_columns() {
        let lines = ["# Title"];
        // The `#` becomes a space, so the title still starts at column 2.
        assert_eq!(display(&lines, 0), "  Title");
        assert!(style_of(&lines, 0, 2).add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_deeper_heading_is_quieter_than_a_shallow_one() {
        // The fallback bag distinguishes levels by weight alone; the theme gives each
        // its own colour, which `ui::markdown_styles` is tested for.
        let one = style_of(&["# One"], 0, 2);
        let three = style_of(&["### Three"], 0, 4);
        assert!(one.add_modifier.contains(Modifier::BOLD));
        assert!(three.add_modifier.contains(Modifier::BOLD | Modifier::DIM));
    }

    #[test]
    fn a_closing_hash_run_is_concealed_but_a_sharp_sign_survives() {
        assert_eq!(display(&["# Title #"], 0), "  Title  ");
        // `C#` in a heading title is not markup, so it survives its own hash.
        assert_eq!(display(&["## Why C# rocks"], 0), "   Why C# rocks");
    }

    #[test]
    fn emphasis_reaches_the_text_between_its_markers() {
        let lines = ["a **b** c"];
        // Both marker pairs are painted over with spaces; the `b` sits at its own
        // column and carries the weight.
        assert_eq!(display(&lines, 0), "a   b   c");
        assert!(style_of(&lines, 0, 4).add_modifier.contains(Modifier::BOLD));
        // The concealed markers take the marker style, not the emphasis.
        assert!(!style_of(&lines, 0, 2).add_modifier.contains(Modifier::BOLD));

        // `x *y* z`: the `y` is at its written column, index 3.
        let ital = style_of(&["x *y* z"], 0, 3);
        assert!(ital.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn triple_markers_carry_both_emphases() {
        let both = style_of(&["***both***"], 0, 3);
        assert!(
            both.add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn emphasis_nests_inside_emphasis() {
        let lines = ["**bold and *ital* end**"];
        // The `i` of `ital`, twelve characters in: italic from its own markers and
        // bold from the run wrapped around it.
        let inner = style_of(&lines, 0, 12);
        assert!(
            inner
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn strikethrough_strikes_rather_than_emboldens() {
        let struck = style_of(&["~~gone~~"], 0, 2);
        assert!(struck.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(!struck.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn snake_case_survives_underscore_emphasis() {
        let lines = ["some_snake_case name"];
        assert_eq!(display(&lines, 0), "some_snake_case name");
        for style in &render_line(&lines, 0).styles {
            assert!(!style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn spaced_arithmetic_stays_upright() {
        let lines = ["2 * 3 * 4"];
        for style in &render_line(&lines, 0).styles {
            assert!(!style.add_modifier.contains(Modifier::ITALIC), "{style:?}");
        }
    }

    #[test]
    fn an_unmatched_marker_is_just_a_character() {
        let lines = ["a *lonely star"];
        for style in &render_line(&lines, 0).styles {
            assert!(!style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn inline_code_is_styled_and_its_content_verbatim() {
        let lines = ["use `a **b**` here"];
        // The backticks themselves are concealed, so the content keeps its column.
        assert_eq!(display(&lines, 0), "use  a **b**  here");
        assert!(style_of(&lines, 0, 5).add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn a_link_colours_its_text_and_dims_its_parts() {
        let lines = ["see [docs](https://x.test) now"];
        // Nothing is hidden: the target stays readable for whoever edits it.
        assert_eq!(display(&lines, 0), "see [docs](https://x.test) now");
        assert!(
            style_of(&lines, 0, 5)
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            !style_of(&lines, 0, 10)
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn an_image_is_treated_as_a_link() {
        let lines = ["![alt](cat.png)"];
        assert!(
            style_of(&lines, 0, 3)
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn a_bullet_becomes_a_typographic_one() {
        assert_eq!(display(&["- one"], 0), "• one");
        assert_eq!(display(&["  * two"], 0), "  • two");
    }

    #[test]
    fn an_ordered_item_keeps_its_number() {
        let lines = ["7. seven"];
        assert_eq!(display(&lines, 0), "7. seven");
        assert_eq!(style_of(&lines, 0, 0), styles().list_number);
    }

    #[test]
    fn a_task_box_is_styled_without_losing_its_brackets() {
        for source in ["- [ ] todo", "- [x] done"] {
            let decorated = render_line(&[source], 0);
            // `[ ]` is three characters and stays three columns wide.
            assert_eq!(decorated.glyphs.len(), source.chars().count());
        }
    }

    #[test]
    fn a_quote_becomes_a_vertical_rule() {
        assert_eq!(display(&["> quoted"], 0), "│ quoted");
        assert_eq!(display(&[">"], 0), "│");
    }

    #[test]
    fn nested_quotes_stack_their_rules() {
        assert_eq!(display(&[">> deep thought"], 0), "││ deep thought");
    }

    #[test]
    fn an_arrow_is_not_a_quote() {
        assert_eq!(display(&["-> go"], 0), "-> go");
    }

    #[test]
    fn a_thematic_break_becomes_one_unbroken_line() {
        let lines = ["-----"];
        assert_eq!(display(&lines, 0), "─────");
        assert_eq!(style_of(&lines, 0, 2), styles().rule);
    }

    #[test]
    fn a_mixed_marker_line_is_prose_not_a_rule() {
        // `-=-` is not a thematic break; painting it as one would be a lie.
        assert_eq!(display(&["-=-"], 0), "-=-");
    }

    #[test]
    fn dashes_alone_in_prose_stay_dashes() {
        assert_eq!(display(&["well - known"], 0), "well - known");
    }

    #[test]
    fn a_setext_underline_titles_the_line_above_it() {
        let lines = ["Title", "====="];
        assert!(style_of(&lines, 0, 0).add_modifier.contains(Modifier::BOLD));
        assert_eq!(display(&lines, 1), "─────");
    }

    #[test]
    fn dashes_after_a_blank_line_are_a_break_not_a_title() {
        let lines = ["text", "", "---"];
        assert!(!style_of(&lines, 0, 0).add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_fenced_block_is_code_and_the_text_around_it_is_not() {
        let lines = ["before", "```rust", "let x: i32 = 1;", "```", "after"];
        let decorated = decorate(&lines, &styles());
        assert_eq!(decorated[2].styles[0], styles().code);
        assert_eq!(decorated[2].display(), "let x: i32 = 1;");
        // Prose either side keeps body styling.
        assert_eq!(decorated[4].styles[0], styles().text);
    }

    #[test]
    fn markup_inside_a_fence_is_left_alone() {
        let lines = ["```", "# not a heading | nor | a table", "```"];
        let decorated = decorate(&lines, &styles());
        assert_eq!(decorated[1].display(), "# not a heading | nor | a table");
        assert_eq!(decorated[1].styles[0], styles().code);
    }

    #[test]
    fn an_unclosed_fence_holds_the_rest_of_the_file() {
        let lines = ["```", "still code", "and more"];
        let decorated = decorate(&lines, &styles());
        assert_eq!(decorated[1].styles[0], styles().code);
        assert_eq!(decorated[2].styles[0], styles().code);
    }

    #[test]
    fn a_short_marker_run_is_not_a_fence() {
        let lines = ["``", "plain text"];
        assert_ne!(style_of(&lines, 1, 0), styles().code);
    }

    #[test]
    fn a_table_draws_its_borders_from_its_own_pipes() {
        let lines = ["| name | value |", "| ---- | ----- |", "| a    | 1     |"];
        assert_eq!(display(&lines, 0), "│ name │ value │");
        assert_eq!(display(&lines, 2), "│ a    │ 1     │");
        let border = display(&lines, 1);
        assert!(border.contains('─'), "{border:?}");
        assert!(!border.contains('-'), "{border:?}");
    }

    #[test]
    fn table_intersections_get_corner_glyphs() {
        // Where a pipe crosses the dash run both ways it is `┼`; at either end an
        // elbow.  That is what makes the grid read as a box.  The reflow also
        // grows the packed row to the normal one-space cells, so both rows of
        // this table end up the same width with their pipes in the same columns.
        let lines = ["|a|b|", "|-|-|"];
        assert_eq!(display(&lines, 1), "├───┼───┤");
        assert_eq!(display(&lines, 0), "│ a │ b │");
    }

    #[test]
    fn ragged_rows_grow_to_one_grid() {
        // The whole point of the reflow: a table whose author padded nothing still
        // draws its borders straight.  Every row shares one layout — same column
        // widths, pipes in the same display cells — and each character's map entry
        // names where it ended up.
        let lines = [
            "| name | value |",
            "| --- | ----- |",
            "| a longer cell | 1 |",
        ];
        let decorated = decorate(&lines, &styles());
        assert!(decorated[0].reflowed() && decorated[2].reflowed());
        for render in &decorated {
            assert_eq!(
                render.glyphs.len(),
                decorated[0].glyphs.len(),
                "rows of one grid are the same width"
            );
        }
        // The long cell pushed column one to its own width; the header's `value`
        // travelled there without moving a byte in the buffer.
        let display = |i: usize| decorated[i].display();
        assert!(
            display(2).starts_with("│ a longer cell │"),
            "{}",
            display(0)
        );
        assert_eq!(
            display(0).find('v'),
            Some(display(2).find('1').expect("a value cell")),
            "column two starts at the same cell on every row"
        );
        // And the caret map follows: buffer character 2 is the header's `n…`'s
        // neighbour — it must still sit two columns in, and every column of the
        // map be monotonic with a terminator equal to the drawn width.
        let render = &decorated[0];
        assert_eq!(render.column_of(0), 0);
        for i in 1..=2 {
            assert!(render.columns[i] >= render.columns[i - 1]);
        }
        assert_eq!(
            *render.columns.last().unwrap() as usize,
            render.glyphs.len()
        );

        // The header's `v` — character nine of its line — has grown along with
        // its column: it now draws where the long row's `1` draws, and clicking
        // that cell must find the `v` back.
        let v = render.column_of(9);
        assert!(v > 9, "`value` did not travel with its column: {v}");
        assert_eq!(render.glyphs[v], 'v');
        assert_eq!(render.character_at(v), 9);
    }

    #[test]
    fn a_padded_table_still_gets_a_cross_at_every_pipe() {
        // The packed form above is the degenerate case; tables people write pad the
        // cells, so the dashes do not touch the pipe.  A border row of
        // `| ----- | ----- |` must still cross at every one of its three pipes.
        let lines = ["| feature | status |", "| ------- | :----: |"];
        let border = display(&lines, 1);
        assert!(
            border.starts_with('├'),
            "no elbow opening the rule: {border:?}"
        );
        assert!(
            border.ends_with('┤'),
            "no elbow closing the rule: {border:?}"
        );
        assert_eq!(
            border.chars().filter(|&c| c == '┼').count(),
            1,
            "the interior pipe did not cross: {border:?}"
        );
        assert!(!border.contains('-'), "dashes left unpainted: {border:?}");
        // Padded cells must not leave blanks in the rule.
        assert!(
            !border.contains(' '),
            "the rule is broken by cell padding: {border:?}"
        );
    }

    #[test]
    fn an_indented_table_keeps_its_indentation_inside_the_rule() {
        // The fill stops at the row's outermost marks, so a table nested in a list
        // does not draw border across the space that nests it.
        let lines = ["  | a | b |", "  | - | - |"];
        let border = display(&lines, 1);
        assert!(
            border.starts_with("  │") || border.starts_with("  ├"),
            "{border:?}"
        );
    }

    #[test]
    fn alignment_colons_survive_on_the_border() {
        let lines = ["| a | b | c |", "| :-: | --: | - |"];
        let border = display(&lines, 1);
        assert!(border.contains(':'), "alignment hint lost: {border:?}");
    }

    #[test]
    fn a_table_header_is_emphasised_and_its_body_is_not() {
        let lines = ["| name |", "| ---- |", "| a |"];
        let decorated = decorate(&lines, &styles());
        assert!(decorated[0].styles[2].add_modifier.contains(Modifier::BOLD));
        assert!(!decorated[2].styles[2].add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_stray_pipe_in_prose_draws_no_border() {
        let lines = ["a | b", "c | d"];
        for index in 0..lines.len() {
            assert_eq!(display(&lines, index), lines[index]);
        }
    }

    #[test]
    fn a_blank_line_ends_a_table() {
        let lines = [
            "| a |",
            "| - |",
            "| b |",
            "",
            "prose between the tables",
            "| c |",
            "| - |",
        ];
        assert_eq!(display(&lines, 4), "prose between the tables");
        // The second table's header is still a header on its own merits.
        assert!(style_of(&lines, 5, 0).add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn every_line_keeps_its_exact_character_count() {
        // The invariant the whole design rests on: one buffer character is still one
        // screen cell, so cursor and hit-testing arithmetic stay valid.
        let lines = [
            "# Heading",
            "some **bold** and `code` text with a [link](url)",
            "| a | b |",
            "| - | - |",
            "> quote",
            "- item",
            "1. item",
            "---",
            "",
            "```",
            "body",
            "```",
        ];
        for (source, decorated) in lines.iter().zip(decorate(&lines, &styles())) {
            assert_eq!(
                decorated.glyphs.len(),
                source.chars().count(),
                "character count moved on {source:?}"
            );
            assert_eq!(decorated.styles.len(), decorated.glyphs.len());
        }
    }

    #[test]
    fn substituted_glyphs_are_all_single_width() {
        // A double-width substitute would shove everything after it sideways and
        // desynchronise the cursor from what is on screen.
        use unicode_width::UnicodeWidthChar;
        let lines = [
            "- item",
            "> quote",
            "| a | b |",
            "| - | - |",
            "-----",
            "# H",
            "**bold** `code` ~~struck~~ [l](u)",
        ];
        for decorated in decorate(&lines, &styles()) {
            for glyph in decorated.glyphs {
                assert_eq!(
                    UnicodeWidthChar::width(glyph).unwrap_or(1),
                    1,
                    "{glyph:?} is not one column wide"
                );
            }
        }
    }

    #[test]
    fn decorating_the_same_text_twice_gives_the_same_answer() {
        let lines = ["# Title", "- a **b** c", "| x | y |", "| - | - |"];
        assert_eq!(decorate(&lines, &styles()), decorate(&lines, &styles()));
    }

    #[test]
    fn an_empty_buffer_plans_to_nothing() {
        let plan = Plan::new(&[]);
        assert!(plan.is_empty());
        assert!(decorate(&[], &styles()).is_empty());
    }

    #[test]
    fn a_plan_that_grew_out_of_date_paints_nothing_rather_than_panic() {
        // A document edited between planning and painting is the normal race; the UI
        // asks for a line the plan never saw.
        let plan = Plan::new(&["# one"]);
        assert!(plan.line(&["# one", "two"], 1, false, &styles()).is_none());
    }

    #[test]
    fn only_the_planned_window_is_rendered() {
        // The UI plans over the whole document but paints the viewport; a long file
        // must not pay for glyphs it will never draw.
        let mut lines = vec!["prose line"; 50];
        lines.push("# far down");
        let plan = Plan::new(&lines);
        assert_eq!(plan.len(), lines.len());
        let rendered = plan
            .line(&lines, lines.len() - 1, false, &styles())
            .expect("planned");
        assert!(rendered.styles[2].add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_revealed_line_shows_its_markup_but_keeps_the_colours() {
        // The cursor's own line is where you edit the source, so it must show the
        // characters that are there; every other line keeps the clean rendering.
        let lines = ["# Heading", "- an **item**"];
        let plan = Plan::new(&lines);
        let styles = styles();

        let hidden = plan.line(&lines, 0, false, &styles).expect("planned");
        assert_eq!(hidden.display(), "  Heading");

        let revealed = plan.line(&lines, 0, true, &styles).expect("planned");
        assert_eq!(revealed.display(), "# Heading");
        assert!(revealed.reveal);
        // Still painted as a heading.
        assert!(revealed.styles[2].add_modifier.contains(Modifier::BOLD));

        let revealed = plan.line(&lines, 1, true, &styles).expect("planned");
        assert_eq!(revealed.display(), "- an **item**");
        // The word between the asterisks keeps its emphasis.
        assert!(revealed.styles[7].add_modifier.contains(Modifier::BOLD));

        let hidden = plan.line(&lines, 1, false, &styles).expect("planned");
        assert_eq!(hidden.display(), "• an   item  ");
    }

    #[test]
    fn a_revealed_table_row_keeps_its_pipes_and_dashes() {
        let lines = ["| name |", "| ---- |"];
        let plan = Plan::new(&lines);
        let styles = styles();
        assert_eq!(
            plan.line(&lines, 0, true, &styles)
                .expect("planned")
                .display(),
            "| name |"
        );
        assert_eq!(
            plan.line(&lines, 1, true, &styles)
                .expect("planned")
                .display(),
            "| ---- |"
        );
    }

    #[test]
    fn a_revealed_line_keeps_every_column() {
        // Reveal must not disturb the one-glyph-per-character invariant: wrapping,
        // hit-testing and the caret all count source columns.
        let lines = ["# **Heading**"];
        let plan = Plan::new(&lines);
        let styles = styles();
        for reveal in [false, true] {
            let rendered = plan.line(&lines, 0, reveal, &styles).expect("planned");
            assert_eq!(rendered.glyphs.len(), lines[0].chars().count());
        }
    }
}
