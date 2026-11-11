//! Automatic formatting.
//!
//! vortex shells out to the language's own formatter.  Runs happen on the tokio
//! runtime, carry the buffer version they started from, and are dropped if the
//! buffer changed in the meantime — the editor never fights the user.

use std::path::Path;
use std::process::Stdio;

use ropey::Rope;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::buffer::{BufferId, Change};
use crate::config::Config;
use crate::event::{AsyncEvent, EventTx};
use crate::selection::{Edit, Selection, Span};

/// A formatter command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatter {
    /// Executable, or the whole command line when `through_shell` is set.
    pub program: String,
    pub args: Vec<String>,
    /// Run through `sh -c` (useful for pipelines in `[format.command]`).
    pub through_shell: bool,
    /// Where the formatted text should be reported as coming from.
    pub label: String,
}

impl Formatter {
    /// Substitute `{file}` with the buffer path (or a synthetic name).
    pub fn resolved(&self, path: Option<&Path>, language: &str) -> Formatter {
        let name = match path {
            Some(path) => path.display().to_string(),
            None => format!("buffer.{}", default_extension(language)),
        };
        let substitute = |value: &str| value.replace("{file}", &name);
        Formatter {
            program: substitute(&self.program),
            args: self.args.iter().map(|arg| substitute(arg)).collect(),
            through_shell: self.through_shell,
            label: self.label.clone(),
        }
    }
}

fn default_extension(language: &str) -> &'static str {
    match language {
        "rust" => "rs",
        "python" => "py",
        "javascript" => "js",
        "typescript" | "tsx" => "ts",
        "json" => "json",
        "toml" => "toml",
        "yaml" => "yaml",
        "go" => "go",
        "c" => "c",
        "bash" => "sh",
        "css" => "css",
        "html" => "html",
        _ => "txt",
    }
}

/// Built-in formatters, used unless `[format.command]` overrides them.
const DEFAULT_FORMATTERS: &[(&str, &str, &[&str])] = &[
    (
        "rust",
        "rustfmt",
        &["--edition", "2024", "--emit", "stdout"],
    ),
    ("python", "ruff", &["format", "-"]),
    ("javascript", "prettier", &["--stdin-filepath", "{file}"]),
    ("typescript", "prettier", &["--stdin-filepath", "{file}"]),
    ("tsx", "prettier", &["--stdin-filepath", "{file}"]),
    ("json", "prettier", &["--stdin-filepath", "{file}"]),
    ("yaml", "prettier", &["--stdin-filepath", "{file}"]),
    ("markdown", "prettier", &["--stdin-filepath", "{file}"]),
    ("css", "prettier", &["--stdin-filepath", "{file}"]),
    ("html", "prettier", &["--stdin-filepath", "{file}"]),
    ("toml", "taplo", &["fmt", "-"]),
    ("go", "gofmt", &[]),
    ("c", "clang-format", &["--assume-filename={file}"]),
];

/// Resolve the formatter for a language.
///
/// `None` means "no formatter configured or the language has none".  An empty
/// override in the config disables formatting for that language.
pub fn formatter_for(language: &str, config: &Config) -> Option<Formatter> {
    if let Some(entry) = config.format.command.get(language) {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        return Some(parse_command(entry, language));
    }
    DEFAULT_FORMATTERS
        .iter()
        .find(|(id, _, _)| *id == language)
        .map(|(id, program, args)| Formatter {
            program: (*program).to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            through_shell: false,
            label: format!("{program} ({id})"),
        })
}

/// Parse a `[format.command]` value: `"taplo fmt -"` or `"!prettier | sed …"`.
fn parse_command(entry: &str, _language: &str) -> Formatter {
    if let Some(shell_command) = entry.strip_prefix('!') {
        return Formatter {
            program: shell_command.to_string(),
            args: Vec::new(),
            through_shell: true,
            label: shell_command.to_string(),
        };
    }
    let mut parts = entry.split_whitespace();
    let program = parts.next().unwrap_or("").to_string();
    let args = parts.map(str::to_string).collect();
    Formatter {
        program,
        args,
        through_shell: false,
        label: entry.to_string(),
    }
}

/// The outcome of a formatter run.
#[derive(Debug, Clone)]
pub struct FormatResult {
    pub buffer: BufferId,
    /// Buffer version the formatter input was produced from.
    pub version: u64,
    pub formatter: String,
    /// `Some(text)` when the formatter produced output worth applying.
    pub text: Option<String>,
    pub error: Option<String>,
}

impl FormatResult {
    pub fn applied(&self) -> bool {
        self.text.is_some()
    }
}

/// Run a formatter in the background and report through `tx`.
pub fn spawn_format(
    tx: EventTx,
    buffer: BufferId,
    version: u64,
    formatter: Formatter,
    text: String,
) {
    tokio::spawn(async move {
        match run(&formatter, &text).await {
            Ok(Some(formatted)) => {
                tx.send_async(AsyncEvent::Formatted(FormatResult {
                    buffer,
                    version,
                    formatter: formatter.label,
                    text: (formatted != text).then_some(formatted),
                    error: None,
                }));
            }
            Ok(None) => {
                tx.send_async(AsyncEvent::Formatted(FormatResult {
                    buffer,
                    version,
                    formatter: formatter.label,
                    text: None,
                    error: None,
                }));
            }
            Err(error) => {
                tx.send_async(AsyncEvent::Formatted(FormatResult {
                    buffer,
                    version,
                    formatter: formatter.label,
                    text: None,
                    error: Some(error),
                }));
            }
        }
    });
}

async fn run(formatter: &Formatter, text: &str) -> Result<Option<String>, String> {
    let mut command = if formatter.through_shell {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&formatter.program);
        command
    } else {
        let mut command = Command::new(&formatter.program);
        command.args(&formatter.args);
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| format!("{}: {error}", formatter.program))?;

    if let Some(mut stdin) = child.stdin.take() {
        let payload = text.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.flush().await;
        });
    }

    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| "formatter timed out".to_string())?
        .map_err(|error| error.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.trim();
        return Err(if reason.is_empty() {
            format!("{} exited with {}", formatter.program, output.status)
        } else {
            reason.to_string()
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() && !text.trim().is_empty() {
        // A formatter that swallows the buffer is broken, not helpful.
        return Ok(None);
    }
    Ok(Some(stdout))
}

/// Turn formatter output into buffer changes.
///
/// Common prefix and suffix lines are trimmed so the change covers only the
/// region that actually differs; unchanged edges keep their undo granularity.
pub fn diff_changes(rope: &Rope, new_text: &str) -> Vec<Change> {
    let old = rope.to_string();
    if old == new_text {
        return Vec::new();
    }
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new_text.split_inclusive('\n').collect();

    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix + prefix < old_lines.len()
        && suffix + prefix < new_lines.len()
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let start: usize = old_lines[..prefix].iter().map(|line| line.len()).sum();
    let tail: usize = if suffix == 0 {
        0
    } else {
        old_lines[old_lines.len() - suffix..]
            .iter()
            .map(|line| line.len())
            .sum()
    };
    let old_end = old.len().saturating_sub(tail);
    let middle: String = new_lines[prefix..new_lines.len().saturating_sub(suffix)].concat();
    if start == old_end && middle.is_empty() {
        return Vec::new();
    }
    vec![Change::replace(Span::new(start, old_end), middle)]
}

/// `(line, character)` of a byte offset, so a cursor can be restored after a
/// whole-buffer rewrite.
pub fn position_of(rope: &Rope, byte: usize) -> (usize, usize) {
    let byte = crate::buffer::floor_boundary(rope, byte);
    let line = if rope.len_bytes() == 0 {
        0
    } else {
        rope.byte_to_line(byte)
    };
    let line_start = rope.line_to_char(line);
    (line, rope.byte_to_char(byte) - line_start)
}

/// Byte offset of `(line, character)`, clamped to the end of that line.
pub fn byte_at_position(rope: &Rope, position: (usize, usize)) -> usize {
    if rope.len_bytes() == 0 {
        return 0;
    }
    let line = position.0.min(rope.len_lines().saturating_sub(1));
    let start = rope.line_to_char(line);
    let mut end = rope.line_to_char(line + 1);
    // Do not land on the line break.
    if end > start && rope.char(end - 1) == '\n' {
        end -= 1;
    }
    let target = (start + position.1).clamp(start, end.max(start));
    rope.char_to_byte(target)
}

/// Where a selection belongs once formatter output has been applied.
///
/// `position` is the caret's `(line, character)` in the text *before* the
/// rewrite.  Outside the rewritten text the selection travels with the text
/// around it: a formatter that splits or joins lines above the caret must not
/// leave it sitting on the line number it happened to be on.  Inside the
/// rewritten text no byte offset survives, so the caret keeps the line and
/// column it had — the same line of the reformatted block — clamped into what
/// is there now.
pub fn selection_after_rewrite(
    position: (usize, usize),
    new: &Rope,
    edits: &[Edit],
    selection: &Selection,
) -> Selection {
    let caret = selection.cursor();
    let rewritten = edits
        .iter()
        .any(|edit| caret > edit.range.start && caret < edit.range.end);
    if rewritten {
        return Selection::point(byte_at_position(new, position));
    }
    selection.map(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(command: &str, value: &str) -> Config {
        let text = format!("[format.command]\n{command} = \"{value}\"\n");
        Config::parse(&text).unwrap().0
    }

    #[test]
    fn default_formatters_per_language() {
        let config = Config::default();
        let rust = formatter_for("rust", &config).expect("rust formatter");
        assert_eq!(rust.program, "rustfmt");
        assert!(formatter_for("text", &config).is_none());
    }

    #[test]
    fn config_overrides_and_disables() {
        let config = config_with("rust", "dprint fmt --stdin {file}");
        let formatter = formatter_for("rust", &config).unwrap();
        assert_eq!(formatter.program, "dprint");
        assert_eq!(formatter.args, ["fmt", "--stdin", "{file}"]);

        let disabled = config_with("rust", "");
        assert!(formatter_for("rust", &disabled).is_none());
    }

    #[test]
    fn shell_formatters_are_marked() {
        let config = config_with("markdown", "!pandoc -f markdown -t markdown");
        let formatter = formatter_for("markdown", &config).unwrap();
        assert!(formatter.through_shell);
        assert_eq!(formatter.program, "pandoc -f markdown -t markdown");
    }

    #[test]
    fn placeholders_resolve_to_the_file_name() {
        let config = config_with("json", "prettier --stdin-filepath {file}");
        let formatter = formatter_for("json", &config)
            .unwrap()
            .resolved(Some(Path::new("package.json")), "json");
        assert_eq!(formatter.args, ["--stdin-filepath", "package.json"]);

        let scratch = formatter_for("json", &config)
            .unwrap()
            .resolved(None, "json");
        assert_eq!(scratch.args, ["--stdin-filepath", "buffer.json"]);
    }

    #[test]
    fn diff_trims_unchanged_edges() {
        let rope = Rope::from("one\ntwo\nthree\n");
        let changes = diff_changes(&rope, "one\nTWO\nthree\n");
        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert_eq!(change.range.start, 4);
        assert_eq!(change.range.end, 8);
        assert_eq!(change.text, "TWO\n");
    }

    #[test]
    fn diff_of_identical_text_is_empty() {
        let rope = Rope::from("same\n");
        assert!(diff_changes(&rope, "same\n").is_empty());
    }

    #[test]
    fn positions_round_trip_through_a_rewrite() {
        let rope = Rope::from("alpha\nbeta\ngamma\n");
        let position = position_of(&rope, 7); // the "e" of "beta"
        assert_eq!(position, (1, 1));
        assert_eq!(byte_at_position(&rope, position), 7);
        // A clamped position lands on the line end, not on the break.
        let far = byte_at_position(&rope, (0, 100));
        assert_eq!(far, 5);
    }

    /// The edits a rewrite amounts to, in the shape `Selection::map` wants.
    fn edits(old: &Rope, new_text: &str) -> Vec<Edit> {
        diff_changes(old, new_text)
            .iter()
            .map(|change| Edit::new(change.range, change.text.len()))
            .collect()
    }

    #[test]
    fn a_caret_below_the_rewrite_travels_with_the_text() {
        // The formatter inserted a line above the caret.  Restoring the caret by
        // the line number it happened to be on drops it onto the line that
        // moved up into its place — the "cursor resets at random" jump.
        let old = Rope::from("a\nb\nc\n");
        let new_text = "a\nX\nb\nc\n";
        let new = Rope::from(new_text);
        let caret = 4; // the "c"
        let position = position_of(&old, caret);
        let after = selection_after_rewrite(
            position,
            &new,
            &edits(&old, new_text),
            &Selection::point(caret),
        );
        assert_eq!(
            after.cursor(),
            6,
            "still the `c`, one line later in the file"
        );
        assert_eq!(&new_text[after.cursor()..after.cursor() + 1], "c");
    }

    #[test]
    fn a_caret_inside_the_rewrite_keeps_its_line_and_column() {
        // Inside the rewritten text there is no byte offset left to keep, so the
        // caret stays on the same line and column of the reformatted block.
        let old = Rope::from("one\ntwo\nthree\n");
        let new_text = "one\nTWO THREE\nthree\n";
        let new = Rope::from(new_text);
        let caret = 5; // the "w" of "two"
        let position = position_of(&old, caret);
        let after = selection_after_rewrite(
            position,
            &new,
            &edits(&old, new_text),
            &Selection::point(caret),
        );
        assert_eq!(position, (1, 1));
        assert_eq!(after.cursor(), 5, "one character into the rewritten line");
    }

    #[test]
    fn a_selection_outside_the_rewrite_keeps_its_spans() {
        // A visual selection above the rewrite is untouched, so it must survive
        // as a selection rather than collapsing to a caret.
        let old = Rope::from("one\ntwo\nthree\n");
        let new_text = "one\ntwo\nTHREE\nfour\n";
        let new = Rope::from(new_text);
        let selection = Selection::between(0, 3); // "one"
        let after = selection_after_rewrite(
            position_of(&old, 3),
            &new,
            &edits(&old, new_text),
            &selection,
        );
        assert_eq!(after.primary(), Span::new(0, 3));
    }

    #[tokio::test]
    async fn missing_formatter_reports_an_error() {
        let formatter = Formatter {
            program: "definitely-not-a-real-formatter".into(),
            args: Vec::new(),
            through_shell: false,
            label: "missing".into(),
        };
        let error = run(&formatter, "x").await.unwrap_err();
        assert!(error.contains("definitely-not-a-real-formatter"));
    }

    #[tokio::test]
    async fn runs_a_real_command() {
        let formatter = Formatter {
            program: "cat".into(),
            args: Vec::new(),
            through_shell: false,
            label: "cat".into(),
        };
        assert_eq!(
            run(&formatter, "hello").await.unwrap().as_deref(),
            Some("hello")
        );
    }
}
