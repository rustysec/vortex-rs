//! Language server protocol support — a working scaffold, not a full client.
//!
//! What works today:
//! * one server process per project root, started on demand from `[lsp.servers]`;
//! * the `initialize` / `initialized` handshake and `textDocument/didOpen` /
//!   `didChange` / `didSave` for buffers in that root;
//! * `textDocument/publishDiagnostics`, turned into [`crate::diagnostic::Diagnostic`]
//!   values that the status area and `ui` render like spell-check hints;
//! * pull diagnostics (`textDocument/diagnostic`), requested when the server
//!   sends `workspace/diagnostic/refresh`.  rust-analyzer pushes only the
//!   compiler's view of the *saved* file, so without the pull an unsaved error
//!   never shows a diagnostic;
//! * hover, completion, code actions and formatting, and navigation —
//!   `textDocument/definition` and `textDocument/references`.
//!
//! What is deliberately not implemented: renames, `codeAction/resolve` and
//! workspace-symbol lookup.
//!
//! [lsp] `enable` defaults to `false`: with no server configured nothing here
//! runs at all.

pub mod jsonrpc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc};

use crate::buffer::{Buffer, BufferId};
use crate::config::{Config, LspSync};
use crate::diagnostic::{Diagnostic, DiagnosticSource, DiagnosticTag, Severity};
use crate::event::{AsyncEvent, EventTx, JobId};
use crate::selection::Span;

/// Which stream a diagnostic set came from.
///
/// rust-analyzer pushes the compiler's view of the *saved* file (its flycheck
/// results, `source: "rustc"`) but serves its own analysis of the buffer as
/// edited only through pull requests (`textDocument/diagnostic`).  A client that
/// listens to pushes alone therefore sees a clean file that is full of unsaved
/// type errors.  The two sets describe different text cuts, so they are tracked
/// separately and merged for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticOrigin {
    /// `textDocument/publishDiagnostics`, pushed by the server.
    Push,
    /// An answer to `textDocument/diagnostic`, pulled on request.
    Pull,
}

/// Traffic from a language server, reported to the editor.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// A server finished its handshake and is ready.
    Ready { language: String, root: PathBuf },
    /// A server could not be started or died.
    Failed { language: String, error: String },
    /// Diagnostics for one file.
    ///
    /// Ranges arrive as LSP line/character positions: only the editor owns the
    /// buffer, so it converts them to byte offsets.  `version` is the document
    /// version the server analysed; a publish for a version the buffer has
    /// already moved past is stale and must be dropped, or its ranges land on
    /// text that has since changed.
    Diagnostics {
        uri: String,
        version: Option<i64>,
        items: Vec<DiagnosticItem>,
        /// Which stream this set came from.  The two are kept apart because
        /// they describe different text: a push usually reports the compiler's
        /// view of what is on disk, while a pull reports the server's own view
        /// of the buffer as edited.  Merging them keeps both.
        origin: DiagnosticOrigin,
        /// A pull answered `unchanged`: the previous set still stands, so the
        /// receiver must keep what it has instead of replacing it with nothing.
        /// Always false for a push.
        unchanged: bool,
    },
    /// A progress or status message worth showing in the status area.
    Progress { language: String, message: String },
    /// The server says its own diagnostics changed and should be pulled again.
    ///
    /// rust-analyzer never pushes its native analysis of an edited buffer; it
    /// sends `workspace/diagnostic/refresh` and expects the client to re-issue
    /// `textDocument/diagnostic`.  The app answers by pulling for every open
    /// document this server owns.
    DiagnosticsRefresh { language: String },
    /// Completion candidates for one file, in answer to a request.
    ///
    /// `request` is the id the app sent, so a slow answer for a caret that has
    /// since moved can be dropped instead of popping up under the wrong word.
    Completions {
        uri: String,
        request: i64,
        items: Vec<CompletionItem>,
    },
    /// What the server knows about a position, as plain text.
    ///
    /// `request` matches the id the app sent, so an answer for a caret that has
    /// since moved is dropped rather than shown under the wrong symbol.
    Hover {
        uri: String,
        request: i64,
        text: String,
    },
    /// The server rewrote a document (or a range of one).
    ///
    /// `edits` are byte spans in the text the server was told about, which is
    /// what the app resolves them against.  `error` is set when the server
    /// refused the request, so the caller can fall back to the external
    /// formatter instead of silently doing nothing.
    Formatted {
        uri: String,
        request: i64,
        edits: Vec<TextEdit>,
        error: Option<String>,
    },
    /// Quick fixes the server offers for a range.
    CodeActions {
        uri: String,
        request: i64,
        actions: Vec<CodeAction>,
    },
    /// Where the symbol at a position is defined.
    ///
    /// `uri` is the document the *request* was made about; the locations can
    /// point at any file, since definitions cross files.  A definition can be
    /// several locations (a trait method and its implementations), so the
    /// receiver must handle a list even though one is the common answer.
    Definition {
        uri: String,
        request: i64,
        locations: Vec<Location>,
    },
    /// Every location that mentions the symbol at a position, in answer to
    /// `textDocument/references`.  `uri` is the document asked about.
    References {
        uri: String,
        request: i64,
        locations: Vec<Location>,
    },
    /// A server exited; it will be restarted the next time it is needed.
    Closed { language: String, code: Option<i32> },
}

/// A server we know how to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub language: String,
    pub command: String,
    pub args: Vec<String>,
    /// How edits are reported to this server.
    pub sync: LspSync,
}

/// Resolve the server for a language from `[lsp.servers]`.
pub fn server_for(language: &str, config: &Config) -> Option<ServerSpec> {
    if !config.lsp.enable {
        return None;
    }
    if let Some(entry) = config.lsp.servers.get(language) {
        if entry.enable && !entry.command.trim().is_empty() {
            return Some(ServerSpec {
                language: language.to_string(),
                command: entry.command.clone(),
                args: entry.args.clone(),
                sync: config.lsp.sync,
            });
        }
        return None;
    }
    // A few well-known servers are implied so that enabling LSP does something
    // useful out of the box, as long as the binary is installed.
    let (command, args) = match language {
        "rust" => ("rust-analyzer", Vec::new()),
        "python" => ("pylsp", Vec::new()),
        "go" => ("gopls", Vec::new()),
        "c" => ("clangd", Vec::new()),
        _ => return None,
    };
    which(command)?;
    Some(ServerSpec {
        language: language.to_string(),
        command: command.to_string(),
        args,
        sync: config.lsp.sync,
    })
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(program);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Convert a path to a `file://` URI.
pub fn uri_for(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    format!("file://{}", absolute.display())
}

/// Convert a `file://` URI back to a path, if it is one.
pub fn path_for_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(PathBuf::from(rest))
}

/// One diagnostic as the server reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticItem {
    /// `(start line, start character, end line, end character)`, 0-based.
    pub range: (usize, usize, usize, usize),
    pub severity: Severity,
    pub message: String,
    pub source: String,
    /// `code` as the server sent it: a string, or a number rendered as one.
    pub code: Option<String>,
    /// Decoded `tags` (`unnecessary`, `deprecated`).
    pub tags: Vec<DiagnosticTag>,
    /// `relatedInformation`, as `(message, range)`.
    pub related: Vec<(String, (usize, usize, usize, usize))>,
}

impl DiagnosticItem {
    /// Resolve to buffer byte offsets.
    ///
    /// LSP columns are UTF-16 code units, so the buffer does the conversion —
    /// treating them as bytes puts every diagnostic after a non-ASCII character
    /// on the wrong span.
    ///
    /// `source` is the storage bucket the caller wants this in.  It must match
    /// what the caller will later pass to `set_diagnostics`: the two streams are
    /// replaced independently, and a pulled set stored under the pushed source
    /// would never be cleared on the next pull, so the list would grow on every
    /// keystroke.
    pub fn to_diagnostic(&self, buffer: &Buffer, source: DiagnosticSource) -> Diagnostic {
        let start = buffer.byte_at_utf16(self.range.0, self.range.1);
        let end = buffer.byte_at_utf16(self.range.2, self.range.3);
        let related = self
            .related
            .iter()
            .map(|(message, range)| {
                let start = buffer.byte_at_utf16(range.0, range.1);
                let end = buffer.byte_at_utf16(range.2, range.3);
                (message.clone(), Span::new(start, end))
            })
            .collect();
        Diagnostic::new(
            Span::new(start, end),
            self.severity,
            format!("{}: {}", self.source, self.message),
            source,
        )
        .with_code(self.code.clone())
        .with_tags(self.tags.clone())
        .with_related(related)
    }
}

/// Decode a whole `publishDiagnostics` notification into an event.
///
/// The notification is a JSON-RPC envelope: the payload lives under `params`,
/// not at the top level.  Reading `diagnostics` off the envelope itself found
/// nothing and returned an empty list, so every diagnostic was silently
/// dropped — the unit tests missed it because they feed a bare params object.
pub fn diagnostics_event(notification: &Value) -> Option<LspEvent> {
    let params = notification.get("params")?;
    Some(LspEvent::Diagnostics {
        uri: params
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version: params.get("version").and_then(Value::as_i64),
        items: diagnostic_items(params),
        origin: DiagnosticOrigin::Push,
        unchanged: false,
    })
}

/// Decode `publishDiagnostics` parameters.
pub fn diagnostic_items(params: &Value) -> Vec<DiagnosticItem> {
    decode_diagnostic_list(params.get("diagnostics"))
}

/// Decode the `items` of a pull-diagnostics report (`textDocument/diagnostic`).
///
/// The report may be `full` (a whole set) or `unchanged` (nothing has changed
/// since the `resultId` we last saw).  An `unchanged` report carries no items
/// and must not be mistaken for "this file is clean": the caller keeps whatever
/// it already had.
pub fn pull_diagnostic_items(result: &Value) -> Option<Vec<DiagnosticItem>> {
    match result.get("kind").and_then(Value::as_str) {
        // `unchanged` means the previous report still stands; `None` tells the
        // caller to leave its copy alone instead of replacing it with nothing.
        Some("unchanged") => None,
        _ => Some(decode_diagnostic_list(result.get("items"))),
    }
}

/// Decode an array of LSP `Diagnostic` objects.
fn decode_diagnostic_list(list: Option<&Value>) -> Vec<DiagnosticItem> {
    let Some(items) = list.and_then(Value::as_array) else {
        return Vec::new();
    };
    items.iter().filter_map(decode_diagnostic_item).collect()
}

fn decode_diagnostic_item(item: &Value) -> Option<DiagnosticItem> {
    let start = position(item.pointer("/range/start")?)?;
    let end = position(item.pointer("/range/end")?)?;
    // `code` is `string | number` in the protocol; both are shown as text, so
    // a numeric code is stringified rather than dropped.
    let code = item.get("code").and_then(|code| {
        code.as_str()
            .map(str::to_string)
            .or_else(|| code.as_u64().map(|number| number.to_string()))
    });
    let tags = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_u64)
                .filter_map(DiagnosticTag::from_lsp)
                .collect()
        })
        .unwrap_or_default();
    let related = item
        .get("relatedInformation")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let message = entry
                        .pointer("/message")
                        .and_then(Value::as_str)?
                        .to_string();
                    let start = position(entry.pointer("/location/range/start")?)?;
                    let end = position(entry.pointer("/location/range/end")?)?;
                    Some((message, (start.0, start.1, end.0, end.1)))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(DiagnosticItem {
        range: (start.0, start.1, end.0, end.1),
        severity: match item.get("severity").and_then(Value::as_u64) {
            Some(1) => Severity::Error,
            Some(2) => Severity::Warning,
            Some(3) => Severity::Info,
            _ => Severity::Hint,
        },
        message: item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("diagnostic")
            .to_string(),
        source: item
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("lsp")
            .to_string(),
        code,
        tags,
        related,
    })
}

/// Turn an LSP `{start,end}{line,character}` range into a byte span.
///
/// `line_starts` are the line start offsets of `text`; `character` counts
/// UTF-16 code units, which is what LSP positions are measured in.
pub fn range_to_span(
    text: &str,
    line_starts: &[usize],
    start: (usize, usize),
    end: (usize, usize),
) -> Span {
    let line_start = |line: usize| {
        *line_starts
            .get(line.min(line_starts.len().saturating_sub(1)))
            .unwrap_or(&0)
    };
    let byte_at = |(line, character): (usize, usize)| {
        let start = line_start(line);
        let end = line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(text.len())
            .min(text.len());
        let slice = text.get(start..end).unwrap_or_default();
        start + crate::buffer::utf16_col_to_byte(slice, character)
    };
    let start_byte = byte_at(start).min(text.len());
    let end_byte = byte_at(end).clamp(start_byte, text.len());
    Span::new(start_byte, end_byte)
}

fn position(value: &Value) -> Option<(usize, usize)> {
    Some((
        value.get("line")?.as_u64()? as usize,
        value.get("character")?.as_u64()? as usize,
    ))
}

/// One completion candidate as the server described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Text shown in the popup.
    pub label: String,
    /// Text inserted when the item is accepted; falls back to `label`.
    pub insert_text: String,
    /// True when `insert_text` carries snippet syntax (`$0`, `${1:default}`),
    /// i.e. the server sent `insertTextFormat: 2`.
    pub snippet: bool,
    /// `CompletionItemKind` number, for the icon/colour.
    pub kind: Option<u64>,
    /// Short type or signature shown after the label.
    pub detail: Option<String>,
    /// Documentation, shown as a preview line.
    pub documentation: Option<String>,
    /// Text the server wants replaced, as a byte span in the buffer.
    pub replace: Option<Span>,
}

impl CompletionItem {
    /// The text this item inserts.
    pub fn text(&self) -> &str {
        if self.insert_text.is_empty() {
            &self.label
        } else {
            &self.insert_text
        }
    }

    /// A short human name for the kind, used in the popup.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            Some(2) | Some(3) => "fn",
            Some(4) => "ctor",
            Some(5) => "field",
            Some(6) => "var",
            Some(7) => "class",
            Some(8) => "iface",
            Some(9) => "mod",
            Some(10) => "prop",
            Some(13) => "enum",
            Some(14) => "kw",
            Some(15) => "snippet",
            Some(21) => "const",
            Some(22) => "struct",
            Some(23) => "event",
            Some(25) => "type",
            _ => "",
        }
    }
}

/// Decode a `textDocument/completion` result.
///
/// The result is either a bare array or a `CompletionList` with an `items`
/// array; both shapes are legal and servers use both.  `text` and its
/// `line_starts` resolve a `textEdit`'s range to a byte span.
pub fn completion_items(result: &Value, text: &str, line_starts: &[usize]) -> Vec<CompletionItem> {
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| result.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let label = item.get("label").and_then(Value::as_str)?.to_string();
            // A `textEdit` is the server's exact instruction — its `newText`
            // wins over `insertText`, which may differ (paths, call parens).
            let new_text = item
                .pointer("/textEdit/newText")
                .and_then(Value::as_str)
                .map(str::to_string);
            let snippet = item.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
            let insert_text = new_text.unwrap_or_else(|| {
                item.get("insertText")
                    .and_then(Value::as_str)
                    .map_or_else(|| label.clone(), str::to_string)
            });
            let replace = item.pointer("/textEdit/range").and_then(|range| {
                let start = position(range.get("start")?)?;
                let end = position(range.get("end")?)?;
                Some(range_to_span(text, line_starts, start, end))
            });
            let documentation = item.get("documentation").and_then(|value| {
                value.as_str().map(str::to_string).or_else(|| {
                    value
                        .get("value")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            });
            Some(CompletionItem {
                label,
                insert_text,
                snippet,
                kind: item.get("kind").and_then(Value::as_u64),
                detail: item
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                documentation,
                replace,
            })
        })
        .collect()
}

/// Decode a `textDocument/hover` result into plain text.
///
/// The result is `{ contents, range? }`, and `contents` is one of: a string, a
/// `MarkupContent` (`{ kind, value }`), or an array of either.  Markdown is
/// flattened to plain text — the status area and the float draw plain text, and
/// a stray `**` reads worse than no emphasis at all.
pub fn hover_text(result: &Value) -> String {
    let contents = result.get("contents").unwrap_or(result);
    let mut parts = Vec::new();
    collect_hover(contents, &mut parts);
    parts
        .into_iter()
        .map(|part| strip_markdown(&part))
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_hover(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_hover(item, out);
            }
        }
        Value::Object(_) => {
            // `MarkupContent` carries the text in `value`; a `MarkedString`
            // object carries it in `value` too, so one lookup covers both.
            if let Some(text) = value.get("value").and_then(Value::as_str) {
                out.push(text.to_string());
            }
        }
        _ => {}
    }
}

/// Flatten the markdown a server sends into readable plain text.
///
/// Only the constructs that actually show up in hover text are handled: code
/// fences, inline code, emphasis and heading markers.  Anything else is left
/// alone rather than mangled.
fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            // A fence line carries no content of its own.
            continue;
        }
        let line = trimmed.strip_prefix("# ").unwrap_or(trimmed);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    let out = out.replace("**", "").replace("__", "");
    // Inline code: drop the backticks, keep the code.
    out.replace('`', "").trim_end().to_string()
}

/// Decode a `textDocument/formatting` (or `rangeFormatting`) result.
///
/// The result is an array of `TextEdit`s, or `null` when the server has nothing
/// to change.  Ranges are resolved against the text the server was told about.
pub fn text_edits(result: &Value, text: &str, line_starts: &[usize]) -> Vec<TextEdit> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let start = position(item.pointer("/range/start")?)?;
            let end = position(item.pointer("/range/end")?)?;
            Some(TextEdit {
                range: range_to_span(text, line_starts, start, end),
                new_text: item
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

/// Decode a `textDocument/codeAction` result into applicable quick fixes.
///
/// A `CodeAction` may carry its edits directly or behind a `codeAction/resolve`
/// round trip; only the direct form is used, because resolving would need a
/// second request per action and the servers that matter send edits inline for
/// quick fixes.  A bare `Command` has no edits and is skipped.
pub fn code_actions(result: &Value, text: &str, line_starts: &[usize]) -> Vec<CodeAction> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let title = item.get("title").and_then(Value::as_str)?.to_string();
            let edits = item
                .pointer("/edit/changes")
                .and_then(|changes| changes.as_object())
                .map(|changes| {
                    changes
                        .values()
                        .filter_map(Value::as_array)
                        .flatten()
                        .filter_map(|edit| {
                            let start = position(edit.pointer("/range/start")?)?;
                            let end = position(edit.pointer("/range/end")?)?;
                            Some(TextEdit {
                                range: range_to_span(text, line_starts, start, end),
                                new_text: edit
                                    .get("newText")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (!edits.is_empty()).then_some(CodeAction { title, edits })
        })
        .collect()
}

/// Encode a diagnostic as the LSP `Diagnostic` object a `codeAction` context
/// expects.
///
/// Servers key their quick fixes off these: rust-analyzer offers "remove unused
/// binding" only when it is told which diagnostic is being fixed, so sending an
/// empty list means an empty answer.
pub fn diagnostic_json(buffer: &Buffer, diagnostic: &Diagnostic) -> Value {
    let start = (
        buffer.line_of(diagnostic.range.start),
        buffer.utf16_col_in_line(diagnostic.range.start),
    );
    let end = (
        buffer.line_of(diagnostic.range.end),
        buffer.utf16_col_in_line(diagnostic.range.end),
    );
    let mut value = json!({
        "range": {
            "start": { "line": start.0, "character": start.1 },
            "end": { "line": end.0, "character": end.1 }
        },
        "severity": diagnostic.severity.number(),
        "message": diagnostic.message,
        "source": "vortex",
    });
    if let Some(code) = &diagnostic.code {
        value["code"] = json!(code);
    }
    value
}

/// What a request in flight is waiting for.
///
/// The answer to a request is decoded differently per kind, so the id alone is
/// not enough: the multiplexer has to remember what it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Completion,
    Hover,
    Formatting,
    RangeFormatting,
    CodeAction,
    Definition,
    References,
    /// A `textDocument/diagnostic` pull, whose answer is a whole set for the
    /// document rather than something tied to the caret.
    PullDiagnostics,
}

/// A request in flight: what it is, and which document it is about.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub kind: PendingKind,
    pub uri: String,
    /// The buffer version the request was made against, for the kinds that
    /// describe a document snapshot.  A pull-diagnostics answer carries no
    /// version of its own, so the version recorded here is what lets the editor
    /// drop an answer that arrived after the buffer moved on.
    pub version: Option<i64>,
}

/// One text replacement the server asked for, as a byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Span,
    pub new_text: String,
}

/// One code action the server offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    /// What the picker shows.
    pub title: String,
    /// The edits it applies, already resolved to byte spans.
    pub edits: Vec<TextEdit>,
}

/// One document position the server pointed at (a definition or a reference),
/// as a file URI and a 0-based `(line, UTF-16 column)` start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub line: usize,
    pub character: usize,
}

/// Decode a `textDocument/definition` or `textDocument/references` result.
///
/// The spec allows many shapes: `References` is an array of `Location`s,
/// while `Definition` may be a single `Location`, an array of them, an array
/// of `LocationLink`s, or even a bare `Range` — which belongs to the document
/// the request was made about, and that is what `fallback_uri` supplies.  A
/// `LocationLink` carries its target URI plus both a covering `targetRange`
/// and the symbol-tight `targetSelectionRange`; the selection range wins when
/// the server sends one, because it lands on the name rather than the whole
/// item.  `null` — the server found nothing — decodes to an empty list.
pub fn locations(result: &Value, fallback_uri: &str) -> Vec<Location> {
    let items: Vec<&Value> = match result {
        Value::Null => return Vec::new(),
        Value::Array(items) => items.iter().collect(),
        single => vec![single],
    };
    items
        .iter()
        .filter_map(|item| {
            let (uri, start) = if let Some(uri) = item.get("targetUri").and_then(Value::as_str) {
                // LocationLink.
                let range = item
                    .get("targetSelectionRange")
                    .or_else(|| item.get("targetRange"))?;
                (uri.to_string(), position(range.get("start")?)?)
            } else if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                // Location.
                (uri.to_string(), position(item.pointer("/range/start")?)?)
            } else {
                // Bare Range, in the requesting document.
                (fallback_uri.to_string(), position(item.pointer("/start")?)?)
            };
            Some(Location {
                uri,
                line: start.0,
                character: start.1,
            })
        })
        .collect()
}

/// Outgoing messages the app can ask a running server to send.
#[derive(Debug)]
enum Outgoing {
    Open {
        uri: String,
        language: String,
        text: String,
        /// The buffer version the text belongs to.  `didOpen` used to claim 1
        /// unconditionally, which made the app drop every diagnostic for a
        /// buffer that was already past version 1 when the server started —
        /// exactly what happens when LSP is switched on mid-session.
        version: i64,
    },
    Change {
        uri: String,
        text: String,
        version: i64,
    },
    /// A set of ranged edits, for incremental sync.
    ///
    /// Each entry is applied to the document the previous one produced, so the
    /// order matters and the ranges are already in LSP positions.
    RangedChange {
        uri: String,
        changes: Vec<crate::buffer::RecordedChange>,
        version: i64,
    },
    Save {
        uri: String,
    },
    /// Ask for completions at a position; `request` is the id the answer will
    /// carry back, and `trigger` is the character just typed (`:` or `.`) when
    /// the request answers a member/path access rather than plain typing.
    Complete {
        uri: String,
        request: i64,
        line: usize,
        character: usize,
        trigger: Option<String>,
    },
    /// Ask what is at a position.
    Hover {
        uri: String,
        request: i64,
        line: usize,
        character: usize,
    },
    /// Ask the server to format a whole document.
    Format {
        uri: String,
        request: i64,
        tab_size: usize,
        insert_spaces: bool,
    },
    /// Ask the server to format one range.
    RangeFormat {
        uri: String,
        request: i64,
        start: (usize, usize),
        end: (usize, usize),
        tab_size: usize,
        insert_spaces: bool,
    },
    /// Ask for quick fixes covering a range.
    CodeAction {
        uri: String,
        request: i64,
        start: (usize, usize),
        end: (usize, usize),
        /// The diagnostics the range covers, as LSP `Diagnostic` objects.
        ///
        /// Servers key their fixes off these: rust-analyzer offers "remove
        /// unused binding" only when it is told which diagnostic is being
        /// fixed, so an empty list here means an empty answer.
        diagnostics: Vec<Value>,
    },
    /// Ask where the symbol at a position is defined.
    Definition {
        uri: String,
        request: i64,
        line: usize,
        character: usize,
    },
    /// Ask for every location that mentions the symbol at a position.
    References {
        uri: String,
        request: i64,
        line: usize,
        character: usize,
    },
    /// Ask the server for its own analysis of a document.
    ///
    /// rust-analyzer publishes the compiler's view of the *saved* file, but
    /// answers `textDocument/diagnostic` from its in-memory view of the edited
    /// buffer.  Without this pull, an unsaved error shows no diagnostic at all.
    /// `version` is the buffer version the request describes, carried so a slow
    /// answer can be dropped once the buffer has moved on.
    PullDiagnostics {
        uri: String,
        request: i64,
        version: i64,
    },
    Shutdown,
}

/// Tracks the `initialize` / `initialized` handshake.
///
/// The protocol allows nothing but `initialized` to follow the `initialize`
/// response.  A server that receives `didOpen` first treats it as a protocol
/// violation and exits — rust-analyzer does exactly this, which is why it used
/// to die the instant a Rust file was opened.  Messages that arrive early are
/// held here and replayed once the handshake completes.
#[derive(Default)]
struct Handshake {
    complete: bool,
    queued: Vec<Outgoing>,
}

impl Handshake {
    /// Hold `message` until the handshake completes, or hand it back to send.
    fn accept(&mut self, message: Outgoing) -> Option<Outgoing> {
        if self.complete {
            Some(message)
        } else {
            self.queued.push(message);
            None
        }
    }

    /// Mark the handshake complete and return the messages to replay, in order.
    fn complete(&mut self) -> Vec<Outgoing> {
        self.complete = true;
        std::mem::take(&mut self.queued)
    }
}

/// A handle to a running server.
pub struct ClientHandle {
    pub language: String,
    /// The server program's name, for the status row ("rust-analyzer").
    pub server: String,
    pub root: PathBuf,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    failed: Arc<std::sync::atomic::AtomicBool>,
}

impl ClientHandle {
    fn new(
        language: String,
        server: String,
        root: PathBuf,
        outgoing: mpsc::UnboundedSender<Outgoing>,
        failed: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            language,
            server,
            root,
            outgoing,
            failed,
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn did_open(&self, uri: String, language: &str, text: String, version: i64) {
        let _ = self.outgoing.send(Outgoing::Open {
            uri,
            language: language.to_string(),
            text,
            version,
        });
    }

    pub fn did_change(&self, uri: String, text: String, version: i64) {
        let _ = self.outgoing.send(Outgoing::Change { uri, text, version });
    }

    /// Report a set of ranged edits (incremental sync).
    pub fn did_change_ranged(
        &self,
        uri: String,
        changes: Vec<crate::buffer::RecordedChange>,
        version: i64,
    ) {
        let _ = self.outgoing.send(Outgoing::RangedChange {
            uri,
            changes,
            version,
        });
    }

    pub fn did_save(&self, uri: String) {
        let _ = self.outgoing.send(Outgoing::Save { uri });
    }

    /// Ask for completions at a 0-based line/character position.
    pub fn complete(
        &self,
        uri: String,
        request: i64,
        line: usize,
        character: usize,
        trigger: Option<String>,
    ) {
        let _ = self.outgoing.send(Outgoing::Complete {
            uri,
            request,
            line,
            character,
            trigger,
        });
    }

    /// Ask what is at a 0-based line/character position.
    pub fn hover(&self, uri: String, request: i64, line: usize, character: usize) {
        let _ = self.outgoing.send(Outgoing::Hover {
            uri,
            request,
            line,
            character,
        });
    }

    /// Ask the server to format a whole document.
    pub fn format(&self, uri: String, request: i64, tab_size: usize, insert_spaces: bool) {
        let _ = self.outgoing.send(Outgoing::Format {
            uri,
            request,
            tab_size,
            insert_spaces,
        });
    }

    /// Ask the server to format one range.
    pub fn range_format(
        &self,
        uri: String,
        request: i64,
        start: (usize, usize),
        end: (usize, usize),
        tab_size: usize,
        insert_spaces: bool,
    ) {
        let _ = self.outgoing.send(Outgoing::RangeFormat {
            uri,
            request,
            start,
            end,
            tab_size,
            insert_spaces,
        });
    }

    /// Ask for quick fixes covering a range.
    pub fn code_action(
        &self,
        uri: String,
        request: i64,
        start: (usize, usize),
        end: (usize, usize),
        diagnostics: Vec<Value>,
    ) {
        let _ = self.outgoing.send(Outgoing::CodeAction {
            uri,
            request,
            start,
            end,
            diagnostics,
        });
    }

    /// Ask where the symbol at a 0-based line/character position is defined.
    pub fn definition(&self, uri: String, request: i64, line: usize, character: usize) {
        let _ = self.outgoing.send(Outgoing::Definition {
            uri,
            request,
            line,
            character,
        });
    }

    /// Ask for every location that mentions the symbol at a position.
    pub fn references(&self, uri: String, request: i64, line: usize, character: usize) {
        let _ = self.outgoing.send(Outgoing::References {
            uri,
            request,
            line,
            character,
        });
    }

    pub fn shutdown(&self) {
        let _ = self.outgoing.send(Outgoing::Shutdown);
    }

    /// Ask the server for its own analysis of a document.
    ///
    /// `version` is the buffer version the request describes; the answer is
    /// dropped by the editor if the buffer has moved on by the time it lands.
    pub fn pull_diagnostics(&self, uri: String, request: i64, version: i64) {
        let _ = self.outgoing.send(Outgoing::PullDiagnostics {
            uri,
            request,
            version,
        });
    }
}

/// Start a server process and drive one LSP session.
pub fn spawn_client(tx: EventTx, spec: ServerSpec, root: PathBuf, job: JobId) -> ClientHandle {
    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<Outgoing>();
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = ClientHandle::new(
        spec.language.clone(),
        spec.command.clone(),
        root.clone(),
        outgoing_tx,
        Arc::clone(&failed),
    );
    let language = spec.language.clone();
    tokio::spawn(async move {
        if let Err(error) = session(tx.clone(), spec, root, outgoing_rx).await {
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
            tx.send_async(AsyncEvent::Lsp(LspEvent::Failed { language, error }));
        }
        let _ = job;
    });
    handle
}

async fn session(
    tx: EventTx,
    spec: ServerSpec,
    root: PathBuf,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
) -> Result<(), String> {
    let mut child: Child = tokio::process::Command::new(&spec.command)
        .args(&spec.args)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("{}: {error}", spec.command))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "server has no stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "server has no stdout".to_string())?;
    let stdin = Arc::new(Mutex::new(stdin));

    // The initialize request; capabilities are deliberately minimal.
    let initialize = jsonrpc::request(
        1,
        "initialize",
        Some(json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "vortex", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": uri_for(&root),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": false },
                    "synchronization": {
                        // 1 = full, 2 = incremental.  This is what we ask for;
                        // the server answers with what it will actually do, and
                        // `server_incremental` below honours that answer.
                        "textDocumentSync": if spec.sync.is_incremental() { 2 } else { 1 },
                        "didSave": true,
                        "dynamicRegistration": false
                    },
                    "completion": {
                        "dynamicRegistration": false,
                        "completionItem": {
                            "snippetSupport": true,
                            "documentationFormat": ["plaintext", "markdown"]
                        },
                        "contextSupport": true
                    },
                    // Navigation: `textDocument/definition` and
                    // `textDocument/references` need no special client
                    // capabilities today, but declaring them is what tells a
                    // server the client actually asks.
                    "definition": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    // Pull diagnostics.  Declaring `diagnostic` here is not
                    // optional: rust-analyzer checks it before sending
                    // `workspace/diagnostic/refresh`.  Without it the only
                    // refresh is the one at startup, so an error typed into the
                    // buffer is never pulled and stays invisible until a save
                    // makes flycheck push it.
                    "diagnostic": { "dynamicRegistration": false }
                },
                "window": { "workDoneProgress": false },
                // Ask for the pull-diagnostics refresh notification.  The spec
                // gates `workspace/diagnostic/refresh` on this, and
                // rust-analyzer honours it: without it the server announces its
                // analysis once and never again as the buffer changes.
                "workspace": { "diagnostics": { "refreshSupport": true } }
            },
            "workspaceFolders": [{ "uri": uri_for(&root), "name": root.display().to_string() }],
            // rust-analyzer gates its own analysis behind this flag: without it
            // `textDocument/diagnostic` answers an empty set, so the only
            // diagnostics a client ever sees are the flycheck results for the
            // saved file.  Unsaved errors would then never appear.
            "initializationOptions": {
                "diagnostics": { "experimental": { "enable": true } }
            }
        })),
    );
    jsonrpc::write_message(&mut *stdin.lock().await, &initialize)
        .await
        .map_err(|error| error.to_string())?;

    let mut reader = BufReader::new(stdout);
    let mut handshake = Handshake::default();
    let next_id: i64 = 2;
    // Requests in flight, keyed by the id the answer will carry.  The kind is
    // remembered so the answer can be decoded as what it actually is.
    let mut pending: HashMap<i64, PendingRequest> = HashMap::new();
    // The text the server has been told about, per URI.  Completion ranges are
    // resolved against it, so they line up with what the server actually saw.
    let mut synced: HashMap<String, String> = HashMap::new();
    // What the server said it will accept.  A server may refuse incremental
    // sync even when asked for it, and sending ranges to one that did would
    // desync its copy of the document silently.
    let mut server_incremental = spec.sync.is_incremental();

    loop {
        tokio::select! {
            message = jsonrpc::read_message(&mut reader) => {
                match message.map_err(|error| error.to_string())? {
                    None => {
                        tx.send_async(AsyncEvent::Lsp(LspEvent::Closed {
                            language: spec.language.clone(),
                            code: None,
                        }));
                        return Ok(());
                    }
                    Some(value) => {
                        if value.get("id").is_some() && value.get("method").is_none() {
                            // A response to one of our requests.
                            if !handshake.complete {
                                // `textDocumentSync` is either a number or an
                                // object with a `change` field; both shapes are
                                // legal and servers use both.
                                let sync = value
                                    .pointer("/result/capabilities/textDocumentSync")
                                    .and_then(|sync| {
                                        sync.as_u64().or_else(|| {
                                            sync.get("change").and_then(Value::as_u64)
                                        })
                                    });
                                if let Some(kind) = sync {
                                    // 2 is incremental; anything else (1, or a
                                    // server that said nothing) means full.
                                    server_incremental = kind == 2;
                                }
                                let note = jsonrpc::notification("initialized", Some(json!({})));
                                let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
                                // Now that the handshake is complete, replay
                                // everything that arrived while it was running.
                                for message in handshake.complete() {
                                    send_outgoing(
                                        &stdin,
                                        message,
                                        &mut pending,
                                        &mut synced,
                                        server_incremental,
                                    )
                                    .await;
                                }
                                tx.send_async(AsyncEvent::Lsp(LspEvent::Ready {
                                    language: spec.language.clone(),
                                    root: root.clone(),
                                }));
                                continue;
                            }
                            let id = value.get("id").and_then(Value::as_i64);
                            if let Some(id) = id
                                && let Some(request) = pending.remove(&id)
                            {
                                // An error response has no `result`; each
                                // decoder treats a missing result as "nothing
                                // to offer" rather than as a failure.
                                let result = value.get("result").cloned().unwrap_or(Value::Null);
                                let error = value
                                    .pointer("/error/message")
                                    .and_then(Value::as_str)
                                    .map(str::to_string);
                                let text = synced.get(&request.uri).cloned().unwrap_or_default();
                                let starts = line_starts(&text);
                                let request_uri = request.uri;
                                let event = match request.kind {
                                    PendingKind::Completion => LspEvent::Completions {
                                        uri: request_uri,
                                        request: id,
                                        items: completion_items(&result, &text, &starts),
                                    },
                                    PendingKind::Hover => LspEvent::Hover {
                                        uri: request_uri,
                                        request: id,
                                        text: hover_text(&result),
                                    },
                                    PendingKind::Formatting | PendingKind::RangeFormatting => {
                                        LspEvent::Formatted {
                                            uri: request_uri,
                                            request: id,
                                            edits: text_edits(&result, &text, &starts),
                                            error,
                                        }
                                    }
                                    PendingKind::CodeAction => LspEvent::CodeActions {
                                        uri: request_uri,
                                        request: id,
                                        actions: code_actions(&result, &text, &starts),
                                    },
                                    PendingKind::Definition => {
                                        // A bare `Range` answer belongs to the
                                        // requesting document; the decoder
                                        // needs the URI before it is moved.
                                        let found = locations(&result, &request_uri);
                                        LspEvent::Definition {
                                            uri: request_uri,
                                            request: id,
                                            locations: found,
                                        }
                                    }
                                    PendingKind::References => {
                                        let found = locations(&result, &request_uri);
                                        LspEvent::References {
                                            uri: request_uri,
                                            request: id,
                                            locations: found,
                                        }
                                    }
                                    PendingKind::PullDiagnostics => {
                                        // `unchanged` carries no items: the
                                        // previous set still stands, so the
                                        // editor keeps what it has.  A `full`
                                        // report replaces the pulled set.
                                        let items = pull_diagnostic_items(&result);
                                        LspEvent::Diagnostics {
                                            uri: request_uri,
                                            version: request.version,
                                            items: items.clone().unwrap_or_default(),
                                            origin: DiagnosticOrigin::Pull,
                                            unchanged: items.is_none(),
                                        }
                                    }
                                };
                                tx.send_async(AsyncEvent::Lsp(event));
                            }
                            continue;
                        }
                        match value.get("method").and_then(Value::as_str) {
                            Some("textDocument/publishDiagnostics") => {
                                if let Some(event) = diagnostics_event(&value) {
                                    tx.send_async(AsyncEvent::Lsp(event));
                                }
                            }
                            // The server says its own diagnostics changed and
                            // wants them pulled again.  rust-analyzer sends
                            // this after it re-analyses an edited buffer, and
                            // it is the only signal that native diagnostics are
                            // ready — they are never pushed.  Answer the request
                            // so the server does not stall, then tell the app to
                            // pull for its open documents.
                            Some("workspace/diagnostic/refresh") => {
                                if let Some(id) = value.get("id") {
                                    let response = json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": Value::Null
                                    });
                                    let _ = jsonrpc::write_message(&mut *stdin.lock().await, &response).await;
                                }
                                tx.send_async(AsyncEvent::Lsp(LspEvent::DiagnosticsRefresh {
                                    language: spec.language.clone(),
                                }));
                            }
                            Some("window/showMessage") | Some("window/logMessage") => {
                                let message = value
                                    .pointer("/params/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                tx.send_async(AsyncEvent::Lsp(LspEvent::Progress {
                                    language: spec.language.clone(),
                                    message,
                                }));
                            }
                            Some(_) => {
                                // Server-to-client requests need answering with an
                                // empty result so the server does not stall.
                                if let Some(id) = value.get("id") {
                                    let response = json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": Value::Null
                                    });
                                    let _ = jsonrpc::write_message(&mut *stdin.lock().await, &response).await;
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
            outgoing = outgoing.recv() => {
                match outgoing {
                    None | Some(Outgoing::Shutdown) => {
                        let shutdown = jsonrpc::request(next_id, "shutdown", None);
                        let _ = jsonrpc::write_message(&mut *stdin.lock().await, &shutdown).await;
                        let exit = jsonrpc::notification("exit", None);
                        let _ = jsonrpc::write_message(&mut *stdin.lock().await, &exit).await;
                        let _ = child.start_kill();
                        return Ok(());
                    }
                    Some(message) => {
                        // Nothing but `initialized` may follow the `initialize`
                        // response.  A server that sees `didOpen` first treats it
                        // as a protocol violation and exits, so hold everything
                        // until the handshake is done.
                        if let Some(message) = handshake.accept(message) {
                            send_outgoing(
                                &stdin,
                                message,
                                &mut pending,
                                &mut synced,
                                server_incremental,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }
}

/// Apply recorded changes to the text the server has been told about.
///
/// The stored copy has to track the server's view: completion ranges are
/// resolved against it, so a stale copy puts every later answer on the wrong
/// text.  Positions are line/UTF-16-column, so each is converted to a byte
/// offset in the *current* copy before the edit is applied — the same order the
/// server applies them in.
fn apply_recorded(text: &mut String, changes: &[crate::buffer::RecordedChange]) {
    for change in changes {
        let start = byte_at_utf16_position(text, change.start);
        let end = byte_at_utf16_position(text, change.end);
        let end = end.max(start);
        if start <= text.len() && end <= text.len() {
            text.replace_range(start..end, &change.text);
        }
    }
}

/// Byte offset of an LSP `(line, UTF-16 column)` position in `text`.
fn byte_at_utf16_position(text: &str, position: (usize, usize)) -> usize {
    let (line, column) = position;
    let mut offset = 0usize;
    for _ in 0..line {
        match text[offset..].find('\n') {
            Some(index) => offset += index + 1,
            None => return text.len(),
        }
    }
    let rest = &text[offset..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let line_text = &rest[..line_end];
    offset + crate::buffer::utf16_col_to_byte(line_text, column)
}

/// Write one queued message to the server.
///
/// Shared by the live path and the post-handshake flush so the two cannot drift
/// apart — a message that is only correct when sent live would be a bug that
/// shows up exactly once, at startup.
async fn send_outgoing(
    stdin: &Arc<Mutex<ChildStdin>>,
    message: Outgoing,
    pending: &mut HashMap<i64, PendingRequest>,
    synced: &mut HashMap<String, String>,
    server_incremental: bool,
) {
    match message {
        Outgoing::Open {
            uri,
            language,
            text,
            version,
        } => {
            let note = jsonrpc::notification(
                "textDocument/didOpen",
                Some(json!({ "textDocument": {
                    "uri": uri, "languageId": language, "version": version, "text": text
                }})),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
            synced.insert(uri, text);
        }
        Outgoing::Change { uri, text, version } => {
            // Whole-buffer sync: simplest correct thing to send.
            let note = jsonrpc::notification(
                "textDocument/didChange",
                Some(json!({ "textDocument": { "uri": uri, "version": version },
                              "contentChanges": [{ "text": text }] })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
            synced.insert(uri, text);
        }
        Outgoing::RangedChange {
            uri,
            changes,
            version,
        } => {
            // The stored copy is the only place the full text exists here, so
            // it is updated first: the fallback below needs the post-edit text,
            // and completion ranges are resolved against it either way.
            let full = match synced.get_mut(&uri) {
                Some(text) => {
                    apply_recorded(text, &changes);
                    Some(text.clone())
                }
                None => None,
            };
            if !server_incremental {
                // The server refused incremental sync even though we asked for
                // it.  Sending ranges to it would desync its copy silently, so
                // the whole document goes instead.
                if let Some(text) = full {
                    let note = jsonrpc::notification(
                        "textDocument/didChange",
                        Some(json!({ "textDocument": { "uri": uri, "version": version },
                                      "contentChanges": [{ "text": text }] })),
                    );
                    let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
                }
                return;
            }
            // Incremental sync: one ranged entry per edit, in the order they
            // were applied.  Each entry is applied to the document the previous
            // one produced, which is exactly how the buffer recorded them.
            let content: Vec<Value> = changes
                .iter()
                .map(|change| {
                    json!({
                        "range": {
                            "start": { "line": change.start.0, "character": change.start.1 },
                            "end": { "line": change.end.0, "character": change.end.1 }
                        },
                        "text": change.text
                    })
                })
                .collect();
            let note = jsonrpc::notification(
                "textDocument/didChange",
                Some(json!({ "textDocument": { "uri": uri, "version": version },
                              "contentChanges": content })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
        }
        Outgoing::Save { uri } => {
            let note = jsonrpc::notification(
                "textDocument/didSave",
                Some(json!({ "textDocument": { "uri": uri } })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
        }
        Outgoing::Complete {
            uri,
            request,
            line,
            character,
            trigger,
        } => {
            // The app's request id doubles as the JSON-RPC id, so the answer can
            // be matched to the caret that asked for it.  A request that answers
            // a typed `:` or `.` reports it as a trigger character (kind 2):
            // servers key member and path lists off that context.
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::Completion,
                    uri: uri.clone(),
                    version: None,
                },
            );
            let context = match trigger {
                Some(character) => json!({ "triggerKind": 2, "triggerCharacter": character }),
                None => json!({ "triggerKind": 1 }),
            };
            let request_message = jsonrpc::request(
                request,
                "textDocument/completion",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": context
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::Hover {
            uri,
            request,
            line,
            character,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::Hover,
                    uri: uri.clone(),
                    version: None,
                },
            );
            let request_message = jsonrpc::request(
                request,
                "textDocument/hover",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::Definition {
            uri,
            request,
            line,
            character,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::Definition,
                    uri: uri.clone(),
                    version: None,
                },
            );
            let request_message = jsonrpc::request(
                request,
                "textDocument/definition",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::References {
            uri,
            request,
            line,
            character,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::References,
                    uri: uri.clone(),
                    version: None,
                },
            );
            // `includeDeclaration` asks for the definition row too: a reader
            // who wants the full picture of a symbol expects its declaration
            // among the hits, and servers that ignore the flag answer the same.
            let request_message = jsonrpc::request(
                request,
                "textDocument/references",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": { "includeDeclaration": true }
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::Format {
            uri,
            request,
            tab_size,
            insert_spaces,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::Formatting,
                    uri: uri.clone(),
                    version: None,
                },
            );
            let request_message = jsonrpc::request(
                request,
                "textDocument/formatting",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "options": { "tabSize": tab_size, "insertSpaces": insert_spaces }
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::RangeFormat {
            uri,
            request,
            start,
            end,
            tab_size,
            insert_spaces,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::RangeFormatting,
                    uri: uri.clone(),
                    version: None,
                },
            );
            let request_message = jsonrpc::request(
                request,
                "textDocument/rangeFormatting",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 }
                    },
                    "options": { "tabSize": tab_size, "insertSpaces": insert_spaces }
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::CodeAction {
            uri,
            request,
            start,
            end,
            diagnostics,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::CodeAction,
                    uri: uri.clone(),
                    version: None,
                },
            );
            let request_message = jsonrpc::request(
                request,
                "textDocument/codeAction",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 }
                    },
                    // Only quick fixes: refactors and source actions need a
                    // picker of their own and are not what a diagnostic offers.
                    // The diagnostics are what the server keys its fixes off,
                    // so an empty list here means an empty answer.
                    "context": { "diagnostics": diagnostics, "only": ["quickfix"] }
                })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        Outgoing::PullDiagnostics {
            uri,
            request,
            version,
        } => {
            pending.insert(
                request,
                PendingRequest {
                    kind: PendingKind::PullDiagnostics,
                    uri: uri.clone(),
                    version: Some(version),
                },
            );
            let request_message = jsonrpc::request(
                request,
                "textDocument/diagnostic",
                Some(json!({ "textDocument": { "uri": uri } })),
            );
            let _ = jsonrpc::write_message(&mut *stdin.lock().await, &request_message).await;
        }
        // Shutdown is handled by the caller, which also owns the child.
        Outgoing::Shutdown => {}
    }
}

/// One client per project root.
#[derive(Default)]
pub struct Clients {
    by_root: HashMap<PathBuf, ClientHandle>,
}

impl Clients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_root.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty()
    }

    /// The client for `root`, starting one when needed.
    pub fn ensure(
        &mut self,
        tx: &EventTx,
        spec: &ServerSpec,
        root: &Path,
    ) -> Option<&mut ClientHandle> {
        let live = self
            .by_root
            .get(root)
            .is_some_and(|client| client.is_alive() && client.language == spec.language);
        if !live {
            let client = spawn_client(tx.clone(), spec.clone(), root.to_path_buf(), JobId::new(0));
            self.by_root.insert(root.to_path_buf(), client);
        }
        self.by_root.get_mut(root)
    }

    pub fn get(&mut self, root: &Path) -> Option<&mut ClientHandle> {
        self.by_root.get_mut(root)
    }

    /// The server program attached for `language`, if a live client serves it.
    pub fn attached(&self, language: &str) -> Option<&str> {
        self.by_root
            .values()
            .find(|client| client.is_alive() && client.language == language)
            .map(|client| client.server.as_str())
    }

    pub fn shutdown_all(&mut self) {
        for (_, client) in self.by_root.drain() {
            client.shutdown();
        }
    }
}

/// Every buffer a client should be told about, keyed by URI.
pub fn uri_of(path: &Path) -> String {
    uri_for(path)
}

/// Line start offsets of a text, for [`range_to_span`].
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// The buffer a diagnostic event refers to, when it can be matched.
pub fn buffer_for_path<'a, T: 'a>(
    mut buffers: impl Iterator<Item = (BufferId, &'a T)>,
    paths: impl Fn(&T) -> Option<&'a Path>,
    path: &Path,
) -> Option<BufferId> {
    buffers
        .find(|(_, buffer)| paths(*buffer) == Some(path))
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let path = Path::new("/tmp/example/main.rs");
        let uri = uri_for(path);
        assert!(uri.starts_with("file://"), "{uri}");
        assert_eq!(path_for_uri(&uri).as_deref(), Some(path));
        assert_eq!(path_for_uri("http://example.com"), None);
    }

    #[test]
    fn decodes_locations_in_every_legal_shape() {
        let fallback = "file:///p/src/app.rs";
        // A references result: an array of plain Locations.
        let array = json!([
            { "uri": "file:///p/src/app.rs", "range": { "start": { "line": 9, "character": 4 }, "end": { "line": 9, "character": 8 } } },
            { "uri": "file:///p/src/lib.rs", "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 3, "character": 1 } } },
        ]);
        assert_eq!(
            locations(&array, fallback),
            vec![
                Location {
                    uri: "file:///p/src/app.rs".into(),
                    line: 9,
                    character: 4
                },
                Location {
                    uri: "file:///p/src/lib.rs".into(),
                    line: 1,
                    character: 0
                },
            ]
        );
        // A single Location, not wrapped in an array.
        let single = array.get(0).cloned().unwrap();
        assert_eq!(locations(&single, fallback).len(), 1);
        // A LocationLink: the selection range beats the covering range.
        let link = json!({
            "targetUri": "file:///p/src/other.rs",
            "targetRange": { "start": { "line": 20, "character": 0 }, "end": { "line": 24, "character": 1 } },
            "targetSelectionRange": { "start": { "line": 21, "character": 7 }, "end": { "line": 21, "character": 12 } },
        });
        assert_eq!(
            locations(&json!([link]), fallback),
            vec![Location {
                uri: "file:///p/src/other.rs".into(),
                line: 21,
                character: 7
            }]
        );
        // A bare Range belongs to the document that asked.
        let bare =
            json!({ "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 6 } });
        assert_eq!(
            locations(&bare, fallback),
            vec![Location {
                uri: fallback.into(),
                line: 3,
                character: 2
            }]
        );
        // Nothing found is an empty list, not a failure.
        assert!(locations(&Value::Null, fallback).is_empty());
        assert!(locations(&json!([]), fallback).is_empty());
        assert!(locations(&json!([{}]), fallback).is_empty());
    }

    #[test]
    fn ranges_map_to_byte_offsets() {
        let text = "first\nsecond line\nthird\n";
        let starts = line_starts(text);
        let span = range_to_span(text, &starts, (1, 7), (1, 11));
        assert_eq!((&text[span.start..span.end]), "line");
    }

    #[test]
    fn ranges_are_clamped_to_the_buffer() {
        let text = "short\n";
        let starts = line_starts(text);
        let span = range_to_span(text, &starts, (0, 2), (9, 40));
        assert_eq!(span.start, 2);
        assert_eq!(span.end, text.len());
    }

    #[test]
    fn lsp_columns_are_utf16_code_units() {
        // `é` is one UTF-16 unit but two bytes; `😀` is two units and four
        // bytes.  A column counted in bytes would land in the wrong place.
        let text = "let é = 1;\nlet 😀 = 2;\n";
        let starts = line_starts(text);
        // `1` sits at UTF-16 column 8 on line 0.
        let span = range_to_span(text, &starts, (0, 8), (0, 9));
        assert_eq!(&text[span.start..span.end], "1");
        // On line 1 the emoji occupies columns 4..6, so `2` is at column 9.
        let span = range_to_span(text, &starts, (1, 9), (1, 10));
        assert_eq!(&text[span.start..span.end], "2");
    }

    fn buffer_with(text: &str) -> Buffer {
        Buffer::new(
            BufferId(1),
            None,
            text.to_string(),
            &crate::config::EditorConfig::default(),
        )
    }

    #[test]
    fn a_publish_diagnostics_notification_yields_an_event() {
        // The real message is a JSON-RPC envelope: the payload is under
        // `params`.  Reading `diagnostics` off the envelope itself found
        // nothing, so every diagnostic was silently dropped in a live session
        // while the params-level unit tests below kept passing.
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/x.rs",
                "version": 7,
                "diagnostics": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "severity": 1,
                    "message": "cannot find value `foo`",
                    "source": "rust-analyzer"
                }]
            }
        });

        let Some(LspEvent::Diagnostics {
            uri,
            version,
            items,
            origin,
            unchanged,
        }) = diagnostics_event(&notification)
        else {
            panic!("no diagnostics event from a publish notification");
        };
        assert_eq!(uri, "file:///tmp/x.rs");
        assert_eq!(version, Some(7));
        assert_eq!(origin, DiagnosticOrigin::Push);
        assert!(!unchanged, "a push is never an unchanged report");
        assert_eq!(items.len(), 1, "the diagnostic must survive the envelope");
        assert_eq!(items[0].message, "cannot find value `foo`");
        assert_eq!(items[0].severity, Severity::Error);
    }

    #[test]
    fn a_notification_without_params_yields_nothing() {
        // A malformed notification must not panic the session loop.
        assert!(diagnostics_event(&json!({ "method": "x" })).is_none());
    }

    #[test]
    fn diagnostics_decode_then_map_to_bytes() {
        let text = "let x = ;\nsecond line\n";
        let buffer = buffer_with(text);
        let params = json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 8 }, "end": { "line": 0, "character": 9 } },
                "severity": 1,
                "message": "expected expression",
                "source": "rust-analyzer"
            }, {
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 6 } },
                "severity": 2,
                "message": "unused variable",
                "source": "rust-analyzer"
            }]
        });
        let items = diagnostic_items(&params);
        assert_eq!(items.len(), 2);
        let diagnostics: Vec<Diagnostic> = items
            .iter()
            .map(|item| item.to_diagnostic(&buffer, DiagnosticSource::Lsp))
            .collect();
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert!(diagnostics[0].message.contains("expected expression"));
        assert_eq!(diagnostics[0].range, Span::new(8, 9));
        assert_eq!(diagnostics[1].range, Span::new(10, 16));
        assert_eq!(diagnostics[1].severity, Severity::Warning);
    }

    #[test]
    fn a_diagnostic_carries_the_source_it_is_stored_under() {
        // `set_diagnostics` replaces a buffer's diagnostics by matching on the
        // source it is given.  A pulled item stored under the pushed source was
        // never retained away, so every pull appended to the pile and errors
        // accumulated as the user typed — only a save (a fresh pushed set) wiped
        // them.  The source has to travel with the item.
        let buffer = buffer_with("let x = 1;\n");
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } },
                "severity": 1,
                "message": "problem",
                "source": "rust-analyzer"
            }]
        });
        let item = &diagnostic_items(&params)[0];
        assert_eq!(
            item.to_diagnostic(&buffer, DiagnosticSource::LspPull)
                .source,
            DiagnosticSource::LspPull
        );
        assert_eq!(
            item.to_diagnostic(&buffer, DiagnosticSource::Lsp).source,
            DiagnosticSource::Lsp
        );
    }

    #[test]
    fn a_pull_report_decodes_like_a_push_but_carries_its_own_origin() {
        // The answer to `textDocument/diagnostic` is a report with an `items`
        // array, not a `diagnostics` array.  Decoding it through the push
        // decoder found nothing, which is exactly how an unsaved error would
        // stay invisible.
        let result = json!({
            "kind": "full",
            "items": [{
                "range": { "start": { "line": 1, "character": 12 }, "end": { "line": 1, "character": 26 } },
                "severity": 1,
                "message": "no such value in this scope",
                "source": "rust-analyzer"
            }]
        });
        let items = pull_diagnostic_items(&result).expect("a full report is a whole set");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "no such value in this scope");
        assert_eq!(items[0].severity, Severity::Error);
    }

    #[test]
    fn an_unchanged_pull_report_is_not_mistaken_for_clean() {
        // `unchanged` means the previous set still stands.  Treating its empty
        // `items` as "nothing wrong" would erase live errors on every no-op
        // refresh.
        let result = json!({ "kind": "unchanged", "resultId": "rust-analyzer" });
        assert!(
            pull_diagnostic_items(&result).is_none(),
            "an unchanged report must leave the existing set alone"
        );
    }

    #[test]
    fn diagnostic_code_tags_and_related_information_survive_the_decode() {
        let text = "let unused = 1;\nlet other = 2;\n";
        let buffer = buffer_with(text);
        let params = json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 10 } },
                "severity": 2,
                "code": "unused_variables",
                "tags": [1],
                "message": "unused variable: `unused`",
                "source": "rust-analyzer",
                "relatedInformation": [{
                    "location": {
                        "uri": "file:///tmp/x.rs",
                        "range": {
                            "start": { "line": 1, "character": 4 },
                            "end": { "line": 1, "character": 9 }
                        }
                    },
                    "message": "`other` is used here"
                }]
            }, {
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 3 } },
                "severity": 1,
                "code": 1234,
                "tags": [2],
                "message": "deprecated",
                "source": "rustc"
            }]
        });
        let items = diagnostic_items(&params);
        let diagnostics: Vec<Diagnostic> = items
            .iter()
            .map(|item| item.to_diagnostic(&buffer, DiagnosticSource::Lsp))
            .collect();

        assert_eq!(diagnostics[0].code.as_deref(), Some("unused_variables"));
        assert!(diagnostics[0].has_tag(DiagnosticTag::Unnecessary));
        assert!(!diagnostics[0].has_tag(DiagnosticTag::Deprecated));
        assert_eq!(diagnostics[0].related.len(), 1);
        assert_eq!(diagnostics[0].related[0].0, "`other` is used here");
        assert_eq!(diagnostics[0].related[0].1, Span::new(20, 25));

        // A numeric `code` is stringified rather than dropped.
        assert_eq!(diagnostics[1].code.as_deref(), Some("1234"));
        assert!(diagnostics[1].has_tag(DiagnosticTag::Deprecated));
        assert!(diagnostics[1].related.is_empty());
    }

    #[test]
    fn unknown_tags_are_ignored_rather_than_failing_the_decode() {
        let text = "x\n";
        let buffer = buffer_with(text);
        let params = json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "severity": 1,
                "tags": [1, 99],
                "message": "x",
                "source": "lsp"
            }]
        });
        let diagnostics: Vec<Diagnostic> = diagnostic_items(&params)
            .iter()
            .map(|item| item.to_diagnostic(&buffer, DiagnosticSource::Lsp))
            .collect();
        assert_eq!(diagnostics[0].tags, vec![DiagnosticTag::Unnecessary]);
    }

    #[test]
    fn servers_come_from_config_when_enabled() {
        let mut config = Config::default();
        assert!(
            server_for("rust", &config).is_none(),
            "lsp is off by default"
        );
        config.lsp.enable = true;
        config.lsp.servers.insert(
            "rust".into(),
            crate::config::LspServer {
                command: "my-rust-analyzer".into(),
                args: vec!["--stdio".into()],
                languages: Vec::new(),
                enable: true,
            },
        );
        let spec = server_for("rust", &config).expect("configured server");
        assert_eq!(spec.command, "my-rust-analyzer");
        assert_eq!(spec.args, ["--stdio"]);

        config.lsp.servers.insert(
            "python".into(),
            crate::config::LspServer {
                command: "pylsp".into(),
                args: Vec::new(),
                languages: Vec::new(),
                enable: false,
            },
        );
        assert!(server_for("python", &config).is_none());
    }

    #[test]
    fn disabled_or_blank_servers_are_skipped() {
        let mut config = Config::default();
        config.lsp.enable = true;
        config.lsp.servers.insert(
            "go".into(),
            crate::config::LspServer {
                command: "   ".into(),
                args: Vec::new(),
                languages: Vec::new(),
                enable: true,
            },
        );
        assert!(server_for("go", &config).is_none());
    }

    #[test]
    fn completion_items_decode_from_a_bare_array() {
        let text = "let value = pri\n";
        let starts = line_starts(text);
        let result = json!([
            { "label": "print", "kind": 3, "detail": "fn(&str)" },
            { "label": "println", "insertText": "println!" }
        ]);
        let items = completion_items(&result, text, &starts);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "print");
        assert_eq!(items[0].kind_label(), "fn");
        assert_eq!(items[0].detail.as_deref(), Some("fn(&str)"));
        assert_eq!(
            items[0].text(),
            "print",
            "no insertText falls back to label"
        );
        assert_eq!(items[1].text(), "println!", "insertText wins");
    }

    #[test]
    fn completion_items_decode_from_a_completion_list() {
        let text = "value.\n";
        let starts = line_starts(text);
        let result = json!({
            "isIncomplete": false,
            "items": [{ "label": "len", "kind": 2 }]
        });
        let items = completion_items(&result, text, &starts);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "len");
    }

    #[test]
    fn a_completion_text_edit_becomes_a_byte_span() {
        let text = "let value = pri\n";
        let starts = line_starts(text);
        let result = json!([{
            "label": "print",
            "textEdit": {
                "range": {
                    "start": { "line": 0, "character": 12 },
                    "end": { "line": 0, "character": 15 }
                },
                "newText": "print"
            }
        }]);
        let items = completion_items(&result, text, &starts);
        let span = items[0].replace.expect("a replace range");
        assert_eq!(&text[span.start..span.end], "pri");
    }

    #[test]
    fn a_text_edit_new_text_wins_over_insert_text() {
        // The `textEdit` is the server's exact instruction; `insertText` may
        // disagree (rust-analyzer sends paths and call parens only in one).
        let text = "use std::\n";
        let starts = line_starts(text);
        let result = json!([{
            "label": "io",
            "insertText": "io",
            "textEdit": {
                "range": {
                    "start": { "line": 0, "character": 9 },
                    "end": { "line": 0, "character": 9 }
                },
                "newText": "std::io"
            }
        }]);
        let items = completion_items(&result, text, &starts);
        assert_eq!(items[0].text(), "std::io", "the newText is authoritative");
    }

    #[test]
    fn a_snippet_format_marks_the_item() {
        let text = "x\n";
        let starts = line_starts(text);
        let result = json!([
            { "label": "main()", "insertTextFormat": 2, "insertText": "main($0)" },
            { "label": "plain", "insertTextFormat": 1, "insertText": "plain" }
        ]);
        let items = completion_items(&result, text, &starts);
        assert!(items[0].snippet, "insertTextFormat 2 is a snippet");
        assert!(!items[1].snippet);
    }

    #[test]
    fn an_empty_or_error_result_offers_nothing() {
        let text = "x\n";
        let starts = line_starts(text);
        assert!(completion_items(&Value::Null, text, &starts).is_empty());
        assert!(completion_items(&json!({ "items": [] }), text, &starts).is_empty());
    }

    #[test]
    fn hover_text_decodes_every_contents_shape() {
        // A plain string.
        assert_eq!(hover_text(&json!({ "contents": "fn main()" })), "fn main()");
        // A `MarkupContent` object.
        assert_eq!(
            hover_text(&json!({ "contents": { "kind": "plaintext", "value": "let x: u8" } })),
            "let x: u8"
        );
        // An array of both, joined in order.
        let result = json!({
            "contents": [
                { "language": "rust", "value": "fn f()" },
                "does a thing"
            ]
        });
        assert_eq!(hover_text(&result), "fn f()\ndoes a thing");
        // Nothing at all.
        assert_eq!(hover_text(&Value::Null), "");
    }

    #[test]
    fn hover_markdown_is_flattened_to_plain_text() {
        let result = json!({
            "contents": {
                "kind": "markdown",
                "value": "```rust\nfn f() -> u8\n```\n\n**Docs** with `code`."
            }
        });
        let text = hover_text(&result);
        assert!(text.contains("fn f() -> u8"), "{text:?}");
        assert!(text.contains("Docs with code."), "{text:?}");
        assert!(!text.contains('`'), "backticks are stripped: {text:?}");
        assert!(
            !text.contains("**"),
            "emphasis markers are stripped: {text:?}"
        );
    }

    #[test]
    fn formatting_edits_become_byte_spans() {
        let text = "fn  main() {}\n";
        let starts = line_starts(text);
        let result = json!([{
            "range": {
                "start": { "line": 0, "character": 2 },
                "end": { "line": 0, "character": 4 }
            },
            "newText": " "
        }]);
        let edits = text_edits(&result, text, &starts);
        assert_eq!(edits.len(), 1);
        assert_eq!(&text[edits[0].range.start..edits[0].range.end], "  ");
        assert_eq!(edits[0].new_text, " ");
        // `null` means "nothing to change", not an error.
        assert!(text_edits(&Value::Null, text, &starts).is_empty());
    }

    #[test]
    fn code_actions_keep_only_the_ones_with_edits() {
        let text = "let unused = 1;\n";
        let starts = line_starts(text);
        let result = json!([
            {
                "title": "remove unused binding",
                "kind": "quickfix",
                "edit": {
                    "changes": {
                        "file:///tmp/x.rs": [{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 15 }
                            },
                            "newText": ""
                        }]
                    }
                }
            },
            // A bare `Command` has no edits and cannot be applied here.
            { "title": "run a command", "command": "something" }
        ]);
        let actions = code_actions(&result, text, &starts);
        assert_eq!(actions.len(), 1, "the command-only action is skipped");
        assert_eq!(actions[0].title, "remove unused binding");
        assert_eq!(actions[0].edits.len(), 1);
        assert_eq!(actions[0].edits[0].new_text, "");
    }

    #[test]
    fn a_diagnostic_is_encoded_as_utf16_positions() {
        // `é` is one UTF-16 unit but two bytes, so a byte column would be wrong.
        let buffer = buffer_with("let é = 1;\n");
        let diagnostic = Diagnostic::new(
            Span::new(4, 6),
            Severity::Warning,
            "unused",
            DiagnosticSource::Lsp,
        )
        .with_code(Some("unused_variables".into()));
        let value = diagnostic_json(&buffer, &diagnostic);
        assert_eq!(value["range"]["start"]["line"], 0);
        assert_eq!(value["range"]["start"]["character"], 4);
        assert_eq!(value["range"]["end"]["character"], 5, "one UTF-16 unit");
        assert_eq!(value["severity"], 2, "warning is LSP severity 2");
        assert_eq!(value["message"], "unused");
        assert_eq!(value["code"], "unused_variables");
    }

    #[test]
    fn a_diagnostic_without_a_code_omits_it() {
        let buffer = buffer_with("x\n");
        let diagnostic = Diagnostic::new(
            Span::new(0, 1),
            Severity::Error,
            "boom",
            DiagnosticSource::Lsp,
        );
        let value = diagnostic_json(&buffer, &diagnostic);
        assert!(value.get("code").is_none(), "{value}");
        assert_eq!(value["severity"], 1, "error is LSP severity 1");
    }

    #[test]
    fn recorded_changes_replay_onto_the_synced_copy() {
        // The stored copy has to track the server's view: completion ranges are
        // resolved against it, so a stale copy puts later answers on wrong text.
        let mut text = String::from("fn  main() {}\n");
        let changes = vec![crate::buffer::RecordedChange {
            start: (0, 2),
            end: (0, 4),
            text: " ".into(),
        }];
        apply_recorded(&mut text, &changes);
        assert_eq!(text, "fn main() {}\n");
    }

    #[test]
    fn recorded_changes_apply_in_order() {
        // Each entry is applied to the document the previous one produced, so a
        // later range is expressed against the already-edited text.
        let mut text = String::from("abc\n");
        let changes = vec![
            crate::buffer::RecordedChange {
                start: (0, 0),
                end: (0, 0),
                text: "X".into(),
            },
            crate::buffer::RecordedChange {
                start: (0, 4),
                end: (0, 4),
                text: "Y".into(),
            },
        ];
        apply_recorded(&mut text, &changes);
        assert_eq!(text, "XabcY\n");
    }

    #[test]
    fn a_recorded_position_counts_utf16_units() {
        // `é` is one UTF-16 unit but two bytes; `😀` is two units and four
        // bytes.  A byte column would land in the wrong place.
        let text = "let é = 1;\nlet 😀 = 2;\n";
        assert_eq!(byte_at_utf16_position(text, (0, 4)), 4);
        assert_eq!(byte_at_utf16_position(text, (0, 5)), 6, "after the é");
        // Line 1 starts after the first newline.
        let line1 = text.find('\n').expect("a newline") + 1;
        assert_eq!(byte_at_utf16_position(text, (1, 0)), line1);
        assert_eq!(byte_at_utf16_position(text, (1, 4)), line1 + 4);
        // The emoji is two units, so column 6 is just past it.
        assert_eq!(byte_at_utf16_position(text, (1, 6)), line1 + 8);
    }

    #[test]
    fn a_position_past_the_end_clamps() {
        let text = "short\n";
        assert_eq!(byte_at_utf16_position(text, (0, 99)), 5);
        assert_eq!(byte_at_utf16_position(text, (99, 0)), text.len());
    }

    fn open(uri: &str) -> Outgoing {
        Outgoing::Open {
            uri: uri.to_string(),
            language: "rust".into(),
            text: String::new(),
            version: 1,
        }
    }

    #[test]
    fn messages_before_the_handshake_are_held_and_replayed_in_order() {
        // rust-analyzer exits with "expected initialized notification, got
        // didOpen" if a document message beats the handshake.  Everything that
        // arrives early has to wait, and then go out in the order it came.
        let mut handshake = Handshake::default();
        assert!(handshake.accept(open("file:///a.rs")).is_none());
        assert!(handshake.accept(open("file:///b.rs")).is_none());

        let replayed = handshake.complete();
        assert_eq!(replayed.len(), 2);
        match (&replayed[0], &replayed[1]) {
            (Outgoing::Open { uri: first, .. }, Outgoing::Open { uri: second, .. }) => {
                assert_eq!(first, "file:///a.rs");
                assert_eq!(second, "file:///b.rs");
            }
            other => panic!("expected two opens, got {other:?}"),
        }
    }

    #[test]
    fn messages_after_the_handshake_go_straight_out() {
        let mut handshake = Handshake::default();
        handshake.complete();
        assert!(handshake.accept(open("file:///a.rs")).is_some());
        assert!(handshake.complete().is_empty(), "nothing was held back");
    }
}
