//! Project search: file names and file contents.
//!
//! ripgrep does the work when it is installed (`[search] ripgrep = "auto"`);
//! otherwise vortex walks the tree itself with the `ignore` crate so `.gitignore`
//! is still honoured.  Searches run as background jobs that stream batches of
//! hits back to the picker.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::config::SearchConfig;
use crate::event::{AsyncEvent, EventTx, JobId};

/// What a search job is looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    FileNames,
    Contents,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    /// 1-based line number; `0` for file-name hits.
    pub line: usize,
    /// 1-based column; `0` when unknown.
    pub column: usize,
    /// The matching line, empty for file-name hits.
    pub text: String,
}

impl Hit {
    pub fn file(path: PathBuf) -> Self {
        Self {
            path,
            line: 0,
            column: 0,
            text: String::new(),
        }
    }

    /// Relative path for display, falling back to the full path.
    pub fn label(&self, root: &Path) -> String {
        let relative = self
            .path
            .strip_prefix(root)
            .unwrap_or(&self.path)
            .display()
            .to_string();
        if self.line == 0 {
            relative
        } else {
            format!("{relative}:{}", self.line)
        }
    }

    /// Preview text: the matching line, trimmed and shortened.
    pub fn preview(&self, width: usize) -> String {
        let text = self.text.trim();
        if text.chars().count() <= width {
            text.to_string()
        } else {
            let mut shortened: String = text.chars().take(width.saturating_sub(1)).collect();
            shortened.push('…');
            shortened
        }
    }
}

/// A batch of results from a running search.
#[derive(Debug, Clone)]
pub struct SearchEvent {
    pub job: JobId,
    pub kind: SearchKind,
    pub query: String,
    pub hits: Vec<Hit>,
    /// True on the final batch, so the picker knows the count is complete.
    pub done: bool,
    pub truncated: bool,
    /// Set when the search could not run at all.
    pub error: Option<String>,
    /// Which engine produced these results, shown in the status area.
    pub engine: &'static str,
}

/// Handle used to stop a search that is no longer interesting.
#[derive(Clone)]
pub struct SearchHandle {
    pub id: JobId,
    cancelled: Arc<AtomicBool>,
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

fn next_job_id() -> JobId {
    JobId(NEXT_JOB.fetch_add(1, Ordering::Relaxed))
}

/// Locate the ripgrep binary according to `[search] ripgrep`.
pub fn ripgrep_binary(config: &SearchConfig) -> Option<PathBuf> {
    match config.ripgrep.trim() {
        "" | "off" | "none" | "builtin" => None,
        "auto" => which("rg"),
        explicit => {
            let path = PathBuf::from(explicit);
            if path.exists() {
                Some(path)
            } else {
                which("rg")
            }
        }
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let separators = ['/', '\\'];
    if program.contains(separators) {
        return PathBuf::from(program)
            .exists()
            .then(|| PathBuf::from(program));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(program);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// List files under `root`, streaming results.
pub fn spawn_file_search(
    tx: EventTx,
    root: PathBuf,
    query: String,
    config: SearchConfig,
) -> SearchHandle {
    let id = next_job_id();
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    tokio::spawn(async move {
        match ripgrep_binary(&config) {
            Some(binary) => {
                let outcome =
                    stream_ripgrep_files(&tx, id, &query, &root, &config, &flag, &binary).await;
                if let Some(outcome) = outcome {
                    outcome.finish(&tx, id, SearchKind::FileNames, &query, "ripgrep");
                }
            }
            None => walk_files(tx, id, &query, root, config, flag).await,
        }
    });
    SearchHandle { id, cancelled }
}

/// Search file contents under `root`.
pub fn spawn_grep(
    tx: EventTx,
    root: PathBuf,
    query: String,
    config: SearchConfig,
) -> Option<SearchHandle> {
    if query.trim().is_empty() {
        return None;
    }
    let id = next_job_id();
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    tokio::spawn(async move {
        match ripgrep_binary(&config) {
            Some(binary) => {
                let mut outcome =
                    stream_ripgrep_grep(&tx, id, &query, &root, &config, &flag, &binary, false)
                        .await;
                // A half-typed pattern (`foo(`, `a|`) is usually not a valid
                // regex yet.  Rather than showing nothing, search for it
                // literally, which is also what the built-in engine does.
                if outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.pattern_rejected && outcome.streamed == 0)
                {
                    outcome =
                        stream_ripgrep_grep(&tx, id, &query, &root, &config, &flag, &binary, true)
                            .await;
                }
                if let Some(outcome) = outcome {
                    outcome.finish(&tx, id, SearchKind::Contents, &query, "ripgrep");
                }
            }
            None => grep_fallback(tx, id, &query, root, config, flag).await,
        }
    });
    Some(SearchHandle { id, cancelled })
}

/// How many results to accumulate before sending a batch.
const BATCH: usize = 256;
/// Milliseconds between batches.
const BATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(60);

#[allow(clippy::too_many_arguments)]
async fn stream_ripgrep_files(
    tx: &EventTx,
    job: JobId,
    query: &str,
    root: &Path,
    config: &SearchConfig,
    cancelled: &AtomicBool,
    binary: &Path,
) -> Option<EngineOutcome> {
    let mut command = Command::new(binary);
    command.arg("--files").arg(root);
    apply_common_flags(&mut command, config);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Some(EngineOutcome::failure(spawn_failed(binary, &error))),
    };

    stream_lines(
        tx,
        job,
        SearchKind::FileNames,
        query,
        child,
        root,
        config,
        cancelled,
        "ripgrep",
        |line, root| Some(Hit::file(resolve(root, line))),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_ripgrep_grep(
    tx: &EventTx,
    job: JobId,
    query: &str,
    root: &Path,
    config: &SearchConfig,
    cancelled: &AtomicBool,
    binary: &Path,
    fixed_strings: bool,
) -> Option<EngineOutcome> {
    let mut command = Command::new(binary);
    command
        .arg("--no-heading")
        .arg("--line-number")
        // `--column` makes the output `path:line:column:text`, so the matched
        // text is whatever follows the third field.  Without it a line whose
        // own text contains a colon is cut in the wrong place.
        .arg("--column")
        .arg("--color")
        .arg("never")
        .arg("--with-filename")
        // Unreadable files are not worth a screen full of noise; real failures
        // still reach stderr and are reported.
        .arg("--no-messages");
    if fixed_strings {
        command.arg("--fixed-strings");
    }
    if config.smart_case {
        command.arg("--smart-case");
    }
    apply_common_flags(&mut command, config);
    // The pattern goes in `-e` so it stays the pattern whatever it starts
    // with, leaving `root` as the only positional argument (the path).
    command.arg("-e").arg(query).arg(root);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Some(EngineOutcome::failure(spawn_failed(binary, &error))),
    };

    stream_lines(
        tx,
        job,
        SearchKind::Contents,
        query,
        child,
        root,
        config,
        cancelled,
        "ripgrep",
        |line, root| parse_grep_line(line, root, true),
    )
    .await
}

fn spawn_failed(binary: &Path, error: &std::io::Error) -> String {
    format!("{}: {error}", binary.display())
}

fn apply_common_flags(command: &mut Command, config: &SearchConfig) {
    if config.hidden {
        command.arg("--hidden");
    }
    if config.follow {
        command.arg("--follow");
    }
    if !config.ignore {
        command.arg("--no-ignore");
    }
    for pattern in &config.prune {
        command.arg("--glob").arg(format!("!{pattern}"));
    }
}

fn resolve(root: &Path, relative: &str) -> PathBuf {
    let path = Path::new(relative);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Parse one line of ripgrep's line-oriented output.
///
/// `with_column` picks the shape: `path:line:column:text` when `--column` was
/// passed, `path:line:text` otherwise.  The matched text may contain colons, so
/// the fields are found by looking for `<digits>:` boundaries rather than by
/// splitting on every colon — splitting blindly turns
/// `app.rs:12:7:let x: u8 = 1;` into a hit whose text starts at `u8`, and a
/// Windows path (`C:\src\app.rs`) has a colon of its own.
fn parse_grep_line(line: &str, root: &Path, with_column: bool) -> Option<Hit> {
    let (path_end, line_no, after_line) = find_numeric_field(line, 0)?;
    let path = &line[..path_end];
    let (column, text) = if with_column {
        let (column, after_column) = numeric_field_at(line, after_line)?;
        (column, &line[after_column..])
    } else {
        (0, &line[after_line..])
    };
    if path.is_empty() {
        return None;
    }
    Some(Hit {
        path: resolve(root, path),
        line: line_no,
        column,
        text: text.to_string(),
    })
}

/// Read a numeric field that must start at `at`, returning its value and the
/// offset just past the colon that ends it.
fn numeric_field_at(line: &str, at: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut end = at.min(line.len());
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == at || bytes.get(end) != Some(&b':') {
        return None;
    }
    Some((line[at..end].parse().ok()?, end + 1))
}

/// Find the line-number field, returning the offset where the path ends, the
/// line number and the offset just past the field.
///
/// The field is located rather than assumed to follow the first colon because
/// the path can contain colons: `C:\src\app.rs:12:7:text` starts with one.
fn find_numeric_field(line: &str, from: usize) -> Option<(usize, usize, usize)> {
    let mut search = from.min(line.len());
    while let Some(relative) = line[search..].find(':') {
        let colon = search + relative;
        if let Some((value, after)) = numeric_field_at(line, colon + 1) {
            return Some((colon, value, after));
        }
        search = colon + 1;
    }
    None
}

/// Read a search engine's stdout, batching hits so the picker can fill in
/// progressively instead of waiting for the whole tree.
#[allow(clippy::too_many_arguments)]
async fn stream_lines(
    tx: &EventTx,
    job: JobId,
    kind: SearchKind,
    query: &str,
    mut child: tokio::process::Child,
    root: &Path,
    config: &SearchConfig,
    cancelled: &AtomicBool,
    engine: &'static str,
    parse: impl Fn(&str, &Path) -> Option<Hit>,
) -> Option<EngineOutcome> {
    // stderr is drained on every run, not only failed ones: a child whose
    // stderr pipe fills up stops writing to stdout, and the search would hang.
    let stderr = child
        .stderr
        .take()
        .map(|pipe| tokio::spawn(async move { drain_stderr(pipe).await }));
    let Some(stdout) = child.stdout.take() else {
        return Some(EngineOutcome::failure(format!(
            "{engine} produced no output"
        )));
    };
    let mut reader = BufReader::new(stdout);
    let mut buffer = vec![0u8; 16 * 1024];
    let mut pending = String::new();
    let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
    let mut total = 0usize;
    let mut truncated = false;
    let mut next_flush = std::time::Instant::now() + BATCH_INTERVAL;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            // Nobody is listening any more; report nothing for a dead job.
            return None;
        }
        if total >= config.max_results {
            truncated = true;
            break;
        }
        // A short timeout doubles as the batch tick and the cancel poll.
        let chunk = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            reader.read(&mut buffer),
        )
        .await;
        match chunk {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                pending.push_str(&String::from_utf8_lossy(&buffer[..n]));
                while let Some(newline) = pending.find('\n') {
                    let line = pending[..newline].trim_end_matches('\r').to_string();
                    pending.drain(..=newline);
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Some(hit) = parse(&line, root) {
                        batch.push(hit);
                        if batch.len() >= BATCH {
                            total += batch.len();
                            send(
                                tx,
                                Batch::progress(
                                    job,
                                    kind,
                                    query,
                                    std::mem::take(&mut batch),
                                    engine,
                                ),
                            );
                            next_flush = std::time::Instant::now() + BATCH_INTERVAL;
                        }
                    }
                }
            }
            Ok(Err(error)) => {
                return Some(EngineOutcome {
                    streamed: total,
                    remainder: batch,
                    truncated,
                    pattern_rejected: false,
                    error: Some(error.to_string()),
                });
            }
            Err(_elapsed) => {
                if std::time::Instant::now() >= next_flush && !batch.is_empty() {
                    total += batch.len();
                    send(
                        tx,
                        Batch::progress(job, kind, query, std::mem::take(&mut batch), engine),
                    );
                    next_flush = std::time::Instant::now() + BATCH_INTERVAL;
                }
            }
        }
    }

    // A last line without a trailing newline still counts.
    if !pending.trim().is_empty() {
        if let Some(hit) = parse(pending.trim(), root) {
            batch.push(hit);
        }
    }

    if truncated {
        // The child is still writing to a pipe nobody reads, so it must be
        // dropped rather than waited on: dropping is what terminates it.
        drop(child);
        return Some(EngineOutcome {
            streamed: total,
            remainder: batch,
            truncated: true,
            pattern_rejected: false,
            error: None,
        });
    }

    let status = child.wait().await.ok();
    let stderr_text = match stderr {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };
    Some(EngineOutcome {
        streamed: total,
        remainder: batch,
        truncated: false,
        // ripgrep's own wording, matched loosely so a version tweak does not
        // turn a bad pattern back into "no results".
        pattern_rejected: stderr_text.contains("regex parse error")
            || stderr_text.contains("error parsing pattern"),
        error: engine_error(engine, status.as_ref().and_then(|s| s.code()), &stderr_text),
    })
}

/// What a finished engine run has to say about itself, as distinct from the hits
/// it already streamed.
struct EngineOutcome {
    /// Hits already sent to the picker.
    streamed: usize,
    /// Hits still buffered, to go out with the final event.
    remainder: Vec<Hit>,
    truncated: bool,
    /// The engine could not compile the pattern, so the caller may want to try
    /// again with the pattern taken literally.
    pattern_rejected: bool,
    error: Option<String>,
}

impl EngineOutcome {
    fn failure(error: String) -> Self {
        Self {
            streamed: 0,
            remainder: Vec::new(),
            truncated: false,
            pattern_rejected: false,
            error: Some(error),
        }
    }

    /// Send the final event, which the picker needs even when it carries no
    /// hits, so it can stop showing the list as still growing.
    fn finish(self, tx: &EventTx, job: JobId, kind: SearchKind, query: &str, engine: &'static str) {
        send(
            tx,
            Batch::finish(
                job,
                kind,
                query,
                self.remainder,
                self.truncated,
                self.error,
                engine,
            ),
        );
    }
}

/// Read a child's stderr into a bounded string so it can never block the child.
async fn drain_stderr(mut pipe: impl AsyncReadExt + Unpin) -> String {
    const KEEP: usize = 4 * 1024;
    let mut text = String::new();
    let mut buffer = [0u8; 1024];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                if text.len() < KEEP {
                    text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
            }
            Err(_) => break,
        }
    }
    text
}

/// Turn an exit code and stderr into a message, if the engine actually failed.
///
/// A non-zero status is not enough on its own: ripgrep exits 1 for "nothing
/// matched", which is a perfectly good answer to a search.
fn engine_error(engine: &str, code: Option<i32>, stderr: &str) -> Option<String> {
    let failed = match code {
        Some(code) => code >= 2,
        // Killed by a signal, or the status could not be read at all.
        None => !stderr.is_empty(),
    };
    if !failed {
        return None;
    }
    let summary = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    Some(if summary.is_empty() {
        format!("{engine} failed (exit {})", code.unwrap_or(-1))
    } else {
        format!("{engine}: {summary}")
    })
}

/// One batch of search results on its way to the UI.
///
/// A struct rather than nine positional arguments: two adjacent `bool`s are far
/// too easy to swap.
struct Batch<'a> {
    job: JobId,
    kind: SearchKind,
    query: &'a str,
    hits: Vec<Hit>,
    done: bool,
    truncated: bool,
    error: Option<String>,
    engine: &'static str,
}

impl<'a> Batch<'a> {
    /// Another page of results, the search is still running.
    fn progress(
        job: JobId,
        kind: SearchKind,
        query: &'a str,
        hits: Vec<Hit>,
        engine: &'static str,
    ) -> Self {
        Self {
            job,
            kind,
            query,
            hits,
            done: false,
            truncated: false,
            error: None,
            engine,
        }
    }

    /// The last update for a job, successful or not.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        job: JobId,
        kind: SearchKind,
        query: &'a str,
        hits: Vec<Hit>,
        truncated: bool,
        error: Option<String>,
        engine: &'static str,
    ) -> Self {
        Self {
            job,
            kind,
            query,
            hits,
            done: true,
            truncated,
            error,
            engine,
        }
    }
}

fn send(tx: &EventTx, batch: Batch<'_>) {
    let Batch {
        job,
        kind,
        query,
        hits,
        done,
        truncated,
        error,
        engine,
    } = batch;
    if hits.is_empty() && !done && error.is_none() {
        return;
    }
    tx.send_async(AsyncEvent::Search(SearchEvent {
        job,
        kind,
        query: query.to_string(),
        hits,
        done,
        truncated,
        error,
        engine,
    }));
}

// ------------------------------------------------------------------ built-in

/// Walk the tree with the `ignore` crate (respects `.gitignore`).
async fn walk_files(
    tx: EventTx,
    job: JobId,
    query: &str,
    root: PathBuf,
    config: SearchConfig,
    cancelled: Arc<AtomicBool>,
) {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<Hit>>();
    let flag = Arc::clone(&cancelled);
    let walk_config = config.clone();
    tokio::task::spawn_blocking(move || {
        let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
        let walker = workspace_walker(&root, &walk_config);
        for entry in walker.flatten() {
            if flag.load(Ordering::Relaxed) {
                return;
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            batch.push(Hit::file(entry.into_path()));
            if batch.len() >= BATCH && sender.send(std::mem::take(&mut batch)).is_err() {
                return;
            }
        }
        if !batch.is_empty() {
            let _ = sender.send(batch);
        }
    });

    let mut total = 0usize;
    let mut truncated = false;
    while let Some(batch) = receiver.recv().await {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        if total >= config.max_results {
            truncated = true;
            break;
        }
        total += batch.len();
        send(
            &tx,
            Batch::progress(job, SearchKind::FileNames, query, batch, "built-in"),
        );
    }
    send(
        &tx,
        Batch::finish(
            job,
            SearchKind::FileNames,
            query,
            Vec::new(),
            truncated,
            None,
            "built-in",
        ),
    );
}

/// Brute force content search, used only when ripgrep is unavailable.
///
/// The pattern is taken literally.  That is what the picker asks for most of the
/// time — and it is what ripgrep falls back to when a half-typed pattern is not
/// a valid regex yet — so the two engines agree on the common case even though
/// only ripgrep understands `a|b`.
async fn grep_fallback(
    tx: EventTx,
    job: JobId,
    query: &str,
    root: PathBuf,
    config: SearchConfig,
    cancelled: Arc<AtomicBool>,
) {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<Hit>>();
    let flag = Arc::clone(&cancelled);
    let needle = query.to_string();
    let search_config = config.clone();
    tokio::task::spawn_blocking(move || {
        let case_sensitive = !search_config.smart_case || needle.chars().any(|c| c.is_uppercase());
        let needle_compare = if case_sensitive {
            needle.clone()
        } else {
            needle.to_lowercase()
        };
        let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
        let walker = workspace_walker(&root, &search_config);
        for entry in walker.flatten() {
            if flag.load(Ordering::Relaxed) {
                return;
            }
            let path = entry.path();
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if !looks_textual(path) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                let haystack = if case_sensitive {
                    line.to_string()
                } else {
                    line.to_lowercase()
                };
                if let Some(at) = haystack.find(&needle_compare) {
                    batch.push(Hit {
                        path: path.to_path_buf(),
                        line: index + 1,
                        column: line[..at].chars().count() + 1,
                        text: line.to_string(),
                    });
                    if batch.len() >= BATCH && sender.send(std::mem::take(&mut batch)).is_err() {
                        return;
                    }
                }
            }
        }
        if !batch.is_empty() {
            let _ = sender.send(batch);
        }
    });

    let mut total = 0usize;
    let mut truncated = false;
    while let Some(batch) = receiver.recv().await {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        if total >= config.max_results {
            truncated = true;
            break;
        }
        total += batch.len();
        send(
            &tx,
            Batch::progress(job, SearchKind::Contents, query, batch, "built-in"),
        );
    }
    send(
        &tx,
        Batch::finish(
            job,
            SearchKind::Contents,
            query,
            Vec::new(),
            truncated,
            None,
            "built-in",
        ),
    );
}

/// Build a gitignore-style matcher from `[search] prune`.
fn prune_matcher(root: &Path, config: &SearchConfig) -> Option<ignore::gitignore::Gitignore> {
    if config.prune.is_empty() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    for pattern in &config.prune {
        let _ = builder.add_line(None, pattern);
    }
    builder.build().ok()
}

fn pruned(matcher: Option<&ignore::gitignore::Gitignore>, path: &Path, is_dir: bool) -> bool {
    matcher.is_some_and(|matcher| matcher.matched(path, is_dir).is_ignore())
}

/// A walker over `root` that honours the same `[search]` settings a search does.
///
/// Background work that reads the tree (the search pickers, and registering
/// workspace files with a language server) must agree about what is in the
/// tree.  Walking `node_modules` or `target` is not just slow: the LSP scan
/// starts one server per root it finds a file for, so a vendored tree full of
/// thousands of directories turns into thousands of processes.
pub fn workspace_walker(root: &Path, config: &SearchConfig) -> ignore::Walk {
    let matcher = prune_matcher(root, config);
    let owned = root.to_path_buf();
    ignore::WalkBuilder::new(root)
        .hidden(!config.hidden)
        .follow_links(config.follow)
        .ignore(config.ignore)
        .git_ignore(config.ignore)
        .git_global(config.ignore)
        // Pruning as a filter also stops the walk descending into `target`.
        .filter_entry(move |entry| keep_entry(matcher.as_ref(), &owned, entry))
        .build()
}

/// Skip `[search] prune` directories without leaving the search root behind.
fn keep_entry(
    matcher: Option<&ignore::gitignore::Gitignore>,
    root: &Path,
    entry: &ignore::DirEntry,
) -> bool {
    let path = entry.path();
    if path == root {
        return true;
    }
    !pruned(
        matcher,
        path,
        entry.file_type().is_some_and(|kind| kind.is_dir()),
    )
}

/// Cheap binary check: reject files with a NUL byte early on.
fn looks_textual(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0u8; 512];
    let Ok(size) = std::io::Read::read(&mut &file, &mut buffer) else {
        return false;
    };
    !buffer[..size].contains(&0)
}

/// The directory searches start from: the git root if there is one, else the
/// directory of the first opened file, else the current directory.
pub fn project_root(path: Option<&Path>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    project_root_in(path, &cwd)
}

/// [`project_root`] with an explicit current directory, so relative paths
/// (as typed on the command line) resolve the way the shell resolves them.
fn project_root_in(path: Option<&Path>, cwd: &Path) -> PathBuf {
    // A relative argument like `src/main.rs` must be joined with the cwd
    // first: walking up from the bare relative path bottoms out at an empty
    // path, which would silently become the search root.
    let start = match path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => cwd.to_path_buf(),
    };
    let start = if start.is_dir() {
        start
    } else {
        match start.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => cwd.to_path_buf(),
        }
    };
    let mut current = start.as_path();
    loop {
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => break,
        }
    }
    if start.is_dir() {
        start
    } else {
        cwd.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Each test needs its own directory: tests run in parallel in one process.
    fn temp_project() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "vortex-search-test-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    vortex();\n}\n").unwrap();
        std::fs::write(root.join("README.md"), "# vortex\n\nnothing here\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/noise"), "vortex in target\n").unwrap();
        root
    }

    async fn collect(rx: &mut mpsc::UnboundedReceiver<crate::event::Event>) -> Vec<Hit> {
        let mut hits = Vec::new();
        while let Some(event) = rx.recv().await {
            if let crate::event::Event::Async(AsyncEvent::Search(search)) = event {
                hits.extend(search.hits);
                if search.done {
                    return hits;
                }
            }
        }
        hits
    }

    #[tokio::test]
    async fn lists_files() {
        let root = temp_project();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_file_search(
            EventTx::from_sender(tx),
            root.clone(),
            String::new(),
            SearchConfig::default(),
        );
        let hits = collect(&mut rx).await;
        handle.cancel();
        let names: Vec<String> = hits
            .iter()
            .map(|hit| hit.label(&root).replace('\\', "/"))
            .collect();
        assert!(
            names.iter().any(|name| name.ends_with("src/main.rs")),
            "got {names:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn greps_contents() {
        let root = temp_project();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_grep(
            EventTx::from_sender(tx),
            root.clone(),
            "vortex".to_string(),
            SearchConfig {
                // Force the built-in engine so the test does not depend on rg.
                ripgrep: "off".into(),
                ..SearchConfig::default()
            },
        )
        .expect("a search is started for a non-empty query");
        let hits = collect(&mut rx).await;
        handle.cancel();
        assert!(
            hits.iter()
                .any(|hit| hit.path.ends_with("main.rs") && hit.line == 2),
            "got {hits:?}"
        );
        assert!(
            !hits
                .iter()
                .any(|hit| hit.path.display().to_string().contains("target")),
            "pruned directories should be skipped: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn greps_contents_with_ripgrep_when_available() {
        // Skip when ripgrep is not installed; the built-in engine has its own test.
        if ripgrep_binary(&SearchConfig::default()).is_none() {
            return;
        }
        let root = temp_project();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_grep(
            EventTx::from_sender(tx),
            root.clone(),
            "vortex".to_string(),
            SearchConfig::default(),
        )
        .expect("a search is started for a non-empty query");
        let hits = collect(&mut rx).await;
        handle.cancel();
        assert!(
            hits.iter()
                .any(|hit| hit.path.ends_with("main.rs") && hit.line == 2),
            "ripgrep produced {hits:?}"
        );
        assert!(
            !hits
                .iter()
                .any(|hit| hit.path.display().to_string().contains("target")),
            "pruned directories should be skipped: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project whose matching lines contain colons and brackets, so a test
    /// can tell a whole matched line from a fragment of one.
    fn temp_colon_project(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vortex-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/app.rs"),
            "struct App;\nimpl App {\n    fn lookup(&self) -> Table { default() } // lookup\n    fn look(&self) -> Self { self }\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "# vortex\n\nnothing here\n").unwrap();
        root
    }

    async fn grep_with(root: &Path, pattern: &str, engine: &str) -> Vec<Hit> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_grep(
            EventTx::from_sender(tx),
            root.to_path_buf(),
            pattern.to_string(),
            SearchConfig {
                ripgrep: engine.to_string(),
                ..SearchConfig::default()
            },
        )
        .expect("a search is started for a non-empty query");
        let hits = collect(&mut rx).await;
        handle.cancel();
        hits
    }

    /// The picker lists a line only because the engine matched the pattern in
    /// it, so the text it shows has to contain the pattern.  Anything else means
    /// the engine's output was mis-parsed or the list kept stale hits.
    #[tokio::test]
    async fn every_hit_shows_a_line_containing_the_pattern() {
        for engine in ["off", "auto"] {
            let root = temp_colon_project("text");
            let hits = grep_with(&root, "lookup", engine).await;
            assert!(!hits.is_empty(), "{engine} found nothing at all");
            for hit in &hits {
                assert!(
                    hit.text.contains("lookup"),
                    "{engine}: {}:{} shows {:#?}, which does not contain the pattern",
                    hit.path.display(),
                    hit.line,
                    hit.text
                );
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Half-typed patterns (`look(`, `a|`) are not valid regexes yet.  Rather
    /// than showing nothing, ripgrep's failure is retried as a literal search.
    #[tokio::test]
    async fn an_incomplete_regex_is_searched_literally() {
        if ripgrep_binary(&SearchConfig::default()).is_none() {
            return;
        }
        let root = temp_colon_project("literal");
        let hits = grep_with(&root, "look(", "auto").await;
        assert!(
            hits.iter().any(|hit| hit.text.contains("look(")),
            "an unclosed group should be searched as the text \"look(\", got {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_ripgrep_lines() {
        let hit = parse_grep_line(
            "src/lib.rs:12:7:    let x = 1;",
            Path::new("/project"),
            true,
        )
        .unwrap();
        assert_eq!(hit.path, PathBuf::from("/project/src/lib.rs"));
        assert_eq!(hit.line, 12);
        assert_eq!(hit.column, 7);
        assert_eq!(hit.text, "    let x = 1;");
        assert!(parse_grep_line("not a result", Path::new("/"), true).is_none());
    }

    #[test]
    fn parsed_text_keeps_every_colon_in_the_matched_line() {
        // The whole rest of the line is the text, not the part up to the next
        // colon: this hit is for `lookup`, and dropping the front of the line
        // would show the user something that does not contain the pattern.
        let hit = parse_grep_line(
            "/p/src/app.rs:12:5:let lookup: Table = config::lookup();",
            Path::new("/p"),
            true,
        )
        .unwrap();
        assert_eq!(hit.line, 12);
        assert_eq!(hit.column, 5);
        assert_eq!(hit.text, "let lookup: Table = config::lookup();");
        assert!(hit.text.contains("lookup"));
        // An empty match text is still a hit.
        let empty = parse_grep_line("a.rs:1:1:", Path::new("/p"), true).unwrap();
        assert_eq!(empty.text, "");
    }

    #[test]
    fn parses_lines_without_a_column_field() {
        let hit = parse_grep_line(
            "src/app.rs:9:    let x: u8 = 1; // needle",
            Path::new("/project"),
            false,
        )
        .unwrap();
        assert_eq!(hit.path, PathBuf::from("/project/src/app.rs"));
        assert_eq!(hit.line, 9);
        assert_eq!(hit.column, 0);
        assert_eq!(hit.text, "    let x: u8 = 1; // needle");
    }

    #[test]
    fn parses_paths_that_contain_a_colon() {
        // A Windows path has a colon of its own before the line number.  Only
        // the fields are checked: on a POSIX host `resolve` cannot tell that
        // `C:\src\app.rs` is already absolute.
        let hit = parse_grep_line(
            "C:\\src\\app.rs:12:7:let x = 1;",
            Path::new("C:\\project"),
            true,
        )
        .unwrap();
        assert!(
            hit.path.to_string_lossy().ends_with("C:\\src\\app.rs"),
            "{}",
            hit.path.display()
        );
        assert_eq!(hit.line, 12);
        assert_eq!(hit.column, 7);
        assert_eq!(hit.text, "let x = 1;");
    }

    #[test]
    fn exit_statuses_that_are_not_failures() {
        // ripgrep exits 1 when nothing matched: an empty list, not an error.
        assert!(engine_error("ripgrep", Some(1), "").is_none());
        assert!(engine_error("ripgrep", Some(0), "").is_none());
        let error =
            engine_error("ripgrep", Some(2), "rg: regex parse error:\n    (?:()\n").unwrap();
        assert!(error.contains("regex parse error"), "{error}");
        assert!(
            engine_error("ripgrep", Some(2), "")
                .unwrap()
                .contains("exit 2")
        );
    }

    #[test]
    fn project_root_prefers_the_git_directory() {
        let root = temp_project();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let found = project_root(Some(&root.join("src/main.rs")));
        assert_eq!(found, root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn project_root_resolves_relative_arguments_against_the_cwd() {
        // `vortex src/main.rs` from the project directory: the relative
        // argument must still find the git root, not an empty path.
        let root = temp_project();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let found = project_root_in(Some(Path::new("src/main.rs")), &root);
        assert_eq!(found, root);
        // Without a git root the result is the file's absolute directory.
        let _ = std::fs::remove_dir_all(root.join(".git"));
        let found = project_root_in(Some(Path::new("src/main.rs")), &root);
        assert_eq!(found, root.join("src"));
        let found = project_root_in(Some(Path::new("README.md")), &root);
        assert_eq!(found, root);
        // A file that does not exist yet still resolves to its directory.
        let found = project_root_in(Some(Path::new("src/new.rs")), &root);
        assert_eq!(found, root.join("src"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
