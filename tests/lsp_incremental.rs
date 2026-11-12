//! Live language-server checks.
//!
//! These drive a real `rust-analyzer` process, because the thing they test —
//! that the server's copy of the document matches ours after a sequence of
//! edits — cannot be checked without one.  A desync is silent: every later
//! diagnostic, completion and hover is answered against text that no longer
//! exists, with no error to notice.
//!
//! The tests skip when `rust-analyzer` is not installed, so they stay portable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;
use vortex::config::LspSync;
use vortex::event::{AsyncEvent, Event, EventTx, JobId};
use vortex::lsp::{self, ClientHandle, LspEvent, ServerSpec};

/// How long to wait for the server to answer.  rust-analyzer loads a fresh
/// project on first run, which is not instant.
const TIMEOUT: Duration = Duration::from_secs(60);

fn rust_analyzer() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("rust-analyzer"))
        .find(|candidate| candidate.is_file())
}

/// A scratch crate, removed when the test ends.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        Self::with_source(name, "fn main() {}\n")
    }

    /// A scratch crate whose `src/lib.rs` holds `source`.
    ///
    /// The file is written to disk as well as opened in memory: rust-analyzer
    /// builds its crate graph from the filesystem, and a file it cannot see
    /// there gets no analysis at all.
    fn with_source(name: &str, source: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("vortex-lsp-{name}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("scratch dir");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"scratch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("Cargo.toml");
        std::fs::write(root.join("src/lib.rs"), source).expect("lib.rs");
        Self { root }
    }

    fn file(&self) -> PathBuf {
        self.root.join("src/lib.rs")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running server plus the event stream it reports on.
struct Session {
    handle: ClientHandle,
    rx: mpsc::UnboundedReceiver<Event>,
}

impl Session {
    fn start(root: &Path, sync: LspSync) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let spec = ServerSpec {
            language: "rust".into(),
            command: "rust-analyzer".into(),
            args: Vec::new(),
            sync,
        };
        let handle = lsp::spawn_client(
            EventTx::from_sender(tx),
            spec,
            root.to_path_buf(),
            JobId::new(0),
        );
        Self { handle, rx }
    }

    /// Wait for the first event `want` accepts, or give up at the deadline.
    async fn wait_for<F>(&mut self, mut want: F) -> Option<LspEvent>
    where
        F: FnMut(&LspEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let event = match tokio::time::timeout(remaining, self.rx.recv()).await {
                Ok(Some(event)) => event,
                _ => return None,
            };
            if let Event::Async(AsyncEvent::Lsp(event)) = event
                && want(&event)
            {
                return Some(event);
            }
        }
    }

    /// Wait for a published diagnostic whose message mentions `needle`.
    ///
    /// rust-analyzer publishes an empty set first (clearing whatever was
    /// there) and the real one once analysis finishes, so an empty publish is
    /// not the answer — the wait continues past it.
    async fn diagnostics_until(
        &mut self,
        uri: &str,
        needle: &str,
    ) -> Option<Vec<lsp::DiagnosticItem>> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let event = match tokio::time::timeout(remaining, self.rx.recv()).await {
                Ok(Some(event)) => event,
                _ => return None,
            };
            if let Event::Async(AsyncEvent::Lsp(LspEvent::Diagnostics {
                uri: published,
                items,
                ..
            })) = event
                && published == uri
                && items.iter().any(|item| item.message.contains(needle))
            {
                return Some(items);
            }
        }
    }

    /// Ask for hover at `(line, column)` until the answer mentions `needle`.
    ///
    /// The retry is not flakiness papering: the server processes `didChange`
    /// asynchronously, so the first request can legitimately arrive before the
    /// edit has been applied.  What matters is that it *eventually* answers
    /// from the edited text — a desynced server never will.
    async fn hover_until(&mut self, uri: &str, needle: &str) -> bool {
        for attempt in 0..10 {
            let request = 100 + attempt;
            self.handle.hover(uri.to_string(), request, 0, 4);
            let answer = self
                .wait_for(
                    |event| matches!(event, LspEvent::Hover { request: id, .. } if *id == request),
                )
                .await;
            if let Some(LspEvent::Hover { text, .. }) = answer
                && text.contains(needle)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        false
    }
}

/// The inserted function, and the hover position that lands on its name.
const INSERTED: &str = "fn added() -> u32 { 1 }\n";

#[tokio::test]
async fn an_incremental_edit_reaches_the_server() {
    if rust_analyzer().is_none() {
        eprintln!("skipping: rust-analyzer is not installed");
        return;
    }
    let scratch = Scratch::new("incremental");
    let mut session = Session::start(&scratch.root, LspSync::Incremental);
    assert!(
        session
            .wait_for(|event| matches!(event, LspEvent::Ready { .. }))
            .await
            .is_some(),
        "the server never became ready"
    );

    let uri = lsp::uri_for(&scratch.file());
    session
        .handle
        .did_open(uri.clone(), "rust", "fn main() {}\n".into(), 1);

    // Insert a function at the top, as a ranged edit.
    session.handle.did_change_ranged(
        uri.clone(),
        vec![vortex::buffer::RecordedChange {
            start: (0, 0),
            end: (0, 0),
            text: INSERTED.into(),
        }],
        2,
    );

    // Hovering the new name only answers if the server's copy has it.
    assert!(
        session.hover_until(&uri, "added").await,
        "the server never saw the incrementally inserted function"
    );
}

#[tokio::test]
async fn a_full_sync_edit_reaches_the_server() {
    // The escape hatch has to work too: it is what a user is told to try when
    // incremental sync looks wrong.
    if rust_analyzer().is_none() {
        eprintln!("skipping: rust-analyzer is not installed");
        return;
    }
    let scratch = Scratch::new("full");
    let mut session = Session::start(&scratch.root, LspSync::Full);
    assert!(
        session
            .wait_for(|event| matches!(event, LspEvent::Ready { .. }))
            .await
            .is_some(),
        "the server never became ready"
    );

    let uri = lsp::uri_for(&scratch.file());
    session
        .handle
        .did_open(uri.clone(), "rust", "fn main() {}\n".into(), 1);
    session
        .handle
        .did_change(uri.clone(), format!("{INSERTED}fn main() {{}}\n"), 2);

    assert!(
        session.hover_until(&uri, "added").await,
        "the server never saw the whole-document edit"
    );
}

#[tokio::test]
async fn a_sequence_of_incremental_edits_stays_in_step() {
    // One edit can be right by luck; a run of them is what catches an offset
    // that drifts.  Each edit is expressed against the text the previous one
    // produced, which is exactly how the buffer records them.
    if rust_analyzer().is_none() {
        eprintln!("skipping: rust-analyzer is not installed");
        return;
    }
    let scratch = Scratch::new("sequence");
    let mut session = Session::start(&scratch.root, LspSync::Incremental);
    assert!(
        session
            .wait_for(|event| matches!(event, LspEvent::Ready { .. }))
            .await
            .is_some(),
        "the server never became ready"
    );

    let uri = lsp::uri_for(&scratch.file());
    session
        .handle
        .did_open(uri.clone(), "rust", "fn main() {}\n".into(), 1);

    // Type the function one piece at a time, the way a person would.
    for (step, (line, column, text)) in [
        (0, 0, "fn "),
        (0, 3, "added"),
        (0, 8, "() -> u32 { 1 }"),
        (0, 22, "\n"),
    ]
    .into_iter()
    .enumerate()
    {
        session.handle.did_change_ranged(
            uri.clone(),
            vec![vortex::buffer::RecordedChange {
                start: (line, column),
                end: (line, column),
                text: text.into(),
            }],
            // Versions start at 2: `didOpen` claimed 1.
            2 + step as i64,
        );
    }

    assert!(
        session.hover_until(&uri, "added").await,
        "the server lost step with a sequence of incremental edits"
    );
}

#[tokio::test]
async fn a_broken_file_publishes_a_diagnostic() {
    // The end-to-end check the decoder tests cannot make: a real server, a real
    // file with a real error, and a diagnostic that has to survive the whole
    // path — envelope decode, URI match, and the version gate.  Every one of
    // those has silently dropped diagnostics at some point, and each time the
    // unit tests kept passing because they never ran the path end to end.
    if rust_analyzer().is_none() {
        eprintln!("skipping: rust-analyzer is not installed");
        return;
    }
    // `undefined_name` is never declared, so rust-analyzer reports
    // "cannot find value `undefined_name` in this scope".
    let source = "fn main() {\n    let _ = undefined_name;\n}\n";
    let scratch = Scratch::with_source("diagnostics", source);
    let mut session = Session::start(&scratch.root, LspSync::Incremental);
    assert!(
        session
            .wait_for(|event| matches!(event, LspEvent::Ready { .. }))
            .await
            .is_some(),
        "the server never became ready"
    );

    let uri = lsp::uri_for(&scratch.file());
    session
        .handle
        .did_open(uri.clone(), "rust", source.into(), 1);

    let items = session
        .diagnostics_until(&uri, "undefined_name")
        .await
        .expect("no diagnostic for an undefined name");

    let item = items
        .iter()
        .find(|item| item.message.contains("undefined_name"))
        .expect("the diagnostic mentions the name");
    assert_eq!(
        item.severity,
        vortex::diagnostic::Severity::Error,
        "an undefined name is an error, got {:?}",
        item.severity
    );
    // The range has to land on the offending line, not at the top of the file:
    // a position conversion that is off by a line still "works" otherwise.
    assert_eq!(item.range.0, 1, "the error is on the second line");
    assert!(
        item.range.1 > 0,
        "the range starts at the name, not at column 0"
    );
}

#[tokio::test]
async fn an_unsaved_error_is_pulled_from_the_server() {
    // The bug this guards: rust-analyzer pushes only the compiler's view of the
    // *saved* file (its flycheck results).  Its own analysis of the edited
    // buffer is served only through `textDocument/diagnostic`, signalled by
    // `workspace/diagnostic/refresh`.  A push-only client therefore shows a
    // clean file that is full of unsaved type errors — the exact case a user
    // hits the moment they type.
    if rust_analyzer().is_none() {
        eprintln!("skipping: rust-analyzer is not installed");
        return;
    }
    // The file on disk is clean; the error exists only in the buffer.
    let scratch = Scratch::with_source("unsaved", "fn main() {}\n");
    let mut session = Session::start(&scratch.root, LspSync::Incremental);
    assert!(
        session
            .wait_for(|event| matches!(event, LspEvent::Ready { .. }))
            .await
            .is_some(),
        "the server never became ready"
    );

    let uri = lsp::uri_for(&scratch.file());
    session
        .handle
        .did_open(uri.clone(), "rust", "fn main() {}\n".into(), 1);
    // Type the error without saving.
    session.handle.did_change(
        uri.clone(),
        "fn main() {\n    let _ = undefined_name;\n}\n".into(),
        2,
    );

    // The server says its analysis changed; that is the cue to pull, exactly as
    // the app does.  It may refresh more than once — an early refresh can
    // describe the clean file before the edit has been analysed — so pull on
    // each one until the answer carries the unsaved error.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut request = 900;
    let items: Vec<lsp::DiagnosticItem>;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "no pulled diagnostic for an unsaved error"
        );
        let event = match tokio::time::timeout(remaining, session.rx.recv()).await {
            Ok(Some(event)) => event,
            _ => panic!("no pulled diagnostic for an unsaved error"),
        };
        match event {
            Event::Async(AsyncEvent::Lsp(LspEvent::DiagnosticsRefresh { .. })) => {
                session.handle.pull_diagnostics(uri.clone(), request, 2);
                request += 1;
            }
            Event::Async(AsyncEvent::Lsp(LspEvent::Diagnostics {
                uri: published,
                origin: lsp::DiagnosticOrigin::Pull,
                unchanged: false,
                items: pulled,
                ..
            })) if published == uri
                && pulled
                    .iter()
                    .any(|item| item.message.contains("no such value")) =>
            {
                items = pulled;
                break;
            }
            _ => {}
        }
    }
    let item = items
        .iter()
        .find(|item| item.message.contains("no such value"))
        .expect("the pulled diagnostic mentions the missing value");
    assert_eq!(
        item.severity,
        vortex::diagnostic::Severity::Error,
        "an undefined name is an error, got {:?}",
        item.severity
    );
    assert_eq!(item.range.0, 1, "the error is on the second line");
}
