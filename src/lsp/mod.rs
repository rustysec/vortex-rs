//! Language server protocol support — a working scaffold, not a full client.
//!
//! What works today:
//! * one server process per project root, started on demand from `[lsp.servers]`;
//! * the `initialize` / `initialized` handshake and `textDocument/didOpen` /
//!   `didChange` / `didSave` for buffers in that root;
//! * `textDocument/publishDiagnostics`, turned into [`crate::diagnostic::Diagnostic`]
//!   values that the status area and `ui` render like spell-check hints.
//!
//! What is deliberately not implemented: requests that need an answer to be
//! useful (hover, completion, code actions, formatting).  Those need a request
//! multiplexer and a UI affordance per feature; formatting therefore still goes
//! through `[format.command]`, which keeps the editor usable without a server.
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
use tokio::process::Child;
use tokio::sync::{Mutex, mpsc};

use crate::buffer::BufferId;
use crate::config::Config;
use crate::diagnostic::{Diagnostic, DiagnosticSource, Severity};
use crate::event::{AsyncEvent, EventTx, JobId};
use crate::selection::Span;

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
    /// buffer, so it converts them to byte offsets.
    Diagnostics {
        uri: String,
        items: Vec<DiagnosticItem>,
    },
    /// A progress or status message worth showing in the status area.
    Progress { language: String, message: String },
    /// A server exited; it will be restarted the next time it is needed.
    Closed { language: String, code: Option<i32> },
}

/// A server we know how to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub language: String,
    pub command: String,
    pub args: Vec<String>,
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
}

impl DiagnosticItem {
    /// Resolve to buffer byte offsets using the buffer's line table.
    pub fn to_diagnostic(&self, line_starts: &[usize], content_len: usize) -> Diagnostic {
        Diagnostic::new(
            range_to_span(
                line_starts,
                content_len,
                (self.range.0, self.range.1),
                (self.range.2, self.range.3),
            ),
            self.severity,
            format!("{}: {}", self.source, self.message),
            DiagnosticSource::Lsp,
        )
    }
}

/// Decode `publishDiagnostics` parameters.
pub fn diagnostic_items(params: &Value) -> Vec<DiagnosticItem> {
    let Some(items) = params.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let start = position(item.pointer("/range/start")?)?;
            let end = position(item.pointer("/range/end")?)?;
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
            })
        })
        .collect()
}

/// Turn an LSP `{start,end}{line,character}` range into a byte span.
///
/// `line_starts` are the buffer's line start offsets; `character` counts UTF-16
/// code units in LSP, which is close enough for ASCII source and clamped
/// otherwise.
pub fn range_to_span(
    line_starts: &[usize],
    content_len: usize,
    start: (usize, usize),
    end: (usize, usize),
) -> Span {
    let line_start = |line: usize| {
        *line_starts
            .get(line.min(line_starts.len().saturating_sub(1)))
            .unwrap_or(&0)
    };
    let start_byte = line_start(start.0) + start.1.min(4096);
    let end_byte = line_start(end.0) + end.1.min(4096);
    Span::new(
        start_byte.min(content_len),
        end_byte.clamp(start_byte.min(content_len), content_len),
    )
}

fn position(value: &Value) -> Option<(usize, usize)> {
    Some((
        value.get("line")?.as_u64()? as usize,
        value.get("character")?.as_u64()? as usize,
    ))
}

/// Outgoing messages the app can ask a running server to send.
enum Outgoing {
    Open {
        uri: String,
        language: String,
        text: String,
    },
    Change {
        uri: String,
        text: String,
        version: i64,
    },
    Save {
        uri: String,
    },
    Shutdown,
}

/// A handle to a running server.
pub struct ClientHandle {
    pub language: String,
    pub root: PathBuf,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    failed: Arc<std::sync::atomic::AtomicBool>,
}

impl ClientHandle {
    fn new(
        language: String,
        root: PathBuf,
        outgoing: mpsc::UnboundedSender<Outgoing>,
        failed: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            language,
            root,
            outgoing,
            failed,
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn did_open(&self, uri: String, language: &str, text: String) {
        let _ = self.outgoing.send(Outgoing::Open {
            uri,
            language: language.to_string(),
            text,
        });
    }

    pub fn did_change(&self, uri: String, text: String, version: i64) {
        let _ = self.outgoing.send(Outgoing::Change { uri, text, version });
    }

    pub fn did_save(&self, uri: String) {
        let _ = self.outgoing.send(Outgoing::Save { uri });
    }

    pub fn shutdown(&self) {
        let _ = self.outgoing.send(Outgoing::Shutdown);
    }
}

/// Start a server process and drive one LSP session.
pub fn spawn_client(tx: EventTx, spec: ServerSpec, root: PathBuf, job: JobId) -> ClientHandle {
    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<Outgoing>();
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = ClientHandle::new(
        spec.language.clone(),
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
                        "didSave": true,
                        "dynamicRegistration": false
                    }
                },
                "window": { "workDoneProgress": false }
            },
            "workspaceFolders": [{ "uri": uri_for(&root), "name": root.display().to_string() }]
        })),
    );
    jsonrpc::write_message(&mut *stdin.lock().await, &initialize)
        .await
        .map_err(|error| error.to_string())?;

    let mut reader = BufReader::new(stdout);
    let mut initialized_sent = false;
    let next_id: i64 = 2;

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
                            // The initialize response.
                            if !initialized_sent {
                                initialized_sent = true;
                                let note = jsonrpc::notification("initialized", Some(json!({})));
                                let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
                                tx.send_async(AsyncEvent::Lsp(LspEvent::Ready {
                                    language: spec.language.clone(),
                                    root: root.clone(),
                                }));
                            }
                            continue;
                        }
                        match value.get("method").and_then(Value::as_str) {
                            Some("textDocument/publishDiagnostics") => {
                                let uri = value
                                    .pointer("/params/uri")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                tx.send_async(AsyncEvent::Lsp(LspEvent::Diagnostics {
                                    uri,
                                    items: diagnostic_items(&value),
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
                    Some(Outgoing::Open { uri, language, text }) => {
                        let note = jsonrpc::notification(
                            "textDocument/didOpen",
                            Some(json!({ "textDocument": {
                                "uri": uri, "languageId": language, "version": 1, "text": text
                            }})),
                        );
                        let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
                    }
                    Some(Outgoing::Change { uri, text, version }) => {
                        // Whole-buffer sync: simplest correct thing to send.
                        let note = jsonrpc::notification(
                            "textDocument/didChange",
                            Some(json!({ "textDocument": { "uri": uri, "version": version },
                                          "contentChanges": [{ "text": text }] })),
                        );
                        let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
                    }
                    Some(Outgoing::Save { uri }) => {
                        let note = jsonrpc::notification(
                            "textDocument/didSave",
                            Some(json!({ "textDocument": { "uri": uri } })),
                        );
                        let _ = jsonrpc::write_message(&mut *stdin.lock().await, &note).await;
                    }
                }
            }
        }
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
    fn ranges_map_to_byte_offsets() {
        let text = "first\nsecond line\nthird\n";
        let starts = line_starts(text);
        let span = range_to_span(&starts, text.len(), (1, 7), (1, 11));
        assert_eq!((&text[span.start..span.end]), "line");
    }

    #[test]
    fn ranges_are_clamped_to_the_buffer() {
        let text = "short\n";
        let starts = line_starts(text);
        let span = range_to_span(&starts, text.len(), (0, 2), (9, 40));
        assert_eq!(span.start, 2);
        assert_eq!(span.end, text.len());
    }

    #[test]
    fn diagnostics_decode_then_map_to_bytes() {
        let text = "let x = ;\nsecond line\n";
        let starts = line_starts(text);
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
            .map(|item| item.to_diagnostic(&starts, text.len()))
            .collect();
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert!(diagnostics[0].message.contains("expected expression"));
        assert_eq!(diagnostics[0].range, Span::new(8, 9));
        assert_eq!(diagnostics[1].range, Span::new(10, 16));
        assert_eq!(diagnostics[1].severity, Severity::Warning);
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
}
