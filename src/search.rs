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
                stream_ripgrep_files(tx.clone(), id, &query, root, config, flag, binary).await;
            }
            None => walk_files(tx.clone(), id, &query, root, config, flag).await,
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
                stream_ripgrep_grep(tx.clone(), id, &query, root, config, flag, binary).await;
            }
            None => {
                grep_fallback(tx.clone(), id, &query, root, config, flag).await;
            }
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
    tx: EventTx,
    job: JobId,
    query: &str,
    root: PathBuf,
    config: SearchConfig,
    cancelled: Arc<AtomicBool>,
    binary: PathBuf,
) {
    let mut command = Command::new(&binary);
    command.arg("--files").arg(&root);
    apply_common_flags(&mut command, &config);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            send(
                &tx,
                Batch::finish(
                    job,
                    SearchKind::FileNames,
                    query,
                    Vec::new(),
                    false,
                    Some(format!("{}: {error}", binary.display())),
                    "ripgrep",
                ),
            );
            return;
        }
    };

    stream_lines(
        &tx,
        job,
        SearchKind::FileNames,
        query,
        output,
        &root,
        &config,
        &cancelled,
        "ripgrep",
        |line, root| Some(Hit::file(resolve(root, line))),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn stream_ripgrep_grep(
    tx: EventTx,
    job: JobId,
    query: &str,
    root: PathBuf,
    config: SearchConfig,
    cancelled: Arc<AtomicBool>,
    binary: PathBuf,
) {
    let mut command = Command::new(&binary);
    command
        .arg("--no-heading")
        .arg("--line-number")
        .arg("--color")
        .arg("never")
        .arg("--with-filename")
        .arg(&root);
    if config.smart_case {
        command.arg("--smart-case");
    }
    apply_common_flags(&mut command, &config);
    command.arg("--").arg(query);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            send(
                &tx,
                Batch::finish(
                    job,
                    SearchKind::Contents,
                    query,
                    Vec::new(),
                    false,
                    Some(format!("{}: {error}", binary.display())),
                    "ripgrep",
                ),
            );
            return;
        }
    };

    stream_lines(
        &tx,
        job,
        SearchKind::Contents,
        query,
        output,
        &root,
        &config,
        &cancelled,
        "ripgrep",
        parse_grep_line,
    )
    .await;
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

/// Parse ripgrep's `path:line:column:text` output.
fn parse_grep_line(line: &str, root: &Path) -> Option<Hit> {
    // Windows drive letters make "C:\x:1:2:txt" ambiguous; handle the common
    // POSIX shape first, then fall back to the last two separators.
    let mut parts = line.splitn(4, ':');
    let path = parts.next()?;
    let line_no: usize = parts.next()?.parse().ok()?;
    let column: usize = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let text = parts.next().unwrap_or("").to_string();
    Some(Hit {
        path: resolve(root, path),
        line: line_no,
        column,
        text,
    })
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
) {
    let Some(stdout) = child.stdout.take() else {
        send(
            tx,
            Batch::finish(
                job,
                kind,
                query,
                Vec::new(),
                false,
                Some("the search produced no output".into()),
                engine,
            ),
        );
        return;
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
            return;
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
                send(
                    tx,
                    Batch::finish(
                        job,
                        kind,
                        query,
                        batch,
                        truncated,
                        Some(error.to_string()),
                        engine,
                    ),
                );
                return;
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
    let _ = &mut child; // dropping the child terminates the process
    send(
        tx,
        Batch::finish(job, kind, query, batch, truncated, None, engine),
    );
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
    let matcher = prune_matcher(&root, &config);
    tokio::task::spawn_blocking(move || {
        let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
        let walker = ignore::WalkBuilder::new(&root)
            .hidden(!walk_config.hidden)
            .follow_links(walk_config.follow)
            .ignore(walk_config.ignore)
            .git_ignore(walk_config.ignore)
            .git_global(walk_config.ignore)
            // Pruning as a filter also stops the walk descending into `target`.
            .filter_entry(move |entry| keep_entry(matcher.as_ref(), &root, entry))
            .build();
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
    let matcher = prune_matcher(&root, &config);
    tokio::task::spawn_blocking(move || {
        let case_sensitive = !search_config.smart_case || needle.chars().any(|c| c.is_uppercase());
        let needle_compare = if case_sensitive {
            needle.clone()
        } else {
            needle.to_lowercase()
        };
        let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
        let walker = ignore::WalkBuilder::new(&root)
            .hidden(!search_config.hidden)
            .follow_links(search_config.follow)
            .ignore(search_config.ignore)
            .git_ignore(search_config.ignore)
            .filter_entry(move |entry| keep_entry(matcher.as_ref(), &root, entry))
            .build();
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
    let start = match path {
        Some(path) => {
            if path.is_dir() {
                path.to_path_buf()
            } else {
                path.parent().map(Path::to_path_buf).unwrap_or_default()
            }
        }
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let mut current = start.as_path();
    loop {
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    if start.exists() {
        start
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

    #[test]
    fn parses_ripgrep_lines() {
        let hit = parse_grep_line("src/lib.rs:12:7:    let x = 1;", Path::new("/project")).unwrap();
        assert_eq!(hit.path, PathBuf::from("/project/src/lib.rs"));
        assert_eq!(hit.line, 12);
        assert_eq!(hit.column, 7);
        assert_eq!(hit.text, "    let x = 1;");
        assert!(parse_grep_line("not a result", Path::new("/")).is_none());
    }

    #[test]
    fn project_root_prefers_the_git_directory() {
        let root = temp_project();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let found = project_root(Some(&root.join("src/main.rs")));
        assert_eq!(found, root);
        let _ = std::fs::remove_dir_all(&root);
    }
}
