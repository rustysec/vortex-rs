//! TOML configuration (kebab-case keys) with sane defaults.
//!
//! Lookup order: `--config` → `$VORTEX_CONFIG` → `$XDG_CONFIG_HOME/vortex/config.toml`
//! → `~/.config/vortex/config.toml`.  Every key is optional; partial files are
//! merged over the defaults below.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::action::SplitMode;
use crate::error::{Error, Result};

/// Where the status area is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusPosition {
    Top,
    #[default]
    Bottom,
}

/// How segments in the status row are separated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSeparators {
    /// Powerline-style filled segments with angled `\u{e0b0}` separators.
    /// Needs a font with the Powerline glyphs.
    #[default]
    Angled,
    /// Plain space-separated sections — works in any terminal.
    Plain,
}

impl StatusPosition {
    pub fn is_top(self) -> bool {
        self == StatusPosition::Top
    }
}

/// Where the transient notice card — a message, the echo of a partial key
/// sequence, or the search / rename line — floats over the panes.
///
/// Named as a compass: the side of the pane the card hugs, refined by a
/// corner.  `left` and `right` hug the side and sit vertically centred;
/// `top` and `bottom` hug the edge and sit horizontally centred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoticePosition {
    TopLeft,
    Top,
    TopRight,
    Left,
    /// The default: against the bottom of the panes, centred.
    #[default]
    Bottom,
    BottomLeft,
    BottomRight,
    Right,
}

impl NoticePosition {
    pub fn is_top_anchored(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    pub fn is_bottom_anchored(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }

    /// A card anchored to the bottom edge moves to the top of the panes while
    /// a picker is open: the picker card covers the middle and bottom, and a
    /// warning must not be buried under the thing that caused it.  The corner
    /// stays; only the edge flips.
    pub fn avoid_picker(self) -> Self {
        match self {
            Self::Bottom => Self::Top,
            Self::BottomLeft => Self::TopLeft,
            Self::BottomRight => Self::TopRight,
            other => other,
        }
    }

    /// Parse a `:set status.notice-position=…` value.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "top-left" => Self::TopLeft,
            "top" => Self::Top,
            "top-right" => Self::TopRight,
            "left" => Self::Left,
            "bottom" => Self::Bottom,
            "bottom-left" => Self::BottomLeft,
            "bottom-right" => Self::BottomRight,
            "right" => Self::Right,
            _ => return None,
        })
    }

    /// The name `parse` accepts, for `:set` output and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::Top => "top",
            Self::TopRight => "top-right",
            Self::Left => "left",
            Self::Bottom => "bottom",
            Self::BottomLeft => "bottom-left",
            Self::BottomRight => "bottom-right",
            Self::Right => "right",
        }
    }
}

/// Line number gutter style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineNumber {
    Absolute,
    #[default]
    Relative,
    None,
}

/// Border characters used to separate panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BorderStyle {
    /// ASCII `+`, `-`, `|` corners — the safest choice for any terminal.
    #[serde(alias = "square")]
    Plain,
    #[default]
    Rounded,
    Double,
    Thick,
    /// No border at all; panes are separated by a blank column.
    None,
}

impl BorderStyle {
    pub fn symbols(self) -> ratatui::symbols::border::Set<'static> {
        use ratatui::symbols::border;
        match self {
            BorderStyle::Plain => border::PLAIN,
            BorderStyle::Rounded => border::ROUNDED,
            BorderStyle::Double => border::DOUBLE,
            BorderStyle::Thick => border::THICK,
            BorderStyle::None => border::Set {
                top_left: " ",
                top_right: " ",
                bottom_left: " ",
                bottom_right: " ",
                vertical_left: " ",
                vertical_right: " ",
                horizontal_top: " ",
                horizontal_bottom: " ",
            },
        }
    }
}

/// A duration expressed as `400`, `"400ms"`, `"1.5s"`, `"1m"` or `"off"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DurationMs(pub u64);

impl DurationMs {
    pub const DISABLED: u64 = u64::MAX;

    pub const fn disabled() -> Self {
        DurationMs(Self::DISABLED)
    }

    pub fn is_disabled(self) -> bool {
        self.0 == Self::DISABLED
    }

    pub fn as_millis(self) -> u64 {
        self.0
    }

    pub fn to_std(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.0)
    }
}

impl FromStr for DurationMs {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim().to_lowercase();
        match s.as_str() {
            "" => Err("empty duration".into()),
            "off" | "never" | "none" | "disable" | "disabled" => Ok(DurationMs::disabled()),
            _ => {
                let (num, mul) = match s.strip_suffix("ms") {
                    Some(n) => (n, 1),
                    None => match s.strip_suffix('s') {
                        Some(n) => (n, 1_000),
                        None => match s.strip_suffix('m') {
                            Some(n) => (n, 60_000),
                            None => (s.as_str(), 1),
                        },
                    },
                };
                let value: f64 = num.trim().parse().map_err(|_| {
                    format!(
                        "invalid duration '{s}' (expected e.g. 400, \"400ms\", \"1.5s\", \"off\")"
                    )
                })?;
                if value < 0.0 {
                    return Err(format!("duration must not be negative: '{s}'"));
                }
                Ok(DurationMs((value * mul as f64) as u64))
            }
        }
    }
}

impl<'de> Deserialize<'de> for DurationMs {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = DurationMs;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("milliseconds, a duration string like \"400ms\", or \"off\"")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<DurationMs, E> {
                Ok(DurationMs(v))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<DurationMs, E> {
                if v < 0 {
                    return Err(E::custom("duration must not be negative"));
                }
                Ok(DurationMs(v as u64))
            }

            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<DurationMs, E> {
                Ok(if v {
                    DurationMs(500)
                } else {
                    DurationMs::disabled()
                })
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<DurationMs, E> {
                DurationMs::from_str(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EditorConfig {
    /// Spaces inserted for a tab.
    pub tab_width: u16,
    /// Insert spaces instead of a tab character.
    pub expand_tab: bool,
    /// Line number gutter: `"absolute"`, `"relative"` or `"none"`.
    pub line_number: LineNumber,
    /// Highlight the line the cursor sits on.
    pub cursorline: bool,
    /// Rows kept visible above/below the cursor.
    pub scroll_margin: u16,
    /// Columns kept visible to the left/right of the cursor.
    pub sidescroll_margin: u16,
    /// Track the mouse (click to focus, drag to select).
    pub mouse: bool,
    /// Continue indentation when pressing enter.
    pub auto_indent: bool,
    /// Insert the closing half of `()[]{}` and quotes.
    pub auto_match_pairs: bool,
    /// Soft wrap long lines.
    pub wrap: bool,
    /// Copy yanked text to the terminal clipboard via OSC-52.
    pub clipboard: bool,
    /// How many undo steps to remember.
    pub undo_limit: usize,
    /// Idle time after which insert-mode typing becomes one undo step.
    pub undo_group_timeout: DurationMs,
    /// Files larger than this skip background parsing/formatting.
    pub large_file_threshold: usize,
    /// Draw a sign column in the gutter marking lines with diagnostics.
    pub diagnostic_signs: bool,
    /// Draw the diagnostic message dimmed at the end of the line it covers.
    pub inline_diagnostics: bool,
    /// Show the full diagnostic message in a card when the caret rests on one.
    pub diagnostic_float: bool,
    /// Decorate markdown buffers in place: conceal markup markers, substitute
    /// bullets and table borders, style headings.  Changes only how a buffer is
    /// drawn — writing still writes plain markdown.
    pub markdown_render: bool,
    /// Per-language indentation overrides, keyed by language id:
    /// `[editor.indent.go] tab-width = 4`, `expand-tab = false`.
    #[serde(default)]
    pub indent: BTreeMap<String, IndentOverride>,
}

/// Indentation settings for one language, overriding the global ones.
///
/// Both fields are optional so a language can change only the width (YAML wants
/// 2) or only the character (Go wants tabs) without restating the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct IndentOverride {
    pub tab_width: Option<u16>,
    pub expand_tab: Option<bool>,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_width: 4,
            expand_tab: true,
            line_number: LineNumber::Relative,
            cursorline: true,
            scroll_margin: 4,
            sidescroll_margin: 4,
            mouse: true,
            auto_indent: true,
            auto_match_pairs: true,
            wrap: false,
            clipboard: true,
            undo_limit: 1000,
            undo_group_timeout: DurationMs(500),
            large_file_threshold: 2_000_000,
            diagnostic_signs: true,
            inline_diagnostics: false,
            diagnostic_float: true,
            markdown_render: true,
            indent: BTreeMap::new(),
        }
    }
}

impl EditorConfig {
    /// The indentation width for `language`, honouring a per-language override.
    pub fn tab_width_for(&self, language: &str) -> usize {
        self.indent
            .get(language)
            .and_then(|entry| entry.tab_width)
            .unwrap_or(self.tab_width)
            .max(1) as usize
    }

    /// Whether `language` indents with spaces, honouring a per-language override.
    pub fn expand_tab_for(&self, language: &str) -> bool {
        self.indent
            .get(language)
            .and_then(|entry| entry.expand_tab)
            .unwrap_or(self.expand_tab)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct StatusConfig {
    /// `"top"` or `"bottom"`.
    pub position: StatusPosition,
    /// Where the transient notice card floats: `top`, `top-right`, `right`,
    /// `bottom-right`, `bottom`, `bottom-left`, `left` or `top-left`.
    pub notice_position: NoticePosition,
    /// `"angled"` (powerline separators) or `"plain"`.
    pub separators: StatusSeparators,
    pub show_mode: bool,
    /// Show the git branch of the focused pane's file.
    pub show_git_branch: bool,
    /// Show the label (name) of the focused text pane.
    pub show_pane_name: bool,
    /// Show the rollup of lsp diagnostics.
    pub show_diagnostics: bool,
    pub show_position: bool,
    pub show_language: bool,
    /// Show the buffer's text encoding (`utf-8`, or `binary` when the file
    /// held bytes that are not valid UTF-8).
    pub show_encoding: bool,
    /// Show the language server attached to the focused pane's language.
    pub show_lsp: bool,
    pub show_percentage: bool,
    /// Milliseconds before a status message disappears.
    pub message_timeout: DurationMs,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            position: StatusPosition::default(),
            notice_position: NoticePosition::default(),
            separators: StatusSeparators::default(),
            show_mode: true,
            show_git_branch: true,
            show_pane_name: true,
            show_diagnostics: true,
            show_position: true,
            show_language: true,
            show_encoding: true,
            show_lsp: true,
            show_percentage: true,
            message_timeout: DurationMs(6_000),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LayoutConfig {
    /// `"equal"` or `"golden"`.
    pub split_mode: SplitMode,
    /// Pane borders.
    pub border: BorderStyle,
    /// Show the buffer name in the pane border.
    pub show_labels: bool,
    /// Show modified / read-only flags next to the label.
    pub show_flags: bool,
    /// Minimum usable pane height (excludes border).
    pub min_height: u16,
    /// Minimum usable pane width (excludes border).
    pub min_width: u16,
    /// Fraction of the parent given to the focused pane in golden mode.
    pub golden_ratio: f64,
    /// Fraction of the parent given to a new pane in equal mode.
    pub split_ratio: f64,
    /// Relative resize step, as a fraction of the parent.
    pub resize_step: f64,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            split_mode: SplitMode::default(),
            border: BorderStyle::default(),
            show_labels: true,
            show_flags: true,
            min_height: 3,
            min_width: 12,
            golden_ratio: 0.618,
            split_ratio: 0.5,
            resize_step: 0.1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SearchConfig {
    /// Path to ripgrep, or `"auto"` to find it in `$PATH`, or `"off"`.
    pub ripgrep: String,
    /// Search hidden files.
    pub hidden: bool,
    /// Follow symlinks.
    pub follow: bool,
    /// Respect `.gitignore` and friends.
    pub ignore: bool,
    /// Glob patterns removed from search results.
    pub prune: Vec<String>,
    /// Ignore case in patterns (vim's `ignorecase`).
    pub ignore_case: bool,
    /// Uppercase in a pattern makes it case sensitive again.
    pub smart_case: bool,
    /// Cap the number of collected results.
    pub max_results: usize,
    /// Highlight matches of the active search.
    pub highlight: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            ripgrep: "auto".into(),
            hidden: false,
            follow: false,
            ignore: true,
            // Build output is never what a search is looking for; keep this in
            // sync with the `[search] prune` line in config.example.toml.
            prune: ["target", "node_modules", ".git"]
                .iter()
                .map(|pattern| pattern.to_string())
                .collect(),
            ignore_case: false,
            smart_case: true,
            max_results: 5_000,
            highlight: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct FormatConfig {
    /// Format once the buffer has been idle for this long (`off` disables).
    pub on_idle: DurationMs,
    /// Format before writing a file.
    pub on_save: bool,
    /// Command per language: `""` disables, `"!cmd"` runs through the shell.
    #[serde(default)]
    pub command: BTreeMap<String, String>,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            on_idle: DurationMs(500),
            on_save: true,
            command: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LspConfig {
    /// Master switch for language server support.  Off by default: turning it
    /// on implies well-known servers (rust-analyzer, pylsp, gopls, clangd) when
    /// those binaries are installed, which is a heavyweight surprise nobody
    /// asked for.
    pub enable: bool,
    /// Ask the server for completions while typing in insert mode.
    pub completion: bool,
    /// How edits are sent to the server: `"incremental"` (a range per change)
    /// or `"full"` (the whole document every time).
    ///
    /// Incremental is the default because it is what makes editing a large file
    /// bearable — a whole-buffer send on every keystroke is megabytes of JSON
    /// for a one-character edit.  `"full"` is the escape hatch: it is always
    /// correct, so it is the thing to try when diagnostics look like they
    /// describe text that is not there.
    pub sync: LspSync,
    /// Servers keyed by language name.
    #[serde(default)]
    pub servers: BTreeMap<String, LspServer>,
}

/// How `textDocument/didChange` reports edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LspSync {
    /// One ranged change per edit.
    #[default]
    Incremental,
    /// The whole document, every time.
    Full,
}

impl LspSync {
    pub fn is_incremental(self) -> bool {
        self == LspSync::Incremental
    }
}

impl std::str::FromStr for LspSync {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "incremental" => Ok(LspSync::Incremental),
            "full" => Ok(LspSync::Full),
            other => Err(format!(
                "lsp.sync expects incremental or full (got \"{other}\")"
            )),
        }
    }
}

impl std::fmt::Display for LspSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LspSync::Incremental => "incremental",
            LspSync::Full => "full",
        })
    }
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enable: false,
            completion: true,
            sync: LspSync::default(),
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LspServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Restrict a server to specific languages.
    #[serde(default)]
    pub languages: Vec<String>,
    pub enable: bool,
}

impl Default for LspServer {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            languages: Vec::new(),
            enable: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SpellConfig {
    pub enable: bool,
    /// Which checker runs: `"auto"` (an installed aspell or hunspell, else the
    /// embedded list), `"aspell"`, `"hunspell"`, or `"off"`/`"builtin"` for the
    /// embedded list alone.  See [`crate::spell::resolve_engine`].
    pub command: String,
    /// Dictionary an external engine should use: `"auto"` sends no language flag
    /// and lets aspell/hunspell pick their own default; anything else is passed
    /// through (`--lang=` / `-d`).  The embedded list is English either way.
    pub language: String,
    /// Only check comments and strings (uses the tree-sitter tree).
    pub only_comments: bool,
    /// Minimum word length to check.
    pub min_len: usize,
    /// Ignore `CamelCase` / `SCREAMING_CASE` identifiers.
    pub ignore_camel_case: bool,
    /// Match dictionary words case-sensitively.  Off by default, so `The` and
    /// `teh` are compared against the lowercase list; turning it on makes
    /// `Hello` a misspelling while `hello` is not, which is what a document
    /// that cares about capitalisation wants.
    pub case_sensitive: bool,
}

impl Default for SpellConfig {
    fn default() -> Self {
        Self {
            enable: true,
            command: "auto".into(),
            language: "auto".into(),
            only_comments: true,
            min_len: 4,
            ignore_camel_case: true,
            case_sensitive: false,
        }
    }
}

/// How vortex finds out whether the terminal is dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Background {
    /// Ask the terminal for its background colour, assume dark if it says nothing.
    #[default]
    Auto,
    /// Use the dark palette without asking.
    Dark,
    /// Use the light palette without asking.
    Light,
}

impl std::fmt::Display for Background {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
        };
        f.write_str(name)
    }
}

/// Colour overrides.  Values are terminal colour names (`"red"`,
/// `"bright-cyan"`), `"reset"` or `"#rrggbb"`.  The default `""` means
/// "whatever the terminal theme says" — the colourscheme follows the theme.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ThemeConfig {
    /// Whether to ask the terminal how dark it is.
    #[serde(default)]
    pub background: Background,
    /// Colour of the focused pane's border and label (`""` = chosen per scheme).
    #[serde(default)]
    pub accent: String,
    pub mode_normal: String,
    pub mode_insert: String,
    pub mode_visual: String,
    pub mode_command: String,
    pub border: String,
    pub border_focused: String,
    pub label: String,
    pub label_focused: String,
    pub selection: String,
    pub cursor_line: String,
    pub gutter: String,
    pub gutter_active: String,
    pub keyword: String,
    pub string: String,
    pub comment: String,
    pub number: String,
    pub function: String,
    pub type_name: String,
    pub constant: String,
    pub property: String,
    pub operator: String,
    pub punctuation: String,
    pub error: String,
    pub diagnostic_error: String,
    pub diagnostic_warning: String,
    pub diagnostic_hint: String,
    pub diagnostic_info: String,
    pub search_match: String,
    pub prompt: String,
    /// Base fill of the status row in angled mode (`""` = chosen per scheme).
    #[serde(default)]
    pub status_bar: String,
    /// Filled segments (file name, cursor position) in angled mode.
    #[serde(default)]
    pub status_segment: String,
    /// Quieter filled segments (encoding, language …) in angled mode.
    #[serde(default)]
    pub status_soft: String,
    /// Accent-filled chips (git branch, language server) in angled mode.
    #[serde(default)]
    pub status_mark: String,
}

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub editor: EditorConfig,
    pub status: StatusConfig,
    pub layout: LayoutConfig,
    pub search: SearchConfig,
    pub format: FormatConfig,
    pub lsp: LspConfig,
    pub spell: SpellConfig,
    pub theme: ThemeConfig,
    /// `[keymap.normal]`, `[keymap.insert]`, `[keymap.visual]`, `[keymap.command]`
    #[serde(default)]
    pub keymap: BTreeMap<String, BTreeMap<String, String>>,
}

impl Config {
    /// A documented starter configuration, written by `vortex --write-config`.
    pub fn default_config_text() -> &'static str {
        include_str!("../config.example.toml")
    }

    /// Path of the configuration file that would be loaded.
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("VORTEX_CONFIG") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
        dirs::config_dir().map(|dir| dir.join("vortex").join("config.toml"))
    }

    /// Load configuration from disk, falling back to defaults.
    ///
    /// Returns the config plus warnings about unknown or dubious keys — vortex
    /// starts regardless, it just tells you what it ignored.
    pub fn load(path: Option<&Path>) -> Result<(Config, Vec<String>)> {
        let path = match path {
            Some(explicit) => {
                if !explicit.exists() {
                    return Err(Error::Config(format!(
                        "config file {} does not exist",
                        explicit.display()
                    )));
                }
                Some(explicit.to_path_buf())
            }
            None => Self::default_path().filter(|p| p.exists()),
        };

        let mut warnings = Vec::new();
        let config = match &path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                let (config, unknown) = Self::parse(&text)
                    .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                warnings.extend(unknown);
                config
            }
            None => Config::default(),
        };

        warnings.extend(config.validate());
        Ok((config, warnings))
    }

    /// Parse configuration text, also returning warnings about unknown keys.
    pub fn parse(text: &str) -> std::result::Result<(Config, Vec<String>), String> {
        let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut warnings = Vec::new();
        if let toml::Value::Table(table) = &value {
            check_keys(table, &defaults_template(), "", &mut warnings);
        }
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        Ok((config, warnings))
    }

    /// Serialize this configuration back to TOML.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Apply one option change, as typed with `:set name=value`.
    ///
    /// Keeping the write path here means `:set` cannot bypass the validation the
    /// config file goes through.
    pub fn set_value(&mut self, key: &str, value: &str) -> std::result::Result<(), String> {
        /// Parse the handful of shapes a `:set` value can take.
        enum Parsed {
            Bool(bool),
            Num(u64),
            Text(String),
        }

        fn parse(value: &str) -> Parsed {
            let text = value.trim();
            match text {
                "true" | "on" | "yes" | "1" => Parsed::Bool(true),
                "false" | "off" | "no" | "0" => Parsed::Bool(false),
                other => match other.parse::<u64>() {
                    Ok(number) => Parsed::Num(number),
                    Err(_) => Parsed::Text(other.to_string()),
                },
            }
        }

        fn boolean(key: &str, value: &str) -> std::result::Result<bool, String> {
            match parse(value) {
                Parsed::Bool(value) => Ok(value),
                _ => Err(format!("{key} expects true/false")),
            }
        }

        fn number<T: TryFrom<u64>>(key: &str, value: &str) -> std::result::Result<T, String> {
            match parse(value) {
                Parsed::Num(number) => {
                    T::try_from(number).map_err(|_| format!("{key} is out of range: {number}"))
                }
                // `1` and `0` read as booleans above, and are perfectly good
                // values for `:set layout.min-width=1`.
                Parsed::Bool(flag) => T::try_from(u64::from(flag))
                    .map_err(|_| format!("{key} is out of range: {flag}")),
                _ => Err(format!("{key} expects a number")),
            }
        }

        fn text(_key: &str, value: &str) -> std::result::Result<String, String> {
            match parse(value) {
                Parsed::Text(text) => Ok(text),
                Parsed::Bool(value) => Ok(value.to_string()),
                Parsed::Num(value) => Ok(value.to_string()),
            }
        }

        fn duration(key: &str, value: &str) -> std::result::Result<DurationMs, String> {
            DurationMs::from_str(value).map_err(|error| format!("{key}: {error}"))
        }

        match key {
            "editor.tab-width" => self.editor.tab_width = number(key, value)?,
            "editor.expand-tab" => self.editor.expand_tab = boolean(key, value)?,
            "editor.auto-indent" => self.editor.auto_indent = boolean(key, value)?,
            "editor.auto-match-pairs" => self.editor.auto_match_pairs = boolean(key, value)?,
            "theme.background" => {
                self.theme.background = match value.trim().to_ascii_lowercase().as_str() {
                    "auto" => Background::Auto,
                    "dark" => Background::Dark,
                    "light" => Background::Light,
                    other => {
                        return Err(format!(
                            "theme.background expects auto, dark or light (got \"{other}\")"
                        ));
                    }
                }
            }
            "theme.accent" => self.theme.accent = value.trim().to_string(),
            "editor.line-number" => {
                self.editor.line_number = match text(key, value)?.as_str() {
                    "none" => LineNumber::None,
                    "absolute" | "absolute " => LineNumber::Absolute,
                    "relative" => LineNumber::Relative,
                    other => {
                        return Err(format!(
                            "editor.line-number expects none, absolute or relative (got \"{other}\")"
                        ));
                    }
                }
            }
            "editor.cursorline" => self.editor.cursorline = boolean(key, value)?,
            "editor.mouse" => self.editor.mouse = boolean(key, value)?,
            "editor.wrap" => self.editor.wrap = boolean(key, value)?,
            "editor.clipboard" => self.editor.clipboard = boolean(key, value)?,
            "editor.scroll-margin" => self.editor.scroll_margin = number(key, value)?,
            "editor.sidescroll-margin" => self.editor.sidescroll_margin = number(key, value)?,
            "editor.undo-limit" => self.editor.undo_limit = number(key, value)?,
            "editor.undo-group-timeout" => self.editor.undo_group_timeout = duration(key, value)?,
            "editor.large-file-threshold" => self.editor.large_file_threshold = number(key, value)?,
            "editor.diagnostic-signs" => self.editor.diagnostic_signs = boolean(key, value)?,
            "editor.inline-diagnostics" => self.editor.inline_diagnostics = boolean(key, value)?,
            "editor.diagnostic-float" => self.editor.diagnostic_float = boolean(key, value)?,
            "editor.markdown-render" => self.editor.markdown_render = boolean(key, value)?,
            "layout.split-mode" => {
                self.layout.split_mode = SplitMode::from_str(&text(key, value)?)?
            }
            "layout.border" => {
                self.layout.border = match text(key, value)?.as_str() {
                    "rounded" => BorderStyle::Rounded,
                    "plain" | "square" => BorderStyle::Plain,
                    "double" => BorderStyle::Double,
                    "thick" => BorderStyle::Thick,
                    "none" => BorderStyle::None,
                    other => {
                        return Err(format!(
                            "layout.border expects rounded/plain/double/thick/none, got \"{other}\""
                        ));
                    }
                }
            }
            "layout.golden-ratio" => self.layout.golden_ratio = ratio(key, value)?,
            "layout.split-ratio" => self.layout.split_ratio = ratio(key, value)?,
            "layout.resize-step" => self.layout.resize_step = ratio(key, value)?,
            "layout.min-height" => self.layout.min_height = number(key, value)?,
            "layout.min-width" => self.layout.min_width = number(key, value)?,
            "layout.show-labels" => self.layout.show_labels = boolean(key, value)?,
            "layout.show-flags" => self.layout.show_flags = boolean(key, value)?,
            "status.position" => {
                self.status.position = match text(key, value)?.as_str() {
                    "top" => StatusPosition::Top,
                    "bottom" => StatusPosition::Bottom,
                    other => {
                        return Err(format!(
                            "status.position expects top or bottom, got \"{other}\""
                        ));
                    }
                }
            }
            "status.show-mode" => self.status.show_mode = boolean(key, value)?,
            "status.notice-position" => {
                let text = text(key, value)?;
                match NoticePosition::parse(&text) {
                    Some(place) => self.status.notice_position = place,
                    None => {
                        return Err(format!(
                            "status.notice-position expects one of top, top-right, right, \
                             bottom-right, bottom, bottom-left, left, top-left, got \"{text}\""
                        ));
                    }
                }
            }
            "status.separators" => {
                self.status.separators = match text(key, value)?.as_str() {
                    "angled" => StatusSeparators::Angled,
                    "plain" => StatusSeparators::Plain,
                    other => {
                        return Err(format!(
                            "status.separators expects angled or plain, got \"{other}\""
                        ));
                    }
                }
            }
            "status.show-git-branch" => self.status.show_git_branch = boolean(key, value)?,
            "status.show-pane-name" => self.status.show_pane_name = boolean(key, value)?,
            "status.show-diagnostics" => self.status.show_diagnostics = boolean(key, value)?,
            "status.show-position" => self.status.show_position = boolean(key, value)?,
            "status.show-language" => self.status.show_language = boolean(key, value)?,
            "status.show-encoding" => self.status.show_encoding = boolean(key, value)?,
            "status.show-lsp" => self.status.show_lsp = boolean(key, value)?,
            "status.show-percentage" => self.status.show_percentage = boolean(key, value)?,
            "status.message-timeout" => self.status.message_timeout = duration(key, value)?,
            "search.ripgrep" => self.search.ripgrep = text(key, value)?,
            "search.hidden" => self.search.hidden = boolean(key, value)?,
            "search.follow" => self.search.follow = boolean(key, value)?,
            "search.ignore" => self.search.ignore = boolean(key, value)?,
            "search.ignore-case" => self.search.ignore_case = boolean(key, value)?,
            "search.smart-case" => self.search.smart_case = boolean(key, value)?,
            "search.max-results" => self.search.max_results = number(key, value)?,
            "search.highlight" => self.search.highlight = boolean(key, value)?,
            "search.prune" => {
                self.search.prune = text(key, value)?
                    .split(',')
                    .map(str::trim)
                    .filter(|pattern| !pattern.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            "format.on-idle" => self.format.on_idle = duration(key, value)?,
            "format.on-save" => self.format.on_save = boolean(key, value)?,
            "spell.enable" => self.spell.enable = boolean(key, value)?,
            "spell.command" => {
                let requested = text(key, value)?.trim().to_lowercase();
                if !matches!(
                    requested.as_str(),
                    "" | "auto" | "aspell" | "hunspell" | "off" | "none" | "builtin" | "internal"
                ) {
                    return Err(format!(
                        "spell.command expects auto, aspell, hunspell or off, got \"{requested}\""
                    ));
                }
                self.spell.command = requested;
            }
            "spell.language" => self.spell.language = text(key, value)?,
            "spell.only-comments" => self.spell.only_comments = boolean(key, value)?,
            "spell.min-len" => self.spell.min_len = number(key, value)?,
            "spell.ignore-camel-case" => self.spell.ignore_camel_case = boolean(key, value)?,
            "spell.case-sensitive" => self.spell.case_sensitive = boolean(key, value)?,
            "lsp.enable" => self.lsp.enable = boolean(key, value)?,
            "lsp.completion" => self.lsp.completion = boolean(key, value)?,
            "lsp.sync" => self.lsp.sync = text(key, value)?.parse::<LspSync>()?,
            other => return Err(format!("unknown option \"{other}\"")),
        }
        // Range checks `validate()` also performs, so a bad `:set` is refused
        // rather than half applied.
        match key {
            "editor.tab-width" if !(1..=32).contains(&self.editor.tab_width) => {
                return Err("editor.tab-width must be between 1 and 32".into());
            }
            "layout.min-height" if self.layout.min_height < 1 => {
                return Err("layout.min-height must be at least 1".into());
            }
            "layout.min-width" if self.layout.min_width < 4 => {
                return Err("layout.min-width should be at least 4".into());
            }
            _ => {}
        }
        Ok(())
    }

    /// Read an option as text, for `:set name?`.
    pub fn value_of(&self, key: &str) -> Option<String> {
        Some(match key {
            "editor.tab-width" => self.editor.tab_width.to_string(),
            "editor.expand-tab" => on_off(self.editor.expand_tab),
            "editor.auto-indent" => on_off(self.editor.auto_indent),
            "editor.auto-match-pairs" => on_off(self.editor.auto_match_pairs),
            "theme.background" => self.theme.background.to_string(),
            "theme.accent" => self.theme.accent.clone(),
            "editor.line-number" => self.editor.line_number.to_string(),
            "editor.cursorline" => on_off(self.editor.cursorline),
            "editor.mouse" => on_off(self.editor.mouse),
            "editor.wrap" => on_off(self.editor.wrap),
            "editor.clipboard" => on_off(self.editor.clipboard),
            "editor.scroll-margin" => self.editor.scroll_margin.to_string(),
            "editor.sidescroll-margin" => self.editor.sidescroll_margin.to_string(),
            "editor.undo-limit" => self.editor.undo_limit.to_string(),
            "editor.diagnostic-signs" => on_off(self.editor.diagnostic_signs),
            "editor.inline-diagnostics" => on_off(self.editor.inline_diagnostics),
            "editor.diagnostic-float" => on_off(self.editor.diagnostic_float),
            "editor.markdown-render" => on_off(self.editor.markdown_render),
            "layout.split-mode" => split_mode_name(self.layout.split_mode).to_string(),
            "layout.border" => format!("{:?}", self.layout.border).to_lowercase(),
            "layout.golden-ratio" => format!("{:.3}", self.layout.golden_ratio),
            "layout.split-ratio" => format!("{:.2}", self.layout.split_ratio),
            "layout.resize-step" => format!("{:.2}", self.layout.resize_step),
            "layout.min-height" => self.layout.min_height.to_string(),
            "layout.min-width" => self.layout.min_width.to_string(),
            "status.position" => format!("{:?}", self.status.position).to_lowercase(),
            "status.notice-position" => self.status.notice_position.as_str().to_string(),
            "status.show-mode" => on_off(self.status.show_mode),
            "status.separators" => format!("{:?}", self.status.separators).to_lowercase(),
            "status.show-git-branch" => on_off(self.status.show_git_branch),
            "status.show-pane-name" => on_off(self.status.show_pane_name),
            "status.show-diagnostics" => on_off(self.status.show_diagnostics),
            "status.show-position" => on_off(self.status.show_position),
            "status.show-language" => on_off(self.status.show_language),
            "status.show-encoding" => on_off(self.status.show_encoding),
            "status.show-lsp" => on_off(self.status.show_lsp),
            "status.show-percentage" => on_off(self.status.show_percentage),
            "search.ripgrep" => self.search.ripgrep.clone(),
            "search.ignore-case" => on_off(self.search.ignore_case),
            "search.smart-case" => on_off(self.search.smart_case),
            "search.max-results" => self.search.max_results.to_string(),
            "search.highlight" => on_off(self.search.highlight),
            "format.on-idle" => duration_text(self.format.on_idle),
            "status.message-timeout" => duration_text(self.status.message_timeout),
            "format.on-save" => on_off(self.format.on_save),
            "spell.enable" => on_off(self.spell.enable),
            "spell.command" => self.spell.command.clone(),
            "spell.language" => self.spell.language.clone(),
            "spell.only-comments" => on_off(self.spell.only_comments),
            "spell.min-len" => self.spell.min_len.to_string(),
            "spell.ignore-camel-case" => on_off(self.spell.ignore_camel_case),
            "spell.case-sensitive" => on_off(self.spell.case_sensitive),
            "lsp.enable" => on_off(self.lsp.enable),
            "lsp.completion" => on_off(self.lsp.completion),
            "lsp.sync" => self.lsp.sync.to_string(),
            _ => return None,
        })
    }

    /// Semantic checks that serde cannot express.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.editor.tab_width == 0 || self.editor.tab_width > 32 {
            warnings.push("editor.tab-width must be between 1 and 32".into());
        }
        if !(0.05..=0.95).contains(&self.layout.golden_ratio) {
            warnings.push("layout.golden-ratio must be between 0.05 and 0.95".into());
        }
        if !(0.05..=0.95).contains(&self.layout.split_ratio) {
            warnings.push("layout.split-ratio must be between 0.05 and 0.95".into());
        }
        if !(0.01..=0.5).contains(&self.layout.resize_step) {
            warnings.push("layout.resize-step must be between 0.01 and 0.5".into());
        }
        if self.layout.min_width < 4 {
            warnings.push("layout.min-width should be at least 4".into());
        }
        for (section, bindings) in &self.keymap {
            // Every resolution context is a legal section name, `operator`
            // included: `[keymap.operator]` binds the keys an operator waits for
            // its motion, which is a thing of its own even though it is not a mode.
            let Some(context) = crate::input::KeyContext::from_name(section) else {
                warnings.push(format!(
                    "unknown keymap section [keymap.{section}] (expected normal, insert, visual, operator or command)"
                ));
                continue;
            };
            let name = context.name();
            for (keys, action) in bindings {
                if keys.trim().is_empty() {
                    warnings.push(format!("[keymap.{name}] contains an empty key"));
                    continue;
                }
                if crate::input::parse_key_sequence(keys).is_none() {
                    warnings.push(format!(
                        "[keymap.{name}] cannot parse key sequence \"{keys}\""
                    ));
                }
                if crate::action::parse_action(action).is_none() {
                    warnings.push(format!(
                        "[keymap.{name}] \"{keys}\" refers to unknown action \"{action}\""
                    ));
                }
            }
        }
        warnings
    }
}

impl std::fmt::Display for LineNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LineNumber::Absolute => "absolute",
            LineNumber::Relative => "relative",
            LineNumber::None => "none",
        })
    }
}

fn on_off(value: bool) -> String {
    if value { "on".into() } else { "off".into() }
}

fn split_mode_name(mode: SplitMode) -> &'static str {
    mode.name()
}

fn duration_text(duration: DurationMs) -> String {
    if duration.is_disabled() {
        "off".into()
    } else {
        duration.as_millis().to_string()
    }
}

/// Ratios are the one place a `:set` value may be fractional.
fn ratio(key: &str, value: &str) -> std::result::Result<f64, String> {
    let text = value.trim();
    let parsed: f64 = text
        .parse()
        .map_err(|_| format!("{key} expects a number between 0.05 and 0.95"))?;
    if !(0.01..=1.0).contains(&parsed) {
        return Err(format!("{key} must be between 0.01 and 1.0 (got {parsed})"));
    }
    Ok(parsed)
}

/// Deserialize into a `toml::Value` first so unknown keys can be reported, then
/// into the typed config.
fn defaults_template() -> toml::Value {
    // `Value::from_str` parses a TOML *value*, not a document: use the
    // deserializer, or every key in the file looks unknown.
    toml::to_string(&Config::default())
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .unwrap_or(toml::Value::Table(toml::Table::new()))
}

fn check_keys(table: &toml::Table, template: &toml::Value, path: &str, out: &mut Vec<String>) {
    let toml::Value::Table(template) = template else {
        return;
    };
    for (key, value) in table {
        let full = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match template.get(key) {
            None => out.push(format!("unknown key '{full}'")),
            Some(toml::Value::Table(sub)) => {
                // `[keymap.normal]`, `[lsp.servers]` … hold user-chosen names.
                if is_free_form(&full) {
                    continue;
                }
                if let toml::Value::Table(sub_value) = value {
                    check_keys(sub_value, &toml::Value::Table(sub.clone()), &full, out);
                } else {
                    out.push(format!("'{full}' should be a table"));
                }
            }
            Some(_) => {}
        }
    }
}

/// Maps whose keys are user data (mode names, languages, colour names).
fn is_free_form(path: &str) -> bool {
    matches!(
        path,
        "keymap" | "format.command" | "lsp.servers" | "editor.indent"
    ) || path.starts_with("keymap.")
        || path.starts_with("lsp.servers.")
        || path.starts_with("editor.indent.")
}

impl Config {
    /// Drop a binding line from the configuration.
    ///
    /// `context` is the keymap section (`normal`, `insert`, …) and `lhs` the key
    /// sequence exactly as written in the TOML file.
    pub fn remove_binding(&mut self, context: &str, lhs: &str) -> bool {
        let Some(bindings) = self.keymap.get_mut(context) else {
            return false;
        };
        bindings.remove(lhs).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_config_reads_back_clean() {
        // `:config write` output must not come back as a pile of warnings.
        let text = Config::default().to_toml_string();
        let (_config, warnings) = Config::parse(&text).expect("round trips");
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
    }

    #[test]
    fn notice_position_round_trips_through_set() {
        let mut config = Config::default();
        assert_eq!(
            config.value_of("status.notice-position").as_deref(),
            Some("bottom")
        );
        for place in [
            "top",
            "top-right",
            "right",
            "bottom-right",
            "bottom",
            "bottom-left",
            "left",
            "top-left",
        ] {
            config
                .set_value("status.notice-position", place)
                .expect(place);
            assert_eq!(
                config.value_of("status.notice-position").as_deref(),
                Some(place),
                "{place} should read back as written"
            );
        }
        // A nonsense side is refused, naming the accepted ones.
        let error = config
            .set_value("status.notice-position", "sideways")
            .expect_err("not a side");
        assert!(error.contains("top-left"), "{error}");
    }

    #[test]
    fn the_documented_default_reads_back_clean() {
        let (_config, warnings) = Config::parse(Config::default_config_text()).expect("parses");
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
    }

    #[test]
    fn example_config_agrees_with_the_defaults() {
        // config.example.toml is documentation, so every value it spells out has
        // to be the value the code really defaults to.  Compared key by key so
        // the free-form tables (`[format.command]`, `[lsp.servers]`), which the
        // file documents with commented-out examples, are skipped instead of
        // reported as drift — and `[keymap.*]` with them: vortex binds nothing
        // itself, so what the starter spells out is a choice it makes *for* you,
        // not a default it would apply anyway.
        let (_config, warnings) = Config::parse(Config::default_config_text()).expect("parses");
        assert!(warnings.is_empty(), "unknown keys: {warnings:?}");

        // Both sides are read as a Config first, so a documented `"6s"` matches
        // a default of 6000 — the comparison is about values, not spelling.
        let documented = value_of(
            &Config::parse(Config::default_config_text())
                .expect("parses")
                .0
                .to_toml_string(),
        );
        let actual = value_of(&Config::default().to_toml_string());
        assert_eq!(
            documented, actual,
            "config.example.toml disagrees with the code defaults — it is the \
             reference `vortex --default-config` prints, so update it to match"
        );
    }

    /// Parse a serialised config, dropping empty tables so that free-form maps
    /// (`[format.command]`, `[lsp.servers]`) documented with commented-out
    /// examples do not read as drift, and the keymap along with them.
    fn value_of(text: &str) -> toml::Value {
        fn prune(value: toml::Value) -> toml::Value {
            match value {
                toml::Value::Table(mut table) => {
                    // Bindings are the user's own doing; the starter spells them
                    // out, an unconfigured editor has none of them, and neither
                    // is drift.
                    table.remove("keymap");
                    for (_, child) in table.iter_mut() {
                        *child = prune(child.clone());
                    }
                    table
                        .retain(|_, child| !child.as_table().is_some_and(|inner| inner.is_empty()));
                    toml::Value::Table(table)
                }
                other => other,
            }
        }
        prune(toml::from_str(text).expect("serialised config is valid TOML"))
    }

    #[test]
    fn kebab_case_keys_are_the_interface() {
        let (config, warnings) = Config::parse(
            r#"
            [editor]
            tab-width = 7
            expand-tab = false
            line-number = "none"

            [search]
            ignore-case = true
            prune = ["build"]

            [format]
            on-save = false
            "#,
        )
        .expect("parses");
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(config.editor.tab_width, 7);
        assert!(!config.editor.expand_tab);
        assert_eq!(config.editor.line_number, LineNumber::None);
        assert!(config.search.ignore_case);
        assert_eq!(config.search.prune, vec!["build".to_string()]);
        assert!(!config.format.on_save);
    }

    #[test]
    fn unknown_keys_are_reported_rather_than_fatal() {
        let (_config, warnings) =
            Config::parse("[editor]\ntab-width = 4\ntabwidth = 9\n").expect("parses");
        assert_eq!(warnings, vec!["unknown key 'editor.tabwidth'"]);
    }

    #[test]
    fn set_value_and_value_of_agree() {
        let mut config = Config::default();
        config.set_value("editor.tab-width", "3").expect("sets");
        assert_eq!(config.editor.tab_width, 3);
        assert_eq!(config.value_of("editor.tab-width").as_deref(), Some("3"));

        config
            .set_value("editor.expand-tab", "false")
            .expect("sets");
        assert!(!config.editor.expand_tab);
        // Booleans read back the vim way, and both spellings are accepted.
        assert_eq!(config.value_of("editor.expand-tab").as_deref(), Some("off"));
        config.set_value("editor.expand-tab", "on").expect("sets");
        assert!(config.editor.expand_tab);
        config.set_value("editor.expand-tab", "true").expect("sets");
        assert!(config.editor.expand_tab);

        config
            .set_value("editor.line-number", "absolute")
            .expect("sets");
        assert_eq!(config.editor.line_number, LineNumber::Absolute);

        // Numbers that happen to be 0 or 1 are still numbers.
        config.set_value("layout.min-height", "1").expect("sets");
        assert_eq!(config.layout.min_height, 1);
        config.set_value("editor.scroll-margin", "0").expect("sets");
        assert_eq!(config.editor.scroll_margin, 0);
        config.set_value("editor.tab-width", "2").expect("sets");
        assert_eq!(config.editor.tab_width, 2);
        // `=1` / `=0` also switch a boolean on and off, like vim.
        config.set_value("editor.expand-tab", "1").expect("sets");
        assert!(config.editor.expand_tab);
        config.set_value("editor.expand-tab", "0").expect("sets");
        assert!(!config.editor.expand_tab);

        assert!(
            config
                .set_value("editor.tab-width", "not a number")
                .is_err()
        );
        assert!(config.set_value("nope.nope", "1").is_err());
        assert_eq!(config.value_of("nope.nope"), None);
    }

    #[test]
    fn bindings_are_removed_by_context() {
        let mut config = Config::default();
        config.keymap.insert(
            "normal".into(),
            [("gd".to_string(), "command-palette".to_string())].into(),
        );
        assert!(config.remove_binding("normal", "gd"));
        assert!(!config.remove_binding("normal", "gd"), "already gone");
        assert!(!config.remove_binding("insert", "gd"), "wrong mode");
    }

    #[test]
    fn durations_accept_both_spellings() {
        let (config, _) =
            Config::parse("[format]\non-idle = \"2s\"\n[editor]\nundo-group-timeout = 750\n")
                .expect("parses");
        assert_eq!(config.format.on_idle.as_millis(), 2_000);
        assert_eq!(config.editor.undo_group_timeout.as_millis(), 750);
    }

    #[test]
    fn per_language_indent_overrides_the_global_setting() {
        let (config, warnings) = Config::parse(
            "[editor]\ntab-width = 4\nexpand-tab = true\n\
             [editor.indent.go]\ntab-width = 8\nexpand-tab = false\n\
             [editor.indent.yaml]\ntab-width = 2\n",
        )
        .expect("parses");
        assert!(warnings.is_empty(), "{warnings:?}");

        // Go overrides both.
        assert_eq!(config.editor.tab_width_for("go"), 8);
        assert!(!config.editor.expand_tab_for("go"));
        // YAML overrides only the width; the global `expand-tab` still applies.
        assert_eq!(config.editor.tab_width_for("yaml"), 2);
        assert!(config.editor.expand_tab_for("yaml"));
        // A language with no entry falls back to the global settings.
        assert_eq!(config.editor.tab_width_for("rust"), 4);
        assert!(config.editor.expand_tab_for("rust"));
    }

    #[test]
    fn a_zero_width_override_still_indents_by_one() {
        // A bad value must not make `tab` insert nothing.
        let (config, _) = Config::parse("[editor.indent.go]\ntab-width = 0\n").expect("parses");
        assert_eq!(config.editor.tab_width_for("go"), 1);
    }

    #[test]
    fn lsp_sync_defaults_to_incremental_and_can_be_turned_off() {
        // Incremental is the default: a whole-buffer send per keystroke is
        // megabytes of JSON for a one-character edit.
        let config = Config::default();
        assert_eq!(config.lsp.sync, LspSync::Incremental);
        assert!(config.lsp.sync.is_incremental());

        // `full` is the escape hatch when diagnostics look like they describe
        // text that is not there.
        let (config, warnings) = Config::parse("[lsp]\nsync = \"full\"\n").expect("parses");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.lsp.sync, LspSync::Full);
        assert!(!config.lsp.sync.is_incremental());
    }

    #[test]
    fn a_bad_lsp_sync_value_is_refused() {
        assert!(Config::parse("[lsp]\nsync = \"sometimes\"\n").is_err());
        let mut config = Config::default();
        assert!(config.set_value("lsp.sync", "sometimes").is_err());
        config.set_value("lsp.sync", "full").expect("sets");
        assert_eq!(config.value_of("lsp.sync").as_deref(), Some("full"));
    }
}
