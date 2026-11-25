//! Colours and styles.
//!
//! Defaults are built from the terminal's own ANSI palette plus attributes, so
//! vortex follows the surrounding theme instead of shipping colours of its own.
//! `[theme]` entries in the config override individual elements using a tiny
//! style syntax: `"bold red"`, `"dim"`, `"yellow on #101010"`, `"reverse"`.
//!
//! Whether the terminal is dark or light is worked out by asking it for its
//! background colour (see [`crate::term::query_colors`]); `[theme] background`
//! can pin the answer.  Knowing the actual background makes the quiet parts of
//! the UI — cursor line, selection, gutter, pane borders — a measured blend of
//! the terminal's own colours rather than a guess.

use ratatui::style::{Color, Modifier, Style};

use crate::config::{Background, ThemeConfig};
use crate::diagnostic::Severity;
use crate::syntax::Hl;

/// An RGB triple, as the terminal reports it.
pub type Rgb = (u8, u8, u8);

/// How dark the terminal behind vortex is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scheme {
    #[default]
    Dark,
    Light,
}

impl Scheme {
    /// Honour `[theme] background`; `auto` keeps the measured answer.
    pub fn resolve(config: Background, observed: Observed) -> Self {
        match config {
            Background::Dark => Self::Dark,
            Background::Light => Self::Light,
            Background::Auto => observed.scheme(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// The text colour a terminal of this scheme is most likely to use, used
    /// when the terminal reported its background but not its foreground.
    pub fn ink(self) -> Rgb {
        match self {
            Self::Dark => (255, 255, 255),
            Self::Light => (0, 0, 0),
        }
    }

    /// Accent colour for the focused pane's border and label.
    pub fn accent(self) -> Color {
        match self {
            // Bright cyan reads on any dark background; dark blue on a light one.
            Self::Dark => Color::Indexed(14),
            Self::Light => Color::Indexed(4),
        }
    }
}

/// What vortex managed to learn about the terminal's own palette.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Observed {
    pub background: Option<Rgb>,
    pub foreground: Option<Rgb>,
}

impl Observed {
    pub const UNKNOWN: Observed = Observed {
        background: None,
        foreground: None,
    };

    /// The scheme implied by the measured background, or dark if unknown.
    pub fn scheme(self) -> Scheme {
        self.background.map(scheme_of).unwrap_or_default()
    }
}

/// Relative luminance (WCAG), 0.0 for black and 1.0 for white.
pub fn luminance(rgb: Rgb) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = value as f32 / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
}

/// Classify a background colour.
///
/// The cut is below the middle on purpose: mid-grey backgrounds read better
/// with a light-foreground palette, and mistaking a light background for a
/// dark one is the more annoying error.
pub fn scheme_of(rgb: Rgb) -> Scheme {
    if luminance(rgb) < 0.4 {
        Scheme::Dark
    } else {
        Scheme::Light
    }
}

/// Blend `amount` (0.0..=1.0) of `towards` into `from`.
pub fn mix(from: Rgb, towards: Rgb, amount: f32) -> Rgb {
    let amount = amount.clamp(0.0, 1.0);
    let blend = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    (
        blend(from.0, towards.0),
        blend(from.1, towards.1),
        blend(from.2, towards.2),
    )
}

fn to_color(rgb: Rgb) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// A bold style in one colour.
fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Parse a colour name.  `None` means "leave it to the terminal".
pub fn parse_color(text: &str) -> Option<Color> {
    let text = text.trim().to_lowercase();
    if text.is_empty() || text == "reset" || text == "default" || text == "none" {
        return None;
    }
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex);
    }
    let color = match text.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "gray" | "grey" => Color::DarkGray,
        "bright-black" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        "bright-white" => Color::White,
        _ => {
            if let Some(index) = text.strip_prefix("color") {
                let index: u8 = index.parse().ok()?;
                return Some(Color::Indexed(index));
            }
            return None;
        }
    };
    Some(color)
}

fn parse_hex(hex: &str) -> Option<Color> {
    match hex.len() {
        3 => {
            let bytes = hex.as_bytes();
            let expand = |b: u8| -> Option<u8> {
                let digit = (b as char).to_digit(16)?;
                Some((digit * 17) as u8)
            };
            Some(Color::Rgb(
                expand(bytes[0])?,
                expand(bytes[1])?,
                expand(bytes[2])?,
            ))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

/// Parse `"bold dim red on #202020"`.  Unknown tokens are ignored.
pub fn parse_style(text: &str) -> Style {
    let mut style = Style::default();
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        match token {
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "dim" => style = style.add_modifier(Modifier::DIM),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" | "underlined" => style = style.add_modifier(Modifier::UNDERLINED),
            "reverse" | "inverse" => style = style.add_modifier(Modifier::REVERSED),
            "blink" => style = style.add_modifier(Modifier::SLOW_BLINK),
            "hidden" => style = style.add_modifier(Modifier::HIDDEN),
            "strike" => style = style.add_modifier(Modifier::CROSSED_OUT),
            "on" | "bg" => {
                if let Some(background) = tokens.get(index + 1) {
                    if let Some(color) = parse_color(background) {
                        style = style.bg(color);
                    }
                    index += 1;
                }
            }
            other => {
                if let Some(color) = parse_color(other) {
                    style = style.fg(color);
                }
            }
        }
        index += 1;
    }
    style
}

/// What a card, or a row in one, *is* — expressed in the palette's hues.
///
/// Nothing here is a literal colour: a search picker is the terminal's yellow,
/// whatever that terminal happens to call yellow.  That is the whole point of
/// the enum — the app names the kind of thing, the theme decides the shade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// No opinion.  A card title takes the accent, a row stays plain text.
    #[default]
    Neutral,
    /// Files, buffers and paths.
    File,
    /// Search patterns and their hits.
    Search,
    /// Locations a language server named — definitions, references.
    Location,
    /// Commands and the parameters they ask for.
    Command,
    /// Literal text: a spelling fix, a replacement string.
    Literal,
    /// A problem, graded.
    Error,
    Warning,
    Info,
    Hint,
}

impl From<Severity> for Tone {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Error => Self::Error,
            Severity::Warning => Self::Warning,
            Severity::Info => Self::Info,
            Severity::Hint => Self::Hint,
        }
    }
}

/// The scheme's six working hues, kept around so the quiet chrome can borrow
/// them; [`Theme::default_for`] builds them once per scheme.
#[derive(Debug, Clone, Copy)]
struct Hues {
    red: Color,
    green: Color,
    yellow: Color,
    blue: Color,
    magenta: Color,
    cyan: Color,
    grey: Color,
}

/// Every style vortex draws with.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Which darkness this palette was built for.
    pub scheme: Scheme,
    /// The accent colour: the focused pane's border, a card's outline, the
    /// marker in a picker's selected row.
    pub accent: Color,
    pub text: Style,
    pub mode_normal: Style,
    pub mode_insert: Style,
    pub mode_visual: Style,
    pub mode_command: Style,
    pub border: Style,
    pub border_focused: Style,
    pub label: Style,
    pub label_focused: Style,
    pub selection: Style,
    pub cursor_line: Style,
    pub gutter: Style,
    pub gutter_active: Style,
    pub diagnostic_error: Style,
    pub diagnostic_warning: Style,
    pub diagnostic_info: Style,
    pub diagnostic_hint: Style,
    pub search_match: Style,
    pub search_match_current: Style,
    pub prompt: Style,
    /// The card a picker or palette sits on.  Deliberately a solid background:
    /// an overlay that lets the text behind it show through is unreadable.
    pub overlay: Style,
    /// A card's outline.  A modal is the focused thing on screen, so it takes
    /// the accent the focused pane's border takes.
    pub overlay_border: Style,
    /// A card's title, filled the way the status row fills its accent chip.
    pub overlay_title: Style,
    /// The highlighted row of a picker or completion list.  A step stronger than
    /// the buffer's selection, which shares its space with syntax colours; this
    /// one does not.
    pub picker_selected: Style,
    /// The marker in the highlighted row's gutter, so the row reads as chosen by
    /// shape as well as by colour.
    pub picker_cursor: Style,
    /// A picker's secondary text: the detail column, the preview, the count line.
    pub picker_detail: Style,
    pub status: Style,
    pub status_inactive: Style,
    /// Base fill of the angled status row: the bar behind the segments.
    pub status_bar: Style,
    /// A filled primary segment (file name, cursor position).
    pub status_segment: Style,
    /// A quieter filled segment (encoding, language, split mode …).
    pub status_soft: Style,
    /// Accent-filled segment: git branch and the attached language server.
    pub status_mark: Style,
    pub error: Style,
    keyword: Style,
    string: Style,
    comment: Style,
    number: Style,
    constant: Style,
    function: Style,
    type_name: Style,
    property: Style,
    operator: Style,
    punctuation: Style,
    tag: Style,
    preproc: Style,
    parse_error: Style,
    /// The scheme's hues, for the family and severity tints a picker uses.
    hues: Hues,
    /// `[theme] overlay-title` was set by hand, so it wins over a card's family
    /// hue rather than being tinted by it.
    title_hue_locked: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_for(Scheme::default())
    }
}

impl Theme {
    /// The palette for a terminal of the given darkness.
    ///
    /// Every colour comes from the 16-colour ANSI palette, so the terminal's
    /// own theme still decides what "magenta" means.  The dark scheme leans on
    /// the middle/bright half and the light scheme on the darker half, because
    /// cyan and yellow simply do not read on a white background.
    pub fn default_for(scheme: Scheme) -> Self {
        let dark = scheme == Scheme::Dark;
        let underlined = |c: Color| Style::default().fg(c).add_modifier(Modifier::UNDERLINED);
        // (on dark, on light)
        let hue = |bright: Color, dim: Color| if dark { bright } else { dim };
        let (red, green, yellow, blue, magenta, cyan, grey) = (
            hue(Color::Red, Color::Indexed(1)),
            hue(Color::Green, Color::Indexed(2)),
            hue(Color::Yellow, Color::Indexed(3)),
            hue(Color::Blue, Color::Indexed(4)),
            hue(Color::Magenta, Color::Indexed(5)),
            hue(Color::Cyan, Color::Indexed(6)),
            hue(Color::DarkGray, Color::Indexed(8)),
        );
        // Cyan comments are a dark-background habit; on a light background grey
        // is the one that reads as a comment.
        let mut comment = Style::default()
            .fg(if dark { cyan } else { grey })
            .add_modifier(Modifier::ITALIC);
        if dark {
            comment = comment.add_modifier(Modifier::DIM);
        }
        let hues = Hues {
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            grey,
        };
        Self {
            scheme,
            accent: scheme.accent(),
            text: Style::default(),
            mode_normal: Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
            mode_insert: bold(green),
            mode_visual: bold(yellow),
            mode_command: bold(cyan),
            border: Style::default().add_modifier(Modifier::DIM),
            border_focused: bold(scheme.accent()),
            label: Style::default().add_modifier(Modifier::DIM),
            label_focused: bold(scheme.accent()),
            selection: Style::default().add_modifier(Modifier::REVERSED),
            cursor_line: Style::default(),
            gutter: Style::default().add_modifier(Modifier::DIM),
            gutter_active: Style::default().add_modifier(Modifier::BOLD),
            diagnostic_error: underlined(red),
            diagnostic_warning: underlined(yellow),
            diagnostic_info: underlined(cyan),
            diagnostic_hint: Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::UNDERLINED),
            search_match: Style::default()
                .fg(yellow)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED),
            search_match_current: Style::default()
                .fg(yellow)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
            prompt: Style::default(),
            // A solid surface, so the overlay reads as a card laid over the
            // buffer rather than as text written on top of other text.
            overlay: Style::default().bg(hue(Color::Black, Color::White)),
            // With nothing measured the chrome is 16-colour only: the outline in
            // the accent, the title on the same fill the status chips use, and no
            // shadow at all rather than a guess at one.
            overlay_border: bold(scheme.accent()),
            overlay_title: Style::default()
                .fg(scheme.accent())
                .bg(hue(Color::DarkGray, Color::Gray))
                .add_modifier(Modifier::BOLD),
            picker_selected: Style::default().add_modifier(Modifier::REVERSED),
            picker_cursor: bold(scheme.accent()),
            picker_detail: Style::default().add_modifier(Modifier::DIM),
            status: Style::default().add_modifier(Modifier::BOLD),
            status_inactive: Style::default().add_modifier(Modifier::DIM),
            // The angled bar: a dark/light strip the segments sit on.  Primary
            // segments are filled one shade off the bar so the separators read;
            // `status_soft` keeps the fill but dims the text, and
            // `status_mark` carries the scheme accent.
            status_bar: Style::default()
                .fg(if dark { Color::White } else { Color::Black })
                .bg(hue(Color::Black, Color::White)),
            status_segment: Style::default()
                .fg(if dark { Color::White } else { Color::Black })
                .bg(hue(Color::DarkGray, Color::Gray))
                .add_modifier(Modifier::BOLD),
            // Its own shade between the primary fill and the bar, so the
            // alternating cycle never puts two neighbours in one colour.
            status_soft: Style::default()
                .fg(if dark { Color::White } else { Color::Black })
                .bg(hue(Color::Indexed(236), Color::Indexed(252)))
                .add_modifier(Modifier::DIM),
            status_mark: Style::default()
                .fg(scheme.accent())
                .bg(hue(Color::DarkGray, Color::Gray))
                .add_modifier(Modifier::BOLD),
            error: bold(red),
            keyword: bold(magenta),
            string: Style::default().fg(green),
            comment,
            number: Style::default().fg(red),
            constant: Style::default().fg(magenta),
            function: bold(blue),
            type_name: Style::default().fg(yellow),
            // A key is neither a string nor plain text: config files are mostly
            // keys, so leaving this unstyled paints them in a single colour.
            // Cyan is the one loud hue not already spoken for by keywords,
            // strings, numbers or types (comments borrow it, but italic).
            property: Style::default().fg(hue(Color::Cyan, Color::Indexed(4))),
            operator: Style::default().add_modifier(Modifier::DIM),
            punctuation: Style::default(),
            tag: bold(green),
            preproc: Style::default().fg(cyan),
            parse_error: underlined(red),
            hues,
            title_hue_locked: false,
        }
    }

    /// Build the theme for a terminal whose colours may have been measured.
    ///
    /// Order matters: the scheme's palette, then blends derived from the
    /// terminal's own background, then the `[theme]` overrides — so a setting
    /// in the config always beats a measurement.
    pub fn with_observed(config: &ThemeConfig, observed: Observed) -> Self {
        let scheme = Scheme::resolve(config.background, observed);
        let mut theme = Self::default_for(scheme);
        let unset = |value: &str| value.trim().is_empty();
        // Read once here because the card chrome below wants it, and it has to
        // mean the same thing as the focused pane's border.
        let accent = parse_color(&config.accent).unwrap_or_else(|| scheme.accent());
        theme.accent = accent;
        theme.title_hue_locked = !unset(&config.overlay_title);

        if let Some(background) = observed.background {
            let ink = observed.foreground.unwrap_or_else(|| scheme.ink());
            if unset(&config.cursor_line) {
                theme.cursor_line = Style::default().bg(to_color(mix(background, ink, 0.09)));
            }
            if unset(&config.selection) {
                theme.selection = Style::default()
                    .bg(to_color(mix(background, ink, 0.26)))
                    .fg(to_color(ink));
            }
            if let Some(foreground) = observed.foreground {
                // A shade off the measured background: enough to separate the
                // card from the buffer without inventing a colour.
                theme.overlay = Style::default()
                    .bg(to_color(mix(background, foreground, 0.06)))
                    .fg(to_color(foreground));
            }
            // The card's own chrome, mixed from the terminal's colours: the title
            // on the same fill the status row's primary chips use.
            if unset(&config.overlay_title) {
                theme.overlay_title = theme.overlay_title.bg(to_color(mix(background, ink, 0.18)));
            }
            // One step past the buffer's selection: inside a card nothing else is
            // competing for the row, so it can be the loudest thing on it.
            if unset(&config.picker_selected) {
                theme.picker_selected = Style::default()
                    .bg(to_color(mix(background, ink, 0.34)))
                    .fg(to_color(ink));
            }
            if unset(&config.picker_detail) {
                theme.picker_detail = Style::default().fg(to_color(mix(ink, background, 0.42)));
            }
            if unset(&config.gutter) {
                theme.gutter = Style::default().fg(to_color(mix(ink, background, 0.45)));
            }
            if unset(&config.gutter_active) {
                theme.gutter_active = Style::default()
                    .fg(to_color(ink))
                    .add_modifier(Modifier::BOLD);
            }
            if unset(&config.border) {
                theme.border = Style::default().fg(to_color(mix(ink, background, 0.55)));
            }
            // The status bar follows the terminal: the strip is the measured
            // background itself, and the segment fill a shade off it, so the
            // angled separators read without inventing colours.
            if unset(&config.status_bar) {
                theme.status_bar = Style::default().fg(to_color(ink)).bg(to_color(background));
            }
            if unset(&config.status_segment) {
                let fill = to_color(mix(background, ink, 0.18));
                theme.status_segment = Style::default()
                    .fg(to_color(ink))
                    .bg(fill)
                    .add_modifier(Modifier::BOLD);
                if unset(&config.status_soft) {
                    // A lighter shade than the primary fill — and off the bar
                    // itself — so alternating chips stay distinguishable.
                    theme.status_soft = Style::default()
                        .fg(to_color(mix(ink, background, 0.35)))
                        .bg(to_color(mix(background, ink, 0.09)));
                }
                if unset(&config.status_mark) {
                    theme.status_mark = Style::default()
                        .fg(scheme.accent())
                        .bg(fill)
                        .add_modifier(Modifier::BOLD);
                }
            }
        }
        // The focused pane gets the accent colour, unless the user styled the
        // border or the label themselves.
        if unset(&config.border_focused) {
            theme.border_focused = bold(accent);
        }
        if unset(&config.label_focused) {
            theme.label_focused = bold(accent);
        }
        // A card is focused too, so its outline, its title and the rail in its
        // highlighted row follow the same accent — measured terminal or not.
        if unset(&config.overlay_border) {
            theme.overlay_border = bold(accent);
        }
        if unset(&config.overlay_title) {
            theme.overlay_title = theme.overlay_title.fg(accent);
        }
        if unset(&config.picker_cursor) {
            theme.picker_cursor = bold(accent);
        }
        // The branch/LSP chips follow the accent colour too.
        if unset(&config.status_mark) {
            theme.status_mark = theme.status_mark.fg(accent);
        }
        theme.scheme = scheme;
        theme.apply_overrides(config);
        theme
    }
}

impl Theme {
    /// Built-in palette with `[theme]` overrides applied on top.
    pub fn from_config(config: &ThemeConfig) -> Self {
        Self::with_observed(config, Observed::UNKNOWN)
    }

    /// Paint the config's style strings over whatever palette is in use.
    /// Empty values leave the element's scheme colour alone.
    fn apply_overrides(&mut self, config: &ThemeConfig) {
        let theme = self;
        let override_style = |current: &mut Style, value: &str| {
            if !value.trim().is_empty() {
                *current = parse_style(value);
            }
        };
        override_style(&mut theme.mode_normal, &config.mode_normal);
        override_style(&mut theme.mode_insert, &config.mode_insert);
        override_style(&mut theme.mode_visual, &config.mode_visual);
        override_style(&mut theme.mode_command, &config.mode_command);
        override_style(&mut theme.border, &config.border);
        override_style(&mut theme.border_focused, &config.border_focused);
        override_style(&mut theme.label, &config.label);
        override_style(&mut theme.label_focused, &config.label_focused);
        override_style(&mut theme.selection, &config.selection);
        override_style(&mut theme.cursor_line, &config.cursor_line);
        override_style(&mut theme.gutter, &config.gutter);
        override_style(&mut theme.gutter_active, &config.gutter_active);
        override_style(&mut theme.keyword, &config.keyword);
        override_style(&mut theme.string, &config.string);
        override_style(&mut theme.comment, &config.comment);
        override_style(&mut theme.number, &config.number);
        override_style(&mut theme.function, &config.function);
        override_style(&mut theme.type_name, &config.type_name);
        override_style(&mut theme.constant, &config.constant);
        override_style(&mut theme.property, &config.property);
        override_style(&mut theme.operator, &config.operator);
        override_style(&mut theme.punctuation, &config.punctuation);
        override_style(&mut theme.parse_error, &config.error);
        override_style(&mut theme.diagnostic_error, &config.diagnostic_error);
        override_style(&mut theme.diagnostic_warning, &config.diagnostic_warning);
        override_style(&mut theme.diagnostic_info, &config.diagnostic_info);
        override_style(&mut theme.diagnostic_hint, &config.diagnostic_hint);
        override_style(&mut theme.search_match, &config.search_match);
        override_style(&mut theme.prompt, &config.prompt);
        override_style(&mut theme.overlay, &config.overlay);
        override_style(&mut theme.overlay_border, &config.overlay_border);
        override_style(&mut theme.overlay_title, &config.overlay_title);
        override_style(&mut theme.picker_selected, &config.picker_selected);
        override_style(&mut theme.picker_cursor, &config.picker_cursor);
        override_style(&mut theme.picker_detail, &config.picker_detail);
        override_style(&mut theme.text, &config.text);
        override_style(&mut theme.status_inactive, &config.status_inactive);
        override_style(&mut theme.status_bar, &config.status_bar);
        override_style(&mut theme.status_segment, &config.status_segment);
        override_style(&mut theme.status_soft, &config.status_soft);
        override_style(&mut theme.status_mark, &config.status_mark);
    }

    /// The hue a card's title belongs to.
    ///
    /// [`Tone::Neutral`] resolves to the accent, so a picker nobody gave a family
    /// still wears whatever `[theme] accent` says.
    pub fn card_hue(&self, tone: Tone) -> Color {
        match tone {
            Tone::Neutral => self.accent,
            Tone::File | Tone::Info => self.hues.cyan,
            Tone::Search | Tone::Warning => self.hues.yellow,
            Tone::Location => self.hues.blue,
            Tone::Command => self.hues.magenta,
            Tone::Literal => self.hues.green,
            Tone::Error => self.hues.red,
            // A hint is the palette's quietest voice, and wants to stay that way.
            Tone::Hint => self.hues.grey,
        }
    }

    /// The colour of a row's label, for what the row *is*.
    ///
    /// Deliberately narrower than [`Self::card_hue`]: a list in which every row
    /// is coloured is a list in which nothing reads as highlighted, so anything
    /// the app did not classify stays plain text.
    pub fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Neutral => Style::default(),
            Tone::File => Style::default().fg(self.hues.cyan),
            Tone::Search => Style::default().fg(self.hues.yellow),
            Tone::Location => Style::default().fg(self.hues.blue),
            Tone::Command => Style::default().fg(self.hues.magenta),
            Tone::Literal => Style::default().fg(self.hues.green),
            Tone::Error => Style::default().fg(self.hues.red),
            Tone::Warning => Style::default().fg(self.hues.yellow),
            Tone::Info => Style::default().fg(self.hues.cyan),
            Tone::Hint => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// A card's title chip: the family's hue over the chip's fill.
    ///
    /// A hand-written `[theme] overlay-title` covers every card and wins — it
    /// asked for one title colour, not six.
    pub fn title_style(&self, tone: Tone) -> Style {
        if self.title_hue_locked {
            return self.overlay_title;
        }
        self.overlay_title
            .patch(Style::default().fg(self.card_hue(tone)))
    }

    /// Style for a syntax highlight group.
    pub fn highlight(&self, hl: Hl) -> Style {
        match hl {
            Hl::None => self.text,
            Hl::Keyword => self.keyword,
            Hl::String => self.string,
            Hl::Comment => self.comment,
            Hl::Number => self.number,
            Hl::Constant => self.constant,
            Hl::Function => self.function,
            Hl::Type => self.type_name,
            Hl::Property => self.property,
            Hl::Operator => self.operator,
            Hl::Punctuation => self.punctuation,
            Hl::Tag => self.tag,
            Hl::Preproc => self.preproc,
            Hl::Error => self.parse_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("reset"), None);
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("BRIGHT-BLUE"), Some(Color::LightBlue));
        assert_eq!(parse_color("#fff"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_color("#0a0b0c"), Some(Color::Rgb(10, 11, 12)));
        assert_eq!(parse_color("color42"), Some(Color::Indexed(42)));
        assert_eq!(parse_color("nonsense"), None);
    }

    #[test]
    fn parses_style_strings() {
        let style = parse_style("bold dim red on #101010");
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, Some(Color::Rgb(16, 16, 16)));
        assert!(style.add_modifier.contains(Modifier::BOLD | Modifier::DIM));
    }

    #[test]
    fn config_overrides_are_optional() {
        let theme = Theme::from_config(&ThemeConfig::default());
        assert_eq!(theme.keyword.fg, Some(Color::Magenta));
        let config = ThemeConfig {
            keyword: "underline blue".into(),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.keyword.fg, Some(Color::Blue));
        assert!(theme.keyword.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn backgrounds_are_classified_by_luminance() {
        assert_eq!(scheme_of((0, 0, 0)), Scheme::Dark);
        assert_eq!(scheme_of((30, 30, 46)), Scheme::Dark);
        // Mid grey reads better with light text, so the cut is below halfway.
        assert_eq!(scheme_of((128, 128, 128)), Scheme::Dark);
        assert_eq!(scheme_of((192, 192, 192)), Scheme::Light);
        assert_eq!(scheme_of((255, 255, 255)), Scheme::Light);
        assert!(luminance((0, 0, 0)) < luminance((255, 255, 255)));
    }

    #[test]
    fn mixing_moves_towards_the_other_colour() {
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0.1), (26, 26, 26));
        assert_eq!(mix((255, 255, 255), (0, 0, 0), 0.25), (191, 191, 191));
        assert_eq!(mix((10, 20, 30), (200, 200, 200), 0.0), (10, 20, 30));
        assert_eq!(mix((10, 20, 30), (200, 200, 200), 2.0), (200, 200, 200));
    }

    #[test]
    fn keys_are_coloured_rather_than_left_as_plain_text() {
        // `Hl::Property` being the terminal's default text style made a
        // highlighted key indistinguishable from an unhighlighted one — the
        // JSON and TOML config files that are mostly keys read as one colour.
        let dark = Theme::default_for(Scheme::Dark);
        assert!(dark.property.fg.is_some(), "a key needs a colour");
        assert_ne!(
            dark.property.fg, dark.text.fg,
            "keys must not be plain text"
        );
        // ...and a different one from the string a key is written with, so a
        // pair reads as `"key": "value"` rather than as two strings.
        assert_ne!(dark.property.fg, dark.string.fg);
        let light = Theme::default_for(Scheme::Light);
        assert!(light.property.fg.is_some(), "a key needs a colour");
        // The config can still restyle it like every other element.
        let config = ThemeConfig {
            property: "bold underline green".into(),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.property.fg, Some(Color::Green));
        assert!(theme.property.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_light_scheme_drops_colours_that_vanish_on_white() {
        let dark = Theme::default_for(Scheme::Dark);
        let light = Theme::default_for(Scheme::Light);
        // Yellow type names and cyan comments are dark-background habits.
        assert_eq!(dark.type_name.fg, Some(Color::Yellow));
        assert_eq!(light.type_name.fg, Some(Color::Indexed(3)));
        assert_eq!(light.comment.fg, Some(Color::Indexed(8)));
        assert_ne!(dark.keyword.fg, light.keyword.fg);
        // The focused outline is colourised either way, in a scheme accent.
        assert_eq!(dark.border_focused.fg, Some(Color::Indexed(14)));
        assert_eq!(light.border_focused.fg, Some(Color::Indexed(4)));
        assert_eq!(dark.label_focused.fg, Some(Color::Indexed(14)));
    }

    #[test]
    fn a_measured_background_tints_the_quiet_elements() {
        let observed = Observed {
            background: Some((30, 30, 30)),
            foreground: Some((220, 220, 220)),
        };
        let theme = Theme::with_observed(&ThemeConfig::default(), observed);
        assert_eq!(theme.scheme, Scheme::Dark);
        // A little lighter than the terminal's own background, not a new colour.
        let Some(Color::Rgb(_, _, blue)) = theme.cursor_line.bg else {
            panic!("the cursor line should have a derived background");
        };
        assert!(blue > 30 && blue < 60, "blend was {blue}");
        assert!(theme.gutter.fg.is_some(), "gutter uses the measured ink");
        assert!(theme.border.fg.is_some(), "borders use a dimmed ink");
        // With no measurement, those stay terminal-default (no invented colours).
        let blind = Theme::with_observed(&ThemeConfig::default(), Observed::UNKNOWN);
        assert!(blind.cursor_line.bg.is_none());
        assert!(blind.gutter.fg.is_none());
    }

    #[test]
    fn settings_beat_measurements() {
        let observed = Observed {
            background: Some((245, 245, 245)),
            foreground: Some((20, 20, 20)),
        };
        // The terminal says light; the config says otherwise, and wins.
        let config = ThemeConfig {
            background: Background::Dark,
            ..ThemeConfig::default()
        };
        assert_eq!(Theme::with_observed(&config, observed).scheme, Scheme::Dark);

        // A hand-picked style survives the derived blends.
        let config = ThemeConfig {
            cursor_line: "bold green".into(),
            accent: "#ff8800".into(),
            ..ThemeConfig::default()
        };
        let theme = Theme::with_observed(&config, observed);
        assert_eq!(theme.cursor_line.bg, None);
        assert_eq!(theme.cursor_line.fg, Some(Color::Green));
        assert_eq!(theme.border_focused.fg, Some(Color::Rgb(255, 136, 0)));
    }

    #[test]
    fn a_card_is_chromed_in_the_accent() {
        // A modal is the focused thing on screen, so its outline and title chip
        // mean what the focused pane's border means, and `[theme] accent` moves
        // all three at once.
        let dark = Theme::default_for(Scheme::Dark);
        assert_eq!(dark.overlay_border.fg, Some(dark.accent));
        assert_eq!(dark.overlay_title.fg, Some(dark.accent));
        assert!(dark.overlay_title.bg.is_some(), "the title sits on a chip");

        let configured = Theme::from_config(&ThemeConfig {
            accent: "#ff8800".into(),
            ..ThemeConfig::default()
        });
        assert_eq!(configured.overlay_border.fg, Some(Color::Rgb(255, 136, 0)));
        assert_eq!(configured.picker_cursor.fg, Some(Color::Rgb(255, 136, 0)));
    }

    #[test]
    fn each_picker_family_wears_its_own_hue() {
        let dark = Theme::default_for(Scheme::Dark);
        let hue = |tone| dark.title_style(tone).fg;
        assert_eq!(hue(Tone::Neutral), Some(dark.accent));
        assert_ne!(hue(Tone::File), hue(Tone::Search));
        assert_ne!(hue(Tone::Search), hue(Tone::Location));
        assert_ne!(hue(Tone::Command), hue(Tone::File));
        // A light terminal gets the darker half of the palette, as everywhere.
        let light = Theme::default_for(Scheme::Light);
        assert_ne!(
            light.title_style(Tone::File).fg,
            dark.title_style(Tone::File).fg
        );
        // A hand-written title is one colour for every card, so it is not tinted.
        let locked = Theme::from_config(&ThemeConfig {
            overlay_title: "bold green".into(),
            ..ThemeConfig::default()
        });
        assert_eq!(locked.title_style(Tone::File).fg, Some(Color::Green));
        assert_eq!(locked.title_style(Tone::Search).fg, Some(Color::Green));
    }

    #[test]
    fn a_row_is_coloured_only_when_it_has_something_to_say() {
        let dark = Theme::default_for(Scheme::Dark);
        assert_eq!(dark.tone(Tone::Neutral).fg, None);
        assert_eq!(dark.tone(Tone::Error).fg, Some(Color::Red));
        assert_eq!(dark.tone(Tone::Literal).fg, Some(Color::Green));
        // A severity names its own tone, so the list and the underline it came
        // from say the same thing.
        assert_eq!(Tone::from(Severity::Warning), Tone::Warning);
        assert_eq!(dark.tone(Tone::from(Severity::Hint)).fg, None);
        assert!(
            dark.tone(Tone::Hint).add_modifier.contains(Modifier::DIM),
            "a hint stays the quietest voice on the row"
        );
    }

    #[test]
    fn a_measured_terminal_blends_the_card_and_its_rows() {
        let observed = Observed {
            background: Some((30, 30, 30)),
            foreground: Some((220, 220, 220)),
        };
        let theme = Theme::with_observed(&ThemeConfig::default(), observed);
        let shade = |style: Style| match style.bg {
            Some(Color::Rgb(value, _, _)) => value,
            other => panic!("expected a blended background, got {other:?}"),
        };
        // The card and its title chip are shades off the terminal's own
        // background rather than colours invented for it…
        assert!(shade(theme.overlay) > 30);
        assert!(shade(theme.overlay_title) > shade(theme.overlay));
        // …and the highlighted row is stronger than the buffer's selection, which
        // it must beat without competing with syntax colours.
        assert!(shade(theme.picker_selected) > shade(theme.selection));
        assert!(theme.picker_detail.fg.is_some());
    }
}
