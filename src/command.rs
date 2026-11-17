//! The `:` command line: parsing, the command table, completion and `:set`.
//!
//! Commands that map onto an editing action return [`Command::Editor`] and run
//! through the same [`Cmd`] table as keys do, so a binding and a command line can
//! never drift apart.  Everything else is an explicit variant the app knows how
//! to execute.

use std::path::{Path, PathBuf};

use crate::action::{Cmd, ToggleOption};
use crate::diagnostic::DiagnosticScope;
use crate::editor::{Editor, StatusLevel};
use crate::input::keys;
use crossterm::event::KeyEvent;

/// What the app should do with a parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run an ordinary editing command.
    Editor(Cmd),
    /// Open a file in the current pane (`:edit`) or a new pane (`:split`).
    Open {
        path: String,
        split: Option<SplitDirection>,
    },
    /// `:edit` / `:edit!` — reload this buffer from disk.
    Reload { force: bool },
    /// `:enew`
    NewBuffer,
    /// `:write`, with an optional explicit path.
    Write { path: Option<String>, force: bool },
    /// `:write` then `:quit`.
    WriteQuit,
    /// `:wall` — save every buffer.
    WriteAll,
    /// `:buffer <name|number>`
    Buffer { target: String, next: Option<i32> },
    /// `:bdelete[!] <name>`
    BufferDelete { target: String, force: bool },
    /// `:ls`
    BufferList,
    /// `:set …`
    Set { arguments: Vec<String> },
    /// `:map`, `:noremap`
    ListMaps { modes: Vec<String> },
    /// `:unmap <lhs>`
    Unmap { lhs: String, mode: MappingMode },
    /// `:config path|source|write`
    Config(ConfigAction),
    /// `:find [pattern]`
    FindFiles(Option<String>),
    /// `:grep <pattern>`
    Grep(String),
    /// `:cd <dir>` / `:pwd`
    ChangeDir(Option<String>),
    /// `:format`
    Format,
    /// `:highlight clear` / clear the search match list.
    ClearSearchHighlight,
    /// `:N` — jump to a line.
    GotoLine(usize),
    /// Show a message directly.
    Message(StatusLevel, String),
    /// Ask the app to reload configuration from disk.
    ReloadConfig,
    /// Nothing to do.
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Row,
    Column,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    /// Print the config file locations.
    Path,
    /// Re-read the files.
    Source,
    /// Write the effective configuration to disk.
    Write { path: String, merge: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingMode {
    All,
    Normal,
    Insert,
    Visual,
    Command,
}

impl MappingMode {
    pub fn label(self) -> &'static str {
        match self {
            MappingMode::All => "!",
            MappingMode::Normal => "n",
            MappingMode::Insert => "i",
            MappingMode::Visual => "v",
            MappingMode::Command => "c",
        }
    }
}

/// One row of the command table, used by the `:` popup and by completion.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    pub abbreviation: &'static str,
    pub help: &'static str,
    pub usage: &'static str,
    /// What the command takes, in order.  The popup shows these under the help
    /// text and steps through them one at a time once the command is chosen, so
    /// this is the list that makes a parameter discoverable without reading the
    /// reference — an empty slice means the command runs on Enter.
    pub params: &'static [Param],
}

/// One parameter of a `:` command.
#[derive(Debug, Clone, Copy)]
pub struct Param {
    /// Shown in the popup and as the prompt label while it is being typed.
    pub name: &'static str,
    pub help: &'static str,
    pub kind: ParamKind,
    /// Whether the command still means something without it — `:write` saves in
    /// place, so its path is optional and Enter on an empty answer accepts.
    pub optional: bool,
}

/// Where a parameter's candidates come from.
#[derive(Debug, Clone, Copy)]
pub enum ParamKind {
    /// Free text (a search pattern, a key sequence).  Nothing to offer.
    Text,
    /// A file system path, completed from the working directory.
    Path,
    /// One of a fixed set of words.
    Choice(&'static [&'static str]),
    /// The name of an open buffer.
    BufferName,
    /// An option name for `:set`, with its current value shown beside it.
    OptionName,
}

/// Every `:` command.  The `:` popup sorts these alphabetically (see
/// [`listing`]); this table's own order is only how they were written down.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "quit",
        abbreviation: "q",
        help: "close the pane, or quit when it is the last one",
        usage: ":quit[!]",
        params: &[],
    },
    CommandSpec {
        name: "qall",
        abbreviation: "qa",
        help: "quit, refusing if buffers are modified",
        usage: ":qall[!]",
        params: &[],
    },
    CommandSpec {
        name: "write",
        abbreviation: "w",
        help: "save the buffer",
        usage: ":write [path]",
        params: &[Param {
            name: "path",
            help: "file to write; the current file when empty",
            kind: ParamKind::Path,
            optional: true,
        }],
    },
    CommandSpec {
        name: "wall",
        abbreviation: "wa",
        help: "save every modified buffer",
        usage: ":wall",
        params: &[],
    },
    CommandSpec {
        name: "wquit",
        abbreviation: "wq",
        help: "save, then quit",
        usage: ":wquit",
        params: &[],
    },
    CommandSpec {
        name: "edit",
        abbreviation: "e",
        help: "reload this file, or open <path> here (`:edit!` discards changes)",
        usage: ":edit [path]",
        params: &[Param {
            name: "path",
            help: "file to open in this pane; reloads the current one when empty",
            kind: ParamKind::Path,
            optional: true,
        }],
    },
    CommandSpec {
        name: "enew",
        abbreviation: "ene",
        help: "open an empty unnamed buffer",
        usage: ":enew",
        params: &[],
    },
    CommandSpec {
        name: "split",
        abbreviation: "sp",
        help: "split this pane (same file), or open <path> in a new pane",
        usage: ":split [path]",
        params: &[Param {
            name: "path",
            help: "file for the new pane; the current buffer when empty",
            kind: ParamKind::Path,
            optional: true,
        }],
    },
    CommandSpec {
        name: "vsplit",
        abbreviation: "vs",
        help: "split this pane side by side (same file), or open <path>",
        usage: ":vsplit [path]",
        params: &[Param {
            name: "path",
            help: "file for the new pane; the current buffer when empty",
            kind: ParamKind::Path,
            optional: true,
        }],
    },
    CommandSpec {
        name: "only",
        abbreviation: "on",
        help: "keep only the focused pane",
        usage: ":only",
        params: &[],
    },
    CommandSpec {
        name: "buffer",
        abbreviation: "b",
        help: "switch to a buffer",
        usage: ":buffer <name>",
        params: &[Param {
            name: "name",
            help: "buffer to switch to",
            kind: ParamKind::BufferName,
            optional: false,
        }],
    },
    CommandSpec {
        name: "bnext",
        abbreviation: "bn",
        help: "switch to the next buffer",
        usage: ":bnext",
        params: &[],
    },
    CommandSpec {
        name: "bprevious",
        abbreviation: "bp",
        help: "switch to the previous buffer",
        usage: ":bprevious",
        params: &[],
    },
    CommandSpec {
        name: "bdelete",
        abbreviation: "bd",
        help: "close a buffer",
        usage: ":bdelete[!] <name>",
        params: &[Param {
            name: "name",
            help: "buffer to close",
            kind: ParamKind::BufferName,
            optional: false,
        }],
    },
    CommandSpec {
        name: "buffers",
        abbreviation: "ls",
        help: "list buffers",
        usage: ":buffers",
        params: &[],
    },
    CommandSpec {
        name: "set",
        abbreviation: "se",
        help: "show or change options",
        usage: ":set [name[=value]|noname]",
        params: &[Param {
            name: "option",
            help: "option to show or change, as `name=value`",
            kind: ParamKind::OptionName,
            optional: true,
        }],
    },
    CommandSpec {
        name: "map",
        abbreviation: "map",
        help: "show key bindings",
        usage: ":map [<lhs>]",
        params: &[Param {
            name: "lhs",
            help: "key sequence to look up; every binding when empty",
            kind: ParamKind::Text,
            optional: true,
        }],
    },
    CommandSpec {
        name: "unmap",
        abbreviation: "unmap",
        help: "remove a key binding",
        usage: ":unmap <lhs>",
        params: &[Param {
            name: "lhs",
            help: "key sequence to unbind",
            kind: ParamKind::Text,
            optional: false,
        }],
    },
    CommandSpec {
        name: "find",
        abbreviation: "f",
        help: "search file names",
        usage: ":find [pattern]",
        params: &[Param {
            name: "pattern",
            help: "text to look for in file names",
            kind: ParamKind::Text,
            optional: true,
        }],
    },
    CommandSpec {
        name: "grep",
        abbreviation: "gr",
        help: "search file contents",
        usage: ":grep <pattern>",
        params: &[Param {
            name: "pattern",
            help: "regular expression to search the project for",
            kind: ParamKind::Text,
            optional: false,
        }],
    },
    CommandSpec {
        name: "format",
        abbreviation: "fo",
        help: "run the buffer's formatter",
        usage: ":format",
        params: &[],
    },
    CommandSpec {
        name: "diagnostic",
        abbreviation: "diag",
        help: "jump between or list diagnostics in the buffer or the project",
        usage: ":diagnostic [next|prev|list] [buffer|project]",
        params: &[
            Param {
                name: "what",
                help: "jump or list",
                kind: ParamKind::Choice(&["next", "prev", "list"]),
                optional: true,
            },
            Param {
                name: "scope",
                help: "buffer or whole project",
                kind: ParamKind::Choice(&["buffer", "project"]),
                optional: true,
            },
        ],
    },
    CommandSpec {
        name: "config",
        abbreviation: "conf",
        help: "config paths, reload or write",
        usage: ":config path|source|write [path]",
        params: &[
            Param {
                name: "action",
                help: "print paths, reload, or write the file",
                kind: ParamKind::Choice(&["path", "source", "write"]),
                optional: true,
            },
            Param {
                name: "path",
                help: "file to write with `write`",
                kind: ParamKind::Path,
                optional: true,
            },
        ],
    },
    CommandSpec {
        name: "cd",
        abbreviation: "cd",
        help: "change the working directory",
        usage: ":cd <dir>",
        params: &[Param {
            name: "dir",
            help: "directory to work in; the home directory when empty",
            kind: ParamKind::Path,
            optional: false,
        }],
    },
    CommandSpec {
        name: "pwd",
        abbreviation: "pwd",
        help: "print the working directory",
        usage: ":pwd",
        params: &[],
    },
    CommandSpec {
        name: "hlsearch",
        abbreviation: "hl",
        help: "clear search highlighting",
        usage: ":hlsearch clear",
        params: &[Param {
            name: "what",
            help: "only `clear`, which is also the default",
            kind: ParamKind::Choice(&["clear"]),
            optional: true,
        }],
    },
    CommandSpec {
        name: "palette",
        abbreviation: "pal",
        help: "list every command, as `:` does",
        usage: ":palette",
        params: &[],
    },
    CommandSpec {
        name: "help",
        abbreviation: "h",
        help: "list commands",
        usage: ":help",
        params: &[],
    },
];

/// Parse a command line.
///
/// Vim allows unambiguous abbreviations (`:w`, `:qall`), so lookup is by prefix;
/// a `:number` style line of digits means "go to line".
pub fn parse(input: &str) -> Command {
    let input = input.trim();
    if input.is_empty() {
        return Command::Noop;
    }
    // `:123` jumps to a line.
    // `:123` jumps to a line, `:%` to the end of the buffer.
    if input == "%" {
        return Command::GotoLine(usize::MAX);
    }
    if let Ok(number) = input.parse::<usize>() {
        return Command::GotoLine(number);
    }
    let (head_key, rest_raw) = split_first(input);
    // `:q!`, `:w!`, `:bd!`: the bang belongs to the command, not its argument.
    let bang = match head_key.strip_suffix('!') {
        Some(bare) if rest_raw.trim().is_empty() && command_for(bare).is_some() => {
            Some(String::from("!"))
        }
        _ => None,
    };
    let head = if bang.is_some() {
        head_key.trim_end_matches('!')
    } else {
        head_key
    };
    let rest: &str = match &bang {
        Some(marker) => marker.as_str(),
        None => rest_raw.trim(),
    };

    // `:number`, `:spell`, `:wrap` … behave like their status-bar toggles.
    if rest.is_empty() {
        if let Some(toggle) =
            toggle_for(head).or_else(|| head.strip_prefix("no").and_then(toggle_for))
        {
            return Command::Editor(Cmd::ToggleOption(toggle));
        }
    }

    match command_for(head) {
        Some("quit") => Command::Editor(if rest == "!" {
            Cmd::ForceQuit
        } else {
            Cmd::Quit
        }),
        Some("qall") => Command::Editor(if rest == "!" {
            Cmd::ForceQuitAll
        } else {
            Cmd::QuitAll
        }),
        Some("write") => match rest {
            "" => Command::Write {
                path: None,
                force: false,
            },
            "!" => Command::Write {
                path: None,
                force: true,
            },
            path => Command::Write {
                path: Some(expand_path(path)),
                force: path.ends_with('!'),
            },
        },
        Some("wall") => Command::WriteAll,
        Some("wquit") => Command::WriteQuit,
        Some("edit") => match rest {
            // `:edit` reloads the file, `:edit!` discards unsaved changes.
            "" => Command::Reload { force: false },
            "!" => Command::Reload { force: true },
            path => Command::Open {
                path: expand_path(path),
                split: None,
            },
        },
        Some("enew") => Command::NewBuffer,
        // No argument splits the window with the buffer that is already open,
        // as in vim; a path opens that file in the new pane instead.
        Some("split") if rest.is_empty() => Command::Editor(Cmd::SplitHorizontal),
        Some("vsplit") if rest.is_empty() => Command::Editor(Cmd::SplitVertical),
        Some("split") => Command::Open {
            path: expand_path(rest),
            split: Some(SplitDirection::Row),
        },
        Some("vsplit") => Command::Open {
            path: expand_path(rest),
            split: Some(SplitDirection::Column),
        },
        Some("only") => Command::Editor(Cmd::OnlyPane),
        Some("buffer") => Command::Buffer {
            target: rest.to_string(),
            next: None,
        },
        Some("bnext") => Command::Buffer {
            target: rest.to_string(),
            next: Some(1),
        },
        Some("bprevious") => Command::Buffer {
            target: rest.to_string(),
            next: Some(-1),
        },
        Some("bdelete") => Command::BufferDelete {
            target: rest.trim_end_matches('!').to_string(),
            force: rest.ends_with('!'),
        },
        Some("buffers") => Command::BufferList,
        Some("set") | Some("setlocal") => Command::Set {
            arguments: split_arguments(rest),
        },
        Some("map") | Some("noremap") => Command::ListMaps {
            modes: vec![mode_of(head).label().to_string()],
        },
        Some("unmap") => Command::Unmap {
            lhs: unescape_lhs(rest),
            mode: mode_of(head),
        },
        Some("find") => Command::FindFiles(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        // `:grep` with no pattern opens the content prompt (like `space s`),
        // not the file picker; `:find` is the file-name search.
        Some("grep") => Command::Grep(rest.to_string()),
        Some("diagnostic") => parse_diagnostic(rest),
        Some("format") => Command::Format,
        Some("config") => match rest.split_whitespace().next() {
            None | Some("path") => Command::Config(ConfigAction::Path),
            Some("source") => Command::ReloadConfig,
            Some("write") => {
                let mut parts = rest.split_whitespace().skip(1);
                let path = parts.next().map(expand_path).unwrap_or_else(|| {
                    crate::config::Config::default_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "config.toml".into())
                });
                Command::Config(ConfigAction::Write {
                    path,
                    merge: parts.any(|arg| arg == "--merge"),
                })
            }
            Some(other) => Command::Message(
                StatusLevel::Error,
                format!("unknown :config subcommand \"{other}\" (path, source, write)"),
            ),
        },
        Some("cd") => Command::ChangeDir(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        // `:pwd` reports the directory without changing it; an empty path would
        // canonicalize to nothing and fail, so it asks for the report instead.
        Some("pwd") => Command::ChangeDir(None),
        Some("hlsearch") => {
            if rest.is_empty() || rest == "clear" {
                Command::ClearSearchHighlight
            } else {
                Command::Message(StatusLevel::Error, "usage: :hlsearch clear".into())
            }
        }
        Some("help") | Some("palette") => Command::Editor(Cmd::CommandPalette),
        Some(_) => Command::Noop, // unreachable: command_for returns a known name
        None => match action_command(head) {
            // Every bindable action is also a `:` command, so the two lists
            // can never drift: if `vortex --list-actions` names it, `:name` runs it.
            Some(cmd) => Command::Editor(cmd),
            None => Command::Message(
                StatusLevel::Error,
                format!("not an editor command: :{head}  (:help lists them)"),
            ),
        },
    }
}

/// `:diagnostic [next|prev|list] [buffer|project]` — the diagnostic verbs.
///
/// The scope argument defaults to the current buffer; `project` widens the
/// jump walk and the picker to every buffer the editor holds.
fn parse_diagnostic(rest: &str) -> Command {
    let mut words = rest.split_whitespace();
    let verb = words.next().unwrap_or("list");
    let scope = match words.next() {
        None => DiagnosticScope::Buffer,
        Some(word) => match DiagnosticScope::parse(word) {
            Some(scope) => scope,
            None => {
                return Command::Message(
                    StatusLevel::Error,
                    format!("unknown diagnostic scope \"{word}\" (buffer, project)"),
                );
            }
        },
    };
    if words.next().is_some() {
        return Command::Message(
            StatusLevel::Error,
            "usage: :diagnostic [next|prev|list] [buffer|project]".into(),
        );
    }
    match verb {
        "list" | "ls" => Command::Editor(Cmd::ShowDiagnostics(scope)),
        "next" | "n" => Command::Editor(Cmd::DiagnosticNext(scope)),
        "prev" | "previous" | "p" => Command::Editor(Cmd::DiagnosticPrev(scope)),
        other => Command::Message(
            StatusLevel::Error,
            format!("unknown :diagnostic verb \"{other}\" (list, next, prev)"),
        ),
    }
}

/// Resolve an action name (`close-pane`, `recenter`, …) to its command.
///
/// Only self-contained commands qualify; motions need the key-handling state
/// machine and make no sense as a one-shot command line.
fn action_command(name: &str) -> Option<Cmd> {
    match crate::action::parse_action(name) {
        Some(crate::action::Action::Cmd(cmd)) => Some(cmd),
        _ => None,
    }
}

/// Resolve a command name, allowing unambiguous abbreviations.
fn command_for(name: &str) -> Option<&'static str> {
    if let Some(spec) = COMMANDS.iter().find(|spec| spec.name == name) {
        return Some(spec.name);
    }
    if let Some(spec) = COMMANDS.iter().find(|spec| spec.abbreviation == name) {
        return Some(spec.name);
    }
    // Vim-style prefix matching, but only when it is unambiguous.  A prefix that
    // could mean `set` or `split` is reported instead of guessed.
    let mut matches = COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(name) && name.len() >= 2);
    let first = matches.next()?;
    // Guessing between `set` and `split` would surprise someone; say so instead.
    if matches.next().is_some() {
        return None;
    }
    Some(first.name)
}

/// True when `name` resolves to a `:` command (exact, abbreviation or
/// unambiguous prefix), so keymaps can bind `"bnext"` as well as `":bnext"`.
pub fn is_command(name: &str) -> bool {
    !name.is_empty() && command_for(name).is_some()
}

fn toggle_for(name: &str) -> Option<ToggleOption> {
    Some(match name {
        "number" | "nu" => ToggleOption::LineNumbers,
        "cursorline" | "cul" => ToggleOption::CursorLine,
        "mouse" => ToggleOption::Mouse,
        "wrap" | "wr" => ToggleOption::Wrap,
        "spell" => ToggleOption::SpellCheck,
        "markdown" | "md" => ToggleOption::Markdown,
        "autoformat" => ToggleOption::AutoFormat,
        _ => return None,
    })
}

fn mode_of(name: &str) -> MappingMode {
    match name {
        "nmap" | "noremap" => MappingMode::Normal,
        "imap" | "inoremap" => MappingMode::Insert,
        "vmap" | "vnoremap" => MappingMode::Visual,
        "cmap" | "cnoremap" => MappingMode::Command,
        _ => MappingMode::All,
    }
}

fn split_first(input: &str) -> (&str, &str) {
    match input.find(char::is_whitespace) {
        Some(index) => (&input[..index], &input[index..]),
        None => (input, ""),
    }
}

/// Split arguments, keeping `<…>` key notations and quoted strings whole.
fn split_arguments(input: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(marker) => {
                if character == marker {
                    quote = None;
                } else {
                    current.push(character);
                }
            }
            None => match character {
                '"' | '\'' => quote = Some(character),
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        arguments.push(std::mem::take(&mut current));
                    }
                }
                '<' => {
                    // Keep key notations such as `<C-w>` as one argument.
                    current.push('<');
                    for inner in characters.by_ref() {
                        current.push(inner);
                        if inner == '>' {
                            break;
                        }
                    }
                }
                other => current.push(other),
            },
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

fn unescape_lhs(input: &str) -> String {
    input.trim().to_string()
}

/// Parse and render a key sequence for `:unmap`.
pub fn parse_keys(sequence: &str) -> Option<Vec<KeyEvent>> {
    keys::parse_key_sequence(sequence)
}

/// `~` and `$HOME` expansion for command-line paths.
fn expand_path(input: &str) -> String {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home
                .join(rest.trim_start_matches('/'))
                .display()
                .to_string();
        }
    }
    input.to_string()
}

/// Apply `:set` arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum SetOutcome {
    /// The editor should apply a toggle.
    Toggle(ToggleOption),
    /// A configuration value changed; the app persists it and rebuilds keymaps.
    Changed { key: String, value: String },
    /// Print a value.
    Show(String),
    /// Print every option with its effective value.
    Summary(String),
    /// Something was wrong with the argument.
    Bad(String),
}

/// Interpret `:set` arguments without touching disk.
pub fn parse_set(arguments: &[String], editor: &Editor) -> Vec<SetOutcome> {
    if arguments.is_empty() {
        return vec![SetOutcome::Summary(option_summary(editor))];
    }
    arguments
        .iter()
        .map(|argument| parse_set_one(argument, editor))
        .collect()
}

fn parse_set_one(argument: &str, editor: &Editor) -> SetOutcome {
    // `name=value`, `name?` (show), `name`, `noname`
    if let Some((name, value)) = argument.split_once('=') {
        let key =
            canonical_option(name).or_else(|| assignment_for(name).map(|(key, _)| key.to_string()));
        return match key {
            Some(key) => SetOutcome::Changed {
                key: key.to_string(),
                value: value.to_string(),
            },
            None => SetOutcome::Bad(format!("unknown option \"{name}\"")),
        };
    }
    if let Some(name) = argument.strip_suffix('?') {
        let Some(key) = canonical_option(name) else {
            return SetOutcome::Bad(format!("unknown option \"{name}\""));
        };
        return SetOutcome::Show(format!(
            "{key}={}",
            editor.config.value_of(&key).unwrap_or_default()
        ));
    }
    // Explicit spellings win: `:set number`, `:set norelativenumber`, …
    if let Some((key, value)) = assignment_for(argument) {
        return SetOutcome::Changed {
            key: key.to_string(),
            value: value.to_string(),
        };
    }
    if let Some(option) = toggle_for(argument) {
        return SetOutcome::Toggle(option);
    }
    // Generic boolean options: `:set expandtab` / `:set noexpandtab`.
    if let Some(key) = canonical_option(argument)
        && BOOLEAN_OPTIONS.contains(&key.as_str())
    {
        return SetOutcome::Changed {
            key,
            value: "true".into(),
        };
    }
    if let Some(stripped) = argument.strip_prefix("no")
        && let Some(key) = canonical_option(stripped)
        && BOOLEAN_OPTIONS.contains(&key.as_str())
    {
        return SetOutcome::Changed {
            key,
            value: "false".into(),
        };
    }
    match canonical_option(argument) {
        Some(key) => SetOutcome::Show(format!(
            "{key}={}",
            editor.config.value_of(&key).unwrap_or_default()
        )),
        None => SetOutcome::Bad(format!("unknown option \"{argument}\"")),
    }
}

/// The options `:set` lists and completes, in the order the listing shows them.
pub const OPTION_KEYS: &[&str] = &[
    "editor.tab-width",
    "editor.expand-tab",
    "editor.auto-indent",
    "editor.line-number",
    "editor.cursorline",
    "editor.wrap",
    "editor.mouse",
    "editor.scroll-margin",
    "layout.split-mode",
    "layout.split-ratio",
    "layout.min-width",
    "layout.min-height",
    "status.position",
    "status.notice-position",
    "spell.enable",
    "spell.only-comments",
    "spell.case-sensitive",
    "search.ignore-case",
    "search.smart-case",
    "search.ripgrep",
    "format.on-idle",
    "lsp.enable",
    "lsp.completion",
];

/// The `:set` listing: every option the user is likely to change.
pub fn option_summary(editor: &Editor) -> String {
    OPTION_KEYS
        .iter()
        .filter_map(|key| {
            editor
                .config
                .value_of(key)
                .map(|value| format!("{key}={value}"))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Resolve the option name a user typed in `:set` to a config key.
///
/// Three spellings work: vim's (`:set so=4`), the config file's own path
/// (`:set editor.scroll-margin=4`) and the bare key exactly as
/// `config.example.toml` documents it (`:set scroll-margin=4`).  A bare tail
/// that could belong to two sections is refused rather than guessed.
fn canonical_option(name: &str) -> Option<String> {
    if let Some(key) = vim_option(name) {
        return Some(key.to_string());
    }
    let keys = config_keys();
    if keys.iter().any(|key| key == name) {
        return Some(name.to_string());
    }
    let mut found = keys
        .iter()
        .filter(|key| key.rsplit('.').next() == Some(name));
    let first = found.next()?.clone();
    found.next().is_none().then_some(first)
}

/// Every `section.key` the config exposes, read off the default config so the
/// list cannot drift from the struct.
fn config_keys() -> &'static [String] {
    static KEYS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    KEYS.get_or_init(|| {
        let text = crate::config::Config::default().to_toml_string();
        // A serialised document has to be deserialized; `str::parse` expects a
        // single TOML value and fails on a whole file.
        let Ok(toml::Value::Table(root)) = toml::from_str::<toml::Value>(&text) else {
            return Vec::new();
        };
        let mut keys = Vec::new();
        for (section, value) in root {
            if let toml::Value::Table(entries) = value {
                keys.extend(entries.keys().map(|key| format!("{section}.{key}")));
            }
        }
        keys.sort();
        keys
    })
}

/// Vim's own option names mapped onto `[section] key` paths.
fn vim_option(name: &str) -> Option<&'static str> {
    Some(match name {
        "tab-width" | "ts" | "tabstop" => "editor.tab-width",
        "expandtab" | "et" => "editor.expand-tab",
        "autoindent" | "ai" => "editor.auto-indent",
        "auto-match-pairs" => "editor.auto-match-pairs",
        "number" | "nu" => "editor.line-number",
        "relativenumber" | "rnu" => "editor.line-number",
        "cursorline" | "cul" => "editor.cursorline",
        "mouse" => "editor.mouse",
        "wrap" | "wr" => "editor.wrap",
        "clipboard" | "cb" => "editor.clipboard",
        "scroll" | "so" => "editor.scroll-margin",
        "sidescroll" | "siso" => "editor.sidescroll-margin",
        "undo-limit" | "ul" => "editor.undo-limit",
        "split-mode" => "layout.split-mode",
        "border" => "layout.border",
        "golden-ratio" => "layout.golden-ratio",
        "split-ratio" => "layout.split-ratio",
        "resize-step" => "layout.resize-step",
        "min-height" => "layout.min-height",
        "min-width" => "layout.min-width",
        "status-position" => "status.position",
        "spell" => "spell.enable",
        "spell-command" => "spell.command",
        "spell-language" => "spell.language",
        "only-comments" => "spell.only-comments",
        "min-len" => "spell.min-len",
        "ignore-camel-case" => "spell.ignore-camel-case",
        "case-sensitive" | "spell-case-sensitive" => "spell.case-sensitive",
        "ignorecase" | "ic" => "search.ignore-case",
        "smartcase" | "scs" => "search.smart-case",
        "hlsearch" | "hls" => "search.highlight",
        "max-results" => "search.max-results",
        "ripgrep" | "rg" => "search.ripgrep",
        "hidden" => "search.hidden",
        "follow" => "search.follow",
        "autoformat" => "format.on-idle",
        "format-on-save" => "format.on-save",
        "lsp" => "lsp.enable",
        _ => return None,
    })
}

/// Options whose value is a boolean, so `:set name` / `:set noname` work.
const BOOLEAN_OPTIONS: &[&str] = &[
    "editor.expand-tab",
    "editor.auto-indent",
    "editor.auto-match-pairs",
    "editor.cursorline",
    "editor.mouse",
    "editor.wrap",
    "editor.clipboard",
    "spell.enable",
    "spell.only-comments",
    "spell.ignore-camel-case",
    "spell.case-sensitive",
    "search.ignore-case",
    "search.smart-case",
    "search.highlight",
    "search.hidden",
    "search.follow",
    "search.ignore",
    "format.on-save",
    "lsp.enable",
    "lsp.completion",
];

/// Vim spellings that mean a specific value rather than a toggle.
fn assignment_for(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "number" | "nu" => ("editor.line-number", "absolute"),
        "nonumber" | "nonu" => ("editor.line-number", "none"),
        "relativenumber" | "rnu" => ("editor.line-number", "relative"),
        "norelativenumber" | "nornu" => ("editor.line-number", "absolute"),
        "expandtab" | "et" => ("editor.expand-tab", "true"),
        "noexpandtab" | "noet" => ("editor.expand-tab", "false"),
        "smartcase" | "scs" => ("search.smart-case", "true"),
        "nosmartcase" | "noscs" => ("search.smart-case", "false"),
        // `autoformat` is the idle timer: on means 500 ms, off means never.
        "autoformat" => ("format.on-idle", "500ms"),
        "noautoformat" => ("format.on-idle", "off"),
        _ => return None,
    })
}

/// Path completion for the command line.
pub fn complete_path(fragment: &str) -> Vec<PathBuf> {
    let expanded = expand_path(fragment);
    let path = Path::new(&expanded);
    let trailing = expanded.ends_with(std::path::MAIN_SEPARATOR_STR);
    let (directory, prefix) = if trailing {
        (path.to_path_buf(), String::new())
    } else {
        (
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut completions: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !prefix.is_empty() && !name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                return None;
            }
            let is_dir = entry.path().is_dir();
            let mut display = if trailing {
                expanded.clone()
            } else {
                expanded[..expanded.len().saturating_sub(prefix.len())].to_string()
            };
            if !display.ends_with(std::path::MAIN_SEPARATOR_STR) && !display.is_empty() {
                display.push(std::path::MAIN_SEPARATOR);
            }
            display.push_str(&name);
            if is_dir {
                display.push(std::path::MAIN_SEPARATOR);
            }
            Some(PathBuf::from(display))
        })
        .collect();
    completions.sort();
    completions
}

/// Complete `:command` names (only the command word itself).
pub fn complete_command(input: &str) -> Vec<String> {
    if input.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(input) || spec.abbreviation.starts_with(input))
        .map(|spec| format!("{} ", spec.name))
        .collect()
}

/// The argument part of a usage string: `":write [path]"` yields `"[path]"`, and
/// `":quit[!]"` — which has no space to split on — yields `"[!]"`.
///
/// The popup prints this dimmed after the command name, so the shape of a command
/// is visible without anyone having to open it first.  Everything past the first
/// space is an argument; a usage with no space at all can still carry a bracketed
/// suffix, which is vim's notation for "and a bang".
pub fn arguments(usage: &'static str) -> &'static str {
    let body = usage.strip_prefix(':').unwrap_or(usage);
    if let Some(at) = body.find(' ') {
        return body[at + 1..].trim();
    }
    match body.find(['[', '<']) {
        Some(at) => &body[at..],
        None => "",
    }
}

/// The parameter lines shown in a command's preview, one per parameter.
///
/// Reads like `path — file to write; the current file when empty (optional)`, so
/// the shape of an unfamiliar command is on screen before anyone commits to it.
pub fn param_lines(params: &[Param]) -> String {
    let mut lines = String::new();
    for param in params {
        let requirement = if param.optional {
            "optional"
        } else {
            "required"
        };
        let choices = match param.kind {
            ParamKind::Choice(words) => format!(" [{}]", words.join(" | ")),
            _ => String::new(),
        };
        lines.push_str(&format!(
            "  {} — {}{} ({requirement})\n",
            param.name, param.help, choices
        ));
    }
    lines
}

/// Every command in the order `:` lists them: alphabetically.
///
/// The popup is a lookup, not a tour of the table — publication order made
/// someone hunt for `:write` among the quit-and-save neighbours — so it sorts by
/// name and lets the fuzzy filter do the narrowing.  Abbreviations are listed too
/// in each row's preview rather than as rows of their own, which would double the
/// list to no purpose.
pub fn listing() -> Vec<&'static CommandSpec> {
    let mut specs: Vec<&'static CommandSpec> = COMMANDS.iter().collect();
    specs.sort_by_key(|spec| spec.name);
    specs
}

/// The bindable actions that run as one-shot commands but have no row of their
/// own in [`listing`]: `(name, help)` pairs.
///
/// `parse` accepts every action name whose effect is self-contained — the
/// fallback through [`action_command`] means `:focus-next` runs today even
/// though no spec names it — so a popup built from COMMANDS alone hides half
/// the editor's commands.  Listing what actually parses keeps the palette and
/// the keymap in step: anything someone can bind, they can find.  Names an ex
/// spec already answers to (`quit`, exact or abbreviation) are skipped, since
/// that row runs the same thing.
pub fn unlisted_commands() -> Vec<(&'static str, &'static str)> {
    crate::action::ACTIONS
        .iter()
        .filter(|(_, action, _)| matches!(action, crate::action::Action::Cmd(_)))
        .map(|(name, _, help)| (*name, *help))
        .filter(|(name, _)| command_for(name).is_none())
        .collect()
}

/// The command a typed word names — exact, abbreviation or unambiguous prefix —
/// as its full name.
///
/// `:w` and `:wri` both mean `write`.  The popup needs this to tell a line that
/// *names* a command from one that merely resembles several: only in the second
/// case does it fall back to the highlighted row, because someone who typed a
/// command word said which one they meant.
pub fn resolve(name: &str) -> Option<&str> {
    match command_for(name) {
        Some(resolved) => Some(resolved),
        // An exact action name is a typed word that names a command too —
        // `:focus-next` means itself, not whatever row the filter left on top.
        // (An empty word resolves to nothing; `parse_action` reads it as Noop.)
        None if !name.is_empty() && action_command(name).is_some() => Some(name),
        None => None,
    }
}

/// The parameters of the command `name` resolves to, abbreviations included.
pub fn params_for(name: &str) -> &'static [Param] {
    match command_for(name) {
        Some(resolved) => COMMANDS
            .iter()
            .find(|spec| spec.name == resolved)
            .map(|spec| spec.params)
            .unwrap_or(&[]),
        None => &[],
    }
}

/// The option names `:set` accepts, for the popup's parameter stage.
pub fn option_keys() -> &'static [&'static str] {
    OPTION_KEYS
}

/// Candidates a parameter can offer while it is being typed.
///
/// `Text` has none — there is nothing to suggest before someone has written a
/// search pattern — and the kinds that depend on editor state (open buffers) are
/// filled in by the caller, which can see them.  Paths come from the file system
/// and choices from the command table itself, so they belong here.
pub fn param_candidates(param: &Param, fragment: &str) -> Vec<String> {
    match param.kind {
        ParamKind::Text => Vec::new(),
        ParamKind::Path => complete_path(fragment)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
        ParamKind::Choice(words) => words
            .iter()
            .filter(|word| word.starts_with(fragment))
            .map(|word| (*word).to_string())
            .collect(),
        ParamKind::OptionName => OPTION_KEYS
            .iter()
            .filter(|key| key.contains(fragment.rsplit('.').next().unwrap_or("")))
            .map(|key| (*key).to_string())
            .collect(),
        // Buffer names belong to the editor, which is the caller that can see them.
        ParamKind::BufferName => Vec::new(),
    }
}

/// True when a typed `:` line carries its own arguments — `:write notes.txt`,
/// `:set tab-width=4`, `:q!`.
///
/// The popup offers commands while you type, but a line like this has already
/// been written by someone who knows what they want: Enter runs exactly what is
/// on the line rather than whatever row happens to be highlighted.
pub fn has_arguments(line: &str) -> bool {
    line.contains(char::is_whitespace) || line.ends_with('!')
}

/// Rebuild a command line from a chosen command and its collected parameters.
pub fn assemble(head: &str, args: &[String]) -> String {
    if args.is_empty() {
        return head.to_string();
    }
    format!("{head} {}", args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::default()
    }

    #[test]
    fn parses_exact_and_abbreviated_commands() {
        assert_eq!(parse("quit"), Command::Editor(Cmd::Quit));
        assert_eq!(parse("q"), Command::Editor(Cmd::Quit));
        assert_eq!(parse("q!"), Command::Editor(Cmd::ForceQuit));
        assert_eq!(parse("qa!"), Command::Editor(Cmd::ForceQuitAll));
        assert_eq!(parse("only"), Command::Editor(Cmd::OnlyPane));
        assert_eq!(
            parse("w note.md"),
            Command::Write {
                path: Some("note.md".into()),
                force: false
            }
        );
        assert_eq!(parse("enew"), Command::NewBuffer);
        // `:grep` always means contents, with or without a pattern; only
        // `:find` searches file names.
        assert_eq!(parse("grep"), Command::Grep(String::new()));
        assert_eq!(parse("grep todo"), Command::Grep("todo".into()));
        assert_eq!(parse("gr todo"), Command::Grep("todo".into()));
        assert_eq!(parse("find"), Command::FindFiles(None));
        assert_eq!(parse("find rs"), Command::FindFiles(Some("rs".into())));
        assert_eq!(parse("%"), Command::GotoLine(usize::MAX));
        // `:e` reloads; `:e!` reloads and discards; `:e path` opens.
        assert_eq!(parse("e"), Command::Reload { force: false });
        assert_eq!(parse("edit!"), Command::Reload { force: true });
        assert_eq!(
            parse("e notes.txt"),
            Command::Open {
                path: "notes.txt".into(),
                split: None
            }
        );
        // `:palette` opens the command palette, the same list `Space p` shows.
        assert_eq!(parse("palette"), Command::Editor(Cmd::CommandPalette));
        assert_eq!(parse("pal"), Command::Editor(Cmd::CommandPalette));
        assert_eq!(parse("help"), Command::Editor(Cmd::CommandPalette));
    }

    #[test]
    fn diagnostic_commands_take_a_verb_and_a_scope() {
        // Bare `:diagnostic` lists this buffer, the way `space d` does.
        assert_eq!(
            parse("diagnostic"),
            Command::Editor(Cmd::ShowDiagnostics(DiagnosticScope::Buffer))
        );
        assert_eq!(
            parse("diag list project"),
            Command::Editor(Cmd::ShowDiagnostics(DiagnosticScope::Project))
        );
        assert_eq!(
            parse("diagnostic next project"),
            Command::Editor(Cmd::DiagnosticNext(DiagnosticScope::Project))
        );
        assert_eq!(
            parse("diag prev buffer"),
            Command::Editor(Cmd::DiagnosticPrev(DiagnosticScope::Buffer))
        );
        // A bad verb or scope is reported, not guessed.
        assert!(matches!(
            parse("diagnostic sideways"),
            Command::Message(StatusLevel::Error, _)
        ));
        assert!(matches!(
            parse("diagnostic next world"),
            Command::Message(StatusLevel::Error, _)
        ));
    }

    #[test]
    fn ambiguous_abbreviations_are_refused() {
        // `s` could mean set, split, spell or status — do not guess.
        assert!(matches!(
            parse("s"),
            Command::Message(StatusLevel::Error, _)
        ));
        // `sp` is unambiguous (`vsplit` abbreviates to `vs`), and with no
        // argument a split reuses the buffer that is already open.
        assert_eq!(parse("sp"), Command::Editor(Cmd::SplitHorizontal));
        assert_eq!(parse("vs"), Command::Editor(Cmd::SplitVertical));
        assert_eq!(
            parse("sp notes.txt"),
            Command::Open {
                path: "notes.txt".into(),
                split: Some(SplitDirection::Row)
            }
        );
    }

    #[test]
    fn bare_number_jumps_to_a_line() {
        assert_eq!(parse("42"), Command::GotoLine(42));
    }

    #[test]
    fn toggles_are_commands_too() {
        assert!(matches!(
            parse("number"),
            Command::Editor(Cmd::ToggleOption(_))
        ));
        assert!(matches!(
            parse("nonumber"),
            Command::Editor(Cmd::ToggleOption(_))
        ));
        assert!(matches!(parse("spell"), Command::Editor(_)));
    }

    #[test]
    fn unknown_commands_are_reported() {
        let command = parse("frobnicate");
        match command {
            Command::Message(StatusLevel::Error, message) => {
                assert!(message.contains("frobnicate"), "{message}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn bindable_actions_are_commands_too() {
        // Anything the keymap can bind (`vortex --list-actions`) also runs from `:`.
        assert_eq!(parse("close-pane"), Command::Editor(Cmd::ClosePane));
        assert_eq!(parse("recenter"), Command::Editor(Cmd::Recenter));
        assert_eq!(parse("equalize-panes"), Command::Editor(Cmd::EqualizePanes));
        assert_eq!(parse("zoom-pane"), Command::Editor(Cmd::ZoomPane));
        // Motions need the key state machine, so they stay out of `:`.
        assert!(matches!(
            parse("word-forward"),
            Command::Message(StatusLevel::Error, _)
        ));
    }

    #[test]
    fn set_arguments_keep_key_notations() {
        assert_eq!(
            split_arguments("tab-width=4 number <C-w>"),
            ["tab-width=4", "number", "<C-w>"]
        );
    }

    #[test]
    fn set_parses_values_toggles_and_queries() {
        let outcomes = parse_set(
            &["tab-width=6".into(), "nonumber".into(), "wrap?".into()],
            &editor(),
        );
        assert_eq!(
            outcomes[0],
            SetOutcome::Changed {
                key: "editor.tab-width".into(),
                value: "6".into()
            }
        );
        assert_eq!(
            outcomes[1],
            SetOutcome::Changed {
                key: "editor.line-number".into(),
                value: "none".into()
            }
        );
        match &outcomes[2] {
            SetOutcome::Show(text) => assert!(text.starts_with("editor.wrap=")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_set(&["nonsense".into()], &editor())[0],
            SetOutcome::Bad(_)
        ));
    }

    #[test]
    fn set_understands_inverted_and_specific_options() {
        assert_eq!(
            parse_set(&["ignorecase".into()], &editor())[0],
            SetOutcome::Changed {
                key: "search.ignore-case".into(),
                value: "true".into()
            }
        );
        assert_eq!(
            parse_set(&["nosmartcase".into()], &editor())[0],
            SetOutcome::Changed {
                key: "search.smart-case".into(),
                value: "false".into()
            }
        );
        assert_eq!(
            parse_set(&["relativenumber".into()], &editor())[0],
            SetOutcome::Changed {
                key: "editor.line-number".into(),
                value: "relative".into()
            }
        );
        assert_eq!(
            parse_set(&["expandtab".into()], &editor())[0],
            SetOutcome::Changed {
                key: "editor.expand-tab".into(),
                value: "true".into()
            }
        );
    }

    #[test]
    fn set_accepts_the_names_the_example_config_uses() {
        // `config.example.toml` spells this `scroll-margin`; vim spells it
        // `scroll`.  Both have to work, and so does the dotted path.
        assert_eq!(
            parse_set(&["scroll-margin=8".into()], &editor())[0],
            SetOutcome::Changed {
                key: "editor.scroll-margin".into(),
                value: "8".into(),
            }
        );
        assert_eq!(
            parse_set(&["editor.expand-tab=false".into()], &editor())[0],
            SetOutcome::Changed {
                key: "editor.expand-tab".into(),
                value: "false".into(),
            }
        );
        // The `no` prefix keeps working on kebab-case names.
        assert_eq!(
            parse_set(&["noignore-case".into()], &editor())[0],
            SetOutcome::Changed {
                key: "search.ignore-case".into(),
                value: "false".into(),
            }
        );
    }

    #[test]
    fn ambiguous_option_names_are_refused() {
        // `[spell] enable` and `[lsp] enable` both answer to a bare `enable`.
        assert!(matches!(
            parse_set(&["enable=true".into()], &editor())[0],
            SetOutcome::Bad(_)
        ));
        assert!(matches!(
            parse_set(&["spell.enable=true".into()], &editor())[0],
            SetOutcome::Changed { .. }
        ));
    }

    #[test]
    fn every_config_key_is_reachable_from_set() {
        use std::collections::HashMap;

        let keys = config_keys();
        assert!(
            keys.len() > 50,
            "config keys should be discovered, got {keys:?}"
        );
        let mut tails: HashMap<&str, usize> = HashMap::new();
        for key in keys {
            *tails.entry(key.rsplit('.').next().unwrap()).or_default() += 1;
            // The path used in the config file always works.
            assert_eq!(canonical_option(key).as_deref(), Some(key.as_str()));
        }
        for key in keys {
            let tail = key.rsplit('.').next().unwrap();
            if vim_option(tail).is_some() {
                // vim's name wins: `:set number` means the gutter, not the
                // `[theme] number` colour that happens to share the tail.
                continue;
            }
            if *tails.get(tail).unwrap_or(&0) == 1 {
                assert_eq!(
                    canonical_option(tail).as_deref(),
                    Some(key.as_str()),
                    "{tail}"
                );
            } else if vim_option(tail).is_none() {
                assert_eq!(
                    canonical_option(tail),
                    None,
                    "ambiguous \'{tail}\' must not resolve",
                );
            }
        }
    }

    #[test]
    fn summary_lists_effective_options() {
        let summary = option_summary(&editor());
        assert!(summary.contains("editor.tab-width=4"), "{summary}");
        assert!(summary.contains("layout.split-mode="), "{summary}");
        assert!(summary.contains("search.ripgrep=auto"), "{summary}");
    }

    #[test]
    fn the_popup_lists_every_command_alphabetically() {
        let listing = listing();
        assert_eq!(listing.len(), COMMANDS.len(), "no command may be hidden");
        let mut names: Vec<&str> = listing.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        assert_eq!(names.first(), Some(&"bdelete"), "{names:?}");
        assert_eq!(
            listing.iter().map(|spec| spec.name).collect::<Vec<_>>(),
            names,
            "the list must be alphabetical — it is a lookup, not a tour"
        );
    }

    #[test]
    fn every_bindable_command_is_listed() {
        // The palette merges `listing` with `unlisted_commands`; together they
        // must cover every action that parses as a one-shot command.  Anything
        // bindable but unlistable is a command found only by typed guess.
        for (name, action, _) in crate::action::ACTIONS {
            if !matches!(action, crate::action::Action::Cmd(_)) {
                continue;
            }
            let listed = listing().iter().any(|spec| spec.name == *name)
                || unlisted_commands().iter().any(|(n, _)| n == name);
            assert!(listed, "{name} runs as :{name} but no row lists it");
        }
        // The reported symptom: pane focus could be bound but never seen.
        let extra: Vec<&str> = unlisted_commands().iter().map(|(n, _)| *n).collect();
        for name in ["focus-next", "focus-prev", "focus-left", "zoom-pane"] {
            assert!(extra.contains(&name), "{name} missing from the palette");
        }
    }

    #[test]
    fn listed_and_unlisted_do_not_overlap() {
        // A name an ex spec already answers to gets that row, not two.
        for (name, _) in unlisted_commands() {
            assert!(command_for(name).is_none(), "{name} would get two rows");
        }
    }

    #[test]
    fn action_names_parse_and_resolve() {
        assert_eq!(parse("focus-next"), Command::Editor(Cmd::FocusNext));
        assert_eq!(resolve("focus-next"), Some("focus-next"));
        // Specs keep their names, abbreviations included; a typed word that
        // means nothing still resolves to nothing.
        assert_eq!(resolve("w"), Some("write"));
        assert_eq!(resolve("no-such-command"), None);
    }

    #[test]
    fn usage_shows_the_arguments_and_the_preview_the_parameters() {
        // The two ways a parameter becomes discoverable without reading the
        // reference: dimmed after the name, then spelled out in the preview.
        let write = COMMANDS
            .iter()
            .find(|spec| spec.name == "write")
            .expect("write");
        assert_eq!(arguments(write.usage), "[path]");
        assert_eq!(arguments(":quit[!]"), "[!]");
        assert_eq!(arguments(":only"), "");

        let lines = param_lines(write.params);
        assert!(lines.contains("path"), "{lines}");
        assert!(lines.contains("(optional)"), "{lines}");
        // The help text and the requirement are separate phrases, not one word.
        assert!(!lines.contains("empty(optional)"), "{lines}");

        let diagnostic = COMMANDS
            .iter()
            .find(|spec| spec.name == "diagnostic")
            .expect("diagnostic");
        let lines = param_lines(diagnostic.params);
        assert!(lines.contains("next | prev | list"), "{lines}");
        assert!(lines.contains("(optional)"), "{lines}");
    }

    #[test]
    fn a_typed_word_names_one_command_or_none() {
        // Enter on the popup runs what was typed when it names a command, so
        // `resolve` is what decides that — exact, abbreviation, or a prefix only
        // one command could mean.  Anything else leaves the choice to the list.
        assert_eq!(resolve("write"), Some("write"));
        assert_eq!(resolve("w"), Some("write"));
        assert_eq!(resolve("wri"), Some("write"));
        assert_eq!(resolve("q"), Some("quit"));
        // `s` could mean set, split, or spell: guessing would surprise someone.
        assert_eq!(resolve("s"), None);
        assert_eq!(resolve("frobnicate"), None);
        assert_eq!(resolve(""), None);
    }

    #[test]
    fn parameters_are_found_through_abbreviations_too() {
        // `:w` is the same command as `:write`, so it must be asked the same
        // questions; a name that resolves to nothing simply has none.
        assert!(!params_for("w").is_empty());
        assert_eq!(params_for("w").len(), params_for("write").len());
        assert!(params_for("se").first().is_some_and(|p| p.name == "option"));
        assert!(params_for("frobnicate").is_empty());
    }

    #[test]
    fn a_typed_line_with_arguments_runs_as_written() {
        assert!(has_arguments("write notes.txt"));
        assert!(has_arguments("set tab-width=4"));
        assert!(has_arguments("q!"));
        assert!(
            !has_arguments("write"),
            "a bare name may step into its params"
        );

        // What the guided stages assemble must parse back to the same command as
        // if it had been typed in one go.
        let args = vec!["notes.txt".to_string()];
        assert_eq!(parse(&assemble("write", &args)), parse("write notes.txt"));
        assert_eq!(assemble("only", &[]), "only");
    }

    #[test]
    fn parameter_candidates_offer_choices_and_narrow_as_typed() {
        let diagnostic = COMMANDS
            .iter()
            .find(|spec| spec.name == "diagnostic")
            .expect("diagnostic");
        let what = &diagnostic.params[0];
        assert_eq!(param_candidates(what, ""), ["next", "prev", "list"]);
        assert_eq!(param_candidates(what, "p"), ["prev"]);

        // A pattern has nothing to suggest before it exists.
        let grep = COMMANDS
            .iter()
            .find(|spec| spec.name == "grep")
            .expect("grep");
        assert!(param_candidates(&grep.params[0], "any").is_empty());
    }

    #[test]
    fn command_completion_keeps_a_trailing_space() {
        let completions = complete_command("sp");
        assert!(
            completions.contains(&"split ".to_string()),
            "{completions:?}"
        );
        assert!(complete_command("").len() >= COMMANDS.len() - 1);
    }

    #[test]
    fn tilde_is_expanded_in_paths() {
        if dirs::home_dir().is_some() {
            let expanded = expand_path("~/notes.md");
            assert!(!expanded.starts_with('~'));
            assert!(expanded.ends_with("notes.md"));
        }
    }
}
