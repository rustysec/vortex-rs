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

/// Every style vortex draws with.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Which darkness this palette was built for.
    pub scheme: Scheme,
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
    pub status: Style,
    pub status_inactive: Style,
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
        Self {
            scheme,
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
            status: Style::default().add_modifier(Modifier::BOLD),
            status_inactive: Style::default().add_modifier(Modifier::DIM),
            error: bold(red),
            keyword: bold(magenta),
            string: Style::default().fg(green),
            comment,
            number: Style::default().fg(red),
            constant: Style::default().fg(magenta),
            function: bold(blue),
            type_name: Style::default().fg(yellow),
            property: Style::default(),
            operator: Style::default().add_modifier(Modifier::DIM),
            punctuation: Style::default(),
            tag: bold(green),
            preproc: Style::default().fg(cyan),
            parse_error: underlined(red),
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
        }
        // The focused pane gets the accent colour, unless the user styled the
        // border or the label themselves.
        let accent = parse_color(&config.accent).unwrap_or_else(|| scheme.accent());
        if unset(&config.border_focused) {
            theme.border_focused = bold(accent);
        }
        if unset(&config.label_focused) {
            theme.label_focused = bold(accent);
        }
        theme.scheme = scheme;
        theme.apply_overrides(config);
        theme
    }
}

impl Theme {
    /// Built-in palette with `[theme]` overrides applied on top.
    /// The theme for the `[theme]` entries alone, with nothing measured.
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
}
