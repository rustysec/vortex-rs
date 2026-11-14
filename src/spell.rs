//! Spell checking.
//!
//! Two engines produce the same [`Diagnostic`] stream, so everything downstream —
//! the curly underline, the gutter mark, `z=` — is identical either way:
//!
//! * the built-in word list, an edit-distance search over a small embedded
//!   dictionary, which needs nothing installed;
//! * aspell or hunspell in their shared ispell pipe mode, chosen by
//!   `[spell] command` and located on `PATH`, for real prose work.
//!
//! Either way the tokenizer here owns the offsets: only the words vortex decided
//! are prose are handed to the engine, one per line, and each misspelling in the
//! report is matched back to the occurrence it came from.  An external checker
//! therefore never reports a position and cannot drift out of step with the
//! buffer, which is what makes `only-comments` trustworthy.
//!
//! Checking runs as a background job and reports through [`AsyncEvent::Spelled`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::buffer::BufferId;
use crate::config::SpellConfig;
use crate::diagnostic::{Diagnostic, DiagnosticSource, Severity};
use crate::event::{AsyncEvent, EventTx, JobId};

static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_job_id() -> JobId {
    JobId(NEXT_JOB.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// A spell-check run for one buffer.
#[derive(Debug, Clone)]
pub struct SpellResult {
    pub buffer: BufferId,
    pub version: u64,
    pub job: JobId,
    pub diagnostics: Vec<Diagnostic>,
    pub error: Option<String>,
}

/// Which checker a run uses, as resolved from `[spell] command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// The embedded word list.  Needs nothing installed, and is what runs when
    /// no external engine was asked for or none is on `PATH`.
    Builtin,
    /// `aspell pipe`.
    Aspell(PathBuf),
    /// `hunspell -a`.
    Hunspell(PathBuf),
}

impl Engine {
    /// Name for the status message when a run fails.
    fn label(&self) -> &'static str {
        match self {
            Engine::Builtin => "the built-in word list",
            Engine::Aspell(_) => "aspell",
            Engine::Hunspell(_) => "hunspell",
        }
    }
}

/// Work out which engine a run should use.
///
/// `"auto"` (the default) prefers aspell, then hunspell, and falls back to the
/// embedded list when neither is installed — so a machine without either still
/// gets hints.  Naming an engine explicitly means *that* one: if it is missing
/// the run reports the problem instead of quietly checking with something else,
/// because a document checked by the wrong dictionary reads as clean.
pub fn resolve_engine(config: &SpellConfig) -> Result<Engine, String> {
    // A missing engine is worth telling the user how to get hints back, since the
    // alternative they are looking for is one keystroke away.
    let missing = |name: &str| {
        format!(
            "{name} is not installed; set [spell] command to auto or off to use the built-in list"
        )
    };
    match config.command.trim().to_lowercase().as_str() {
        "" | "off" | "none" | "builtin" | "internal" => Ok(Engine::Builtin),
        "auto" => Ok(which("aspell")
            .map(Engine::Aspell)
            .or_else(|| which("hunspell").map(Engine::Hunspell))
            .unwrap_or(Engine::Builtin)),
        "aspell" => which("aspell")
            .map(Engine::Aspell)
            .ok_or_else(|| missing("aspell")),
        "hunspell" => which("hunspell")
            .map(Engine::Hunspell)
            .ok_or_else(|| missing("hunspell")),
        other => Err(format!(
            "[spell] command expects auto, aspell, hunspell or off, got \"{other}\""
        )),
    }
}

/// Find `program` on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    if program.contains(['/', '\\']) {
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

/// One run's addressing, so every way a check can finish — built-in list,
/// external engine, or an engine that was never there — sends the same event and
/// the caller cannot forget a field.
#[derive(Clone)]
struct Reporter {
    tx: EventTx,
    buffer: BufferId,
    version: u64,
    job: JobId,
}

impl Reporter {
    fn report(&self, diagnostics: Vec<Diagnostic>, error: Option<String>) {
        self.tx.send_async(AsyncEvent::Spelled(SpellResult {
            buffer: self.buffer,
            version: self.version,
            job: self.job,
            diagnostics,
            error,
        }));
    }
}

/// Kick off a spell check.  `text` is a snapshot of the buffer, so the offsets
/// in the resulting diagnostics are buffer offsets.
///
/// `prose` is the comment and string regions the checker is allowed to look at,
/// when `[spell] only-comments` is on.  `None` checks the whole buffer.  The
/// spans are computed on the main thread from the buffer's tree, because the
/// tree is not `Send` and the job only has the text.
pub fn spawn_spell(
    tx: EventTx,
    buffer: BufferId,
    version: u64,
    text: String,
    prose: Option<Vec<crate::selection::Span>>,
    config: SpellConfig,
) -> JobId {
    let job = next_job_id();
    let reporter = Reporter {
        tx,
        buffer,
        version,
        job,
    };
    match resolve_engine(&config) {
        // The embedded list is pure CPU work over a static table, so it stays on
        // the blocking pool where a big buffer cannot stall the runtime.  An
        // external engine waits on a process instead, which wants the runtime.
        Ok(Engine::Builtin) => {
            tokio::task::spawn_blocking(move || {
                let diagnostics = check_scoped(&text, &config, prose.as_deref());
                reporter.report(diagnostics, None);
            });
        }
        Ok(engine) => {
            tokio::spawn(async move {
                match check_with_engine(&engine, &text, &config, prose.as_deref()).await {
                    Ok(diagnostics) => reporter.report(diagnostics, None),
                    Err(error) => {
                        reporter.report(Vec::new(), Some(format!("{}: {error}", engine.label())))
                    }
                }
            });
        }
        // A misconfigured `[spell] command` is worth a status line even though it
        // starts no work; empty diagnostics clear whatever the last run left.
        Err(error) => reporter.report(Vec::new(), Some(error)),
    }
    job
}

/// The word list, loaded once per process.
///
/// Only English is bundled.  Another locale means a different word list, which is
/// a data file rather than a code change; until then the built-in checker stays
/// English and reports `Hint` diagnostics only.
fn dictionary() -> &'static WordList {
    static EN: OnceLock<WordList> = OnceLock::new();
    EN.get_or_init(|| WordList::from_words(BASE_WORDS))
}

/// A set of words with a prefix index for cheap "is this a stem?" checks.
pub struct WordList {
    words: HashMap<String, ()>,
    stems: Vec<String>,
    /// Extra words from the project's `.vortex-words` file in the working
    /// directory — the dictionary is embedded, so project vocabulary is the
    /// one thing a project needs to add.
    extras: Vec<String>,
}

impl WordList {
    fn from_words(words: &[&str]) -> Self {
        let list: Vec<String> = words.iter().map(|word| word.to_string()).collect();
        Self {
            words: list.iter().cloned().map(|word| (word, ())).collect(),
            stems: list.clone(),
            extras: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Add project/user words.
    pub fn with_extra(mut self, extra: &[&str]) -> Self {
        self.extras
            .extend(extra.iter().map(|word| word.to_string()));
        self
    }

    fn contains(&self, word: &str) -> bool {
        self.words.contains_key(word)
            || self.extras.iter().any(|extra| extra == word)
            // A clitic is not part of the word: `line's` is the known word `line`
            // with a possessive attached, and flagging it suggested `line`, which
            // reads as correcting a word to its own spelling.
            || self.has_clitic_base(word)
            // Very common inflections of a known word.
            || self.words.contains_key(&format!("{word}s"))
            || self.words.contains_key(&format!("{word}e"))
            || self.words.contains_key(&format!("{word}es"))
            || (word.ends_with('s') && self.words.contains_key(word.trim_end_matches('s')))
            || (word.ends_with("ing") && self.has_stem(word, 3))
            || (word.ends_with("ed") && self.has_stem(word, 2))
            || (word.ends_with("ly") && self.has_stem(word, 2))
    }

    /// Whether the part before an apostrophe is a known word: `line's`, `pane's`,
    /// `don't`.  The tokenizer keeps the apostrophe inside the token, so without
    /// this every possessive was a misspelling whose best suggestion was the same
    /// letters — which reads as being told to correct a word to itself.
    fn has_clitic_base(&self, word: &str) -> bool {
        let Some((base, rest)) = word.split_once('\'') else {
            return false;
        };
        // A trailing apostrophe is trimmed by the tokenizer, and an empty base
        // (`'twas`) is not a known word with a clitic.  `s'` after a plural has
        // already lost its apostrophe by the time it gets here.
        !base.is_empty() && rest.chars().all(|c| c.is_alphabetic()) && self.contains(base)
    }

    fn has_stem(&self, word: &str, suffix: usize) -> bool {
        let stem = &word[..word.len() - suffix];
        self.words.contains_key(stem)
            || self.words.contains_key(&format!("{stem}e"))
            || self
                .stems
                .iter()
                .any(|candidate| candidate.as_str() == stem || candidate.starts_with(stem))
    }

    /// Words close enough to `word` to be plausible corrections.
    pub fn suggestions(&self, word: &str, limit: usize) -> Vec<String> {
        // An exact hit leads the list: `z=` on a correct word says "it's fine".
        if self.contains(word) {
            return vec![word.to_string()];
        }
        let max_distance = if word.len() <= 4 { 1 } else { 2 };
        let mut scored: Vec<(usize, String)> = self
            .words
            .keys()
            .chain(self.extras.iter())
            .filter_map(|candidate| {
                let distance = bounded_distance(word, candidate, max_distance)?;
                Some((distance, candidate.clone()))
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        scored.truncate(limit);
        scored.into_iter().map(|(_, word)| word).collect()
    }
}

const SCHEMES: &[&str] = &[
    "http", "https", "ftp", "ftps", "file", "git", "mailto", "ssh", "ws", "wss", "mailto",
];

/// Largest number of diagnostics one run reports, so a huge file of identifiers
/// cannot flood the status area.
const MAX_DIAGNOSTICS: usize = 2_000;

/// Spell check `text`, producing diagnostics addressed by byte offset.
///
/// This checks the whole buffer.  The editor uses [`check_scoped`] instead when
/// `[spell] only-comments` is on, so code identifiers are not reported.
pub fn check(text: &str, config: &SpellConfig) -> Vec<Diagnostic> {
    check_scoped(text, config, None)
}

/// Spell check the parts of `text` that fall inside `prose`, with the embedded
/// word list.  [`check_with_engine`] is the same pass through aspell or hunspell.
///
/// `prose` is the comment and string regions from the syntax tree; a word is
/// checked only when its start lies inside one of them.  `None` checks the whole
/// buffer, which is what prose files (markdown, plain text) and the un-scoped
/// setting want.
pub fn check_scoped(
    text: &str,
    config: &SpellConfig,
    prose: Option<&[crate::selection::Span]>,
) -> Vec<Diagnostic> {
    let base = dictionary();
    let extras = load_user_words();
    let with_extras = WordList {
        words: base.words.clone(),
        stems: base.stems.clone(),
        extras,
    };
    let list = if with_extras.extras.is_empty() {
        base
    } else {
        &with_extras
    };

    let mut diagnostics = Vec::new();
    for (word, start) in candidates(text, config, prose) {
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            break;
        }
        let probe = lookup(&word, config);
        if list.contains(&probe) {
            continue;
        }
        let suggestions = list.suggestions(&probe, SUGGESTION_LIMIT);
        diagnostics.push(misspelling(&word, start, suggestions));
    }
    diagnostics
}

/// The words worth checking, with the byte offset each starts at.
///
/// Both engines are fed from here, so switching checker changes only what says a
/// word is wrong — never which words get looked at.  Everything that is not prose
/// goes first: text outside a comment or string (which would report every
/// identifier), URL schemes and hosts, and the code-ish tokens [`skip`] rejects.
fn candidates(
    text: &str,
    config: &SpellConfig,
    prose: Option<&[crate::selection::Span]>,
) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for (word, byte, _length) in words(text).into_iter() {
        if found.len() >= MAX_DIAGNOSTICS {
            break;
        }
        // Only look at prose the syntax tree marked as a comment or a string.
        // Checking the rest is what reported every identifier and macro as a
        // misspelling, which `only-comments` exists to prevent.
        if let Some(spans) = prose
            && !spans.iter().any(|span| span.contains(byte))
        {
            continue;
        }
        let end = byte + word.len();
        // `https://…`, `mailto:…`: the scheme and the host are not prose.
        if text[end..].starts_with("://") || SCHEMES.contains(&word.to_lowercase().as_str()) {
            continue;
        }
        if skip(&word, config) {
            continue;
        }
        found.push((word, byte));
    }
    found
}

/// The form of a word a checker is asked about.
///
/// Case-sensitive mode keeps the word as typed; otherwise everything is compared
/// lowercased so `The` matches `the`.  The embedded dictionary is all lowercase,
/// and an external engine gets the same folded form so both agree on what a user
/// configured.
fn lookup(word: &str, config: &SpellConfig) -> String {
    if config.case_sensitive {
        word.to_string()
    } else {
        word.to_lowercase()
    }
}

/// A `Hint` diagnostic for a word one of the checkers does not know.  Both build
/// it here, so the message and the `z=` list read the same either way.
fn misspelling(word: &str, start: usize, mut suggestions: Vec<String>) -> Diagnostic {
    suggestions.truncate(SUGGESTION_LIMIT);
    let message = if suggestions.is_empty() {
        format!("possible misspelling: {word}")
    } else {
        format!("possible misspelling: {word}: {}", suggestions.join(", "))
    };
    Diagnostic::new(
        crate::selection::Span::new(start, start + word.len()),
        Severity::Hint,
        message,
        DiagnosticSource::Spell,
    )
    .with_suggestions(suggestions)
}

/// How many corrections one diagnostic offers.
const SUGGESTION_LIMIT: usize = 6;

/// Seconds an external engine gets before its run is given up on.
const ENGINE_TIMEOUT_SECS: u64 = 10;

/// Spell check `text` with aspell or hunspell instead of the embedded list.
///
/// The engines are asked about *words*, not positions: the candidates go out one
/// per line and each report is matched back to the occurrences it came from, so
/// every offset in the result is still a buffer offset from our own tokenizer.
/// That is what keeps `only-comments` honest — an engine never sees code, and
/// never gets to say where in the buffer a word lives.
async fn check_with_engine(
    engine: &Engine,
    text: &str,
    config: &SpellConfig,
    prose: Option<&[crate::selection::Span]>,
) -> Result<Vec<Diagnostic>, String> {
    // `.vortex-words` is vortex's own project vocabulary; an installed engine has
    // never heard of it, so those words are dropped before the run rather than
    // reported as misspellings the user already declared fine.
    let project = load_user_words();
    let found = candidates(text, config, prose);

    // One line per distinct word: a comment that says `renderer` twenty times is
    // one lookup, not twenty.
    let mut asked: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (word, _start) in &found {
        let probe = lookup(word, config);
        if project.contains(&probe) || !seen.insert(probe.clone()) {
            continue;
        }
        asked.push(probe);
    }
    if asked.is_empty() {
        return Ok(Vec::new());
    }

    let report = run_engine(engine, &asked, &config.language).await?;
    let mut misses: HashMap<String, Vec<String>> = HashMap::new();
    for miss in parse_pipe_report(&report) {
        misses.entry(miss.word).or_insert(miss.suggestions);
    }

    let mut diagnostics = Vec::new();
    for (word, start) in &found {
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            break;
        }
        let probe = lookup(word, config);
        // The engines report the form they were sent, which is what `lookup`
        // produced; matching on it puts the underline back on the typed word.
        if let Some(suggestions) = misses.get(&probe) {
            diagnostics.push(misspelling(word, *start, suggestions.clone()));
        }
    }
    Ok(diagnostics)
}

/// Ask an external engine about `words`, one per line, and return its report.
///
/// Both engines speak the ispell pipe format, so one call site and one parser
/// cover them; only the argument spelling differs.
async fn run_engine(engine: &Engine, words: &[String], language: &str) -> Result<String, String> {
    // `pipe` is aspell's filter mode; `-a` is hunspell's ispell interface.  The
    // encoding flag keeps the report UTF-8 whatever locale the terminal that
    // started vortex was in, since a mangled suggestion is worse than none.
    let mut command = match engine {
        Engine::Aspell(binary) => {
            let mut command = Command::new(binary);
            command.arg("--encoding=utf-8").arg("pipe");
            command
        }
        Engine::Hunspell(binary) => {
            let mut command = Command::new(binary);
            command.arg("-a");
            command
        }
        Engine::Builtin => return Err("the built-in list needs no process".to_string()),
    };
    if lets_choose(language) {
        match engine {
            Engine::Aspell(_) => {
                command.arg(format!("--lang={language}"));
            }
            Engine::Hunspell(_) => {
                command.args(["-d", language]);
            }
            Engine::Builtin => {}
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| format!("{}: {error}", engine.label()))?;

    if let Some(mut stdin) = child.stdin.take() {
        let payload = format!("{}\n", words.join("\n"));
        tokio::spawn(async move {
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.flush().await;
            // Closing stdin is what tells the engine it has seen everything.
            let _ = stdin.shutdown().await;
        });
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(ENGINE_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| format!("{} timed out after {ENGINE_TIMEOUT_SECS}s", engine.label()))?
    .map_err(|error| error.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.trim();
        return Err(if reason.is_empty() {
            format!("{} exited with {}", engine.label(), output.status)
        } else {
            reason.to_string()
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `[spell] language` names a dictionary, or leaves it to the engine.
///
/// `"auto"` (the default) sends no language flag: aspell and hunspell each have
/// their own idea of the user's default dictionary, and guessing one here would
/// break installs whose dictionary is named something else (`en_US`, `en_GB-ize`).
fn lets_choose(language: &str) -> bool {
    !language.trim().is_empty() && !language.eq_ignore_ascii_case("auto")
}

/// One misspelling from an ispell pipe report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Miss {
    /// The word as it was sent, which is how it is matched back to its span.
    word: String,
    suggestions: Vec<String>,
}

/// Read aspell's or hunspell's report.
///
/// Both mark a misspelling with `&` when they have corrections and `#` when they
/// do not: `& teh 3 7, 4: the, then, tea`.  Everything else is chatter — the
/// version banner, `*` and `@` lines for words that were fine.  The byte offsets
/// in a record point into *what we sent*, not the buffer, so they are ignored: the
/// spans come from our own tokenizer, which is the whole reason the words are fed
/// one per line instead of as text.
fn parse_pipe_report(report: &str) -> Vec<Miss> {
    let mut misses = Vec::new();
    for line in report.lines() {
        let Some(rest) = line.strip_prefix("& ").or_else(|| line.strip_prefix("# ")) else {
            continue;
        };
        // The offsets end at the first `: `, after which the corrections are
        // comma-separated.  A `#` record has no suggestions and so no colon.
        let (head, tail) = match rest.split_once(": ") {
            Some((head, tail)) => (head, Some(tail)),
            None => (rest, None),
        };
        let Some(word) = head.split_whitespace().next() else {
            continue;
        };
        let suggestions = tail
            .map(|tail| {
                tail.split(',')
                    .map(str::trim)
                    .filter(|suggestion| !suggestion.is_empty())
                    .map(str::to_string)
                    .take(SUGGESTION_LIMIT)
                    .collect()
            })
            .unwrap_or_default();
        misses.push(Miss {
            word: word.to_string(),
            suggestions,
        });
    }
    misses
}

/// Decide whether a token is even a word worth checking.
fn skip(word: &str, config: &SpellConfig) -> bool {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() < config.min_len {
        return true;
    }
    if !chars.iter().any(|c| c.is_alphabetic()) || chars.iter().all(|c| c.is_numeric()) {
        return true;
    }
    // Paths and dotted names are code, not prose.
    if word.contains('/') || word.contains('\\') || word.contains('.') || word.contains('_') {
        return true;
    }
    // `SCREAMING_CASE` is a constant; `CamelCase` is a type or method name.
    if config.ignore_camel_case {
        if word.chars().all(|c| c.is_uppercase()) {
            return true;
        }
        if word.chars().next().is_some_and(char::is_uppercase) {
            return true;
        }
        if word.chars().filter(|c| c.is_uppercase()).count() > 1 {
            return true;
        }
    }
    false
}

/// Yield `(word, byte_offset, byte_length)` for each word token.
///
/// Walks characters rather than bytes: taking `&text[start..end]` at an index
/// inside `…` or `é` panics, and real files are full of them.
pub fn words(text: &str) -> Vec<(String, usize, usize)> {
    let mut found = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, first)) = chars.next() {
        if !word_char(first) {
            continue;
        }
        let mut end = start + first.len_utf8();
        while let Some(&(at, ch)) = chars.peek() {
            if word_char(ch) {
                end = at + ch.len_utf8();
                chars.next();
            } else if ch == '\'' && end > start {
                // `don't` and `runner's` are one word; a trailing apostrophe is
                // punctuation and is trimmed below.
                end = at + 1;
                chars.next();
            } else {
                break;
            }
        }
        let word = text[start..end].trim_end_matches('\'');
        if !word.is_empty() {
            found.push((word.to_string(), start, word.len()));
        }
    }
    found
}

/// Letters that count as prose.  Latin script only: Cyrillic, Greek and CJK are
/// alphabetic but not English, and every one of them would be reported as a
/// misspelling by an English dictionary.
fn word_char(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch, '\u{c0}'..='\u{24f}' | '\u{1e00}'..='\u{1eff}')
}

/// Project words from `.vortex-words` in the working directory.
fn load_user_words() -> Vec<String> {
    let mut words = Vec::new();
    if let Ok(text) = std::fs::read_to_string(".vortex-words") {
        words.extend(text.split_whitespace().map(|word| word.to_lowercase()));
    }
    words
}

/// Levenshtein distance, stopping early once `max` is exceeded.
fn bounded_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let left: Vec<char> = a.chars().collect();
    let right: Vec<char> = b.chars().collect();
    if left.is_empty() || right.is_empty() {
        return None;
    }
    if left[0] != right[0] && max < 1 {
        return None;
    }
    if left.len().abs_diff(right.len()) > max {
        return None;
    }

    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];

    for (i, &a_char) in left.iter().enumerate() {
        current[0] = i + 1;
        let mut row_min = current[0];
        for (j, &b_char) in right.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            row_min = row_min.min(current[j + 1]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[right.len()];
    (distance <= max && distance > 0).then_some(distance)
}

/// Ask the runtime to drop the dictionary caches (used by `:config source`).
pub fn forget_dictionaries() {
    // Dictionaries are `OnceLock` statics; reloading them would need interior
    // mutability.  The editor restarts on `:config source`, which is cheaper
    // than keeping the cache mutable.
}

/// Small English word list used by default (a fuller dictionary would be a data
/// file; this keeps the binary self-contained).
///
/// Missing words surface as hints rather than errors, so false negatives are
/// cheap and false positives are the thing to avoid.
const BASE_WORDS: &[&str] = &[
    // everyday English, the words a comment is mostly made of
    "able",
    "about",
    "across",
    "after",
    "again",
    "against",
    "almost",
    "alone",
    "along",
    "already",
    "although",
    "always",
    "among",
    "another",
    "answer",
    "anyone",
    "anything",
    "around",
    "because",
    "become",
    "before",
    "behind",
    "being",
    "below",
    "beside",
    "best",
    "better",
    "between",
    "beyond",
    "both",
    "bring",
    "brought",
    "build",
    "calling",
    "came",
    "cannot",
    "care",
    "certain",
    "change",
    "changes",
    "check",
    "chose",
    "choose",
    "clean",
    "clear",
    "clearly",
    "close",
    "comes",
    "common",
    "consider",
    "contain",
    "continue",
    "correct",
    "could",
    "country",
    "create",
    "current",
    "currently",
    "default",
    "derive",
    "describe",
    "design",
    "detail",
    "determine",
    "develop",
    "does",
    "doing",
    "done",
    "drive",
    "during",
    "easily",
    "each",
    "either",
    "else",
    "enough",
    "enter",
    "entire",
    "error",
    "especially",
    "establish",
    "even",
    "eventually",
    "every",
    "example",
    "expect",
    "expert",
    "fails",
    "field",
    "final",
    "finally",
    "find",
    "first",
    "follow",
    "following",
    "force",
    "found",
    "frame",
    "frequently",
    "friend",
    "fully",
    "further",
    "general",
    "given",
    "gives",
    "going",
    "great",
    "group",
    "happens",
    "hard",
    "having",
    "help",
    "hidden",
    "holds",
    "however",
    "huge",
    "immediately",
    "important",
    "inside",
    "instead",
    "itself",
    "just",
    "keeps",
    "known",
    "large",
    "later",
    "least",
    "leave",
    "leaving",
    "least",
    "less",
    "levels",
    "like",
    "limits",
    "lines",
    "little",
    "located",
    "long",
    "looks",
    "made",
    "makes",
    "meaning",
    "means",
    "might",
    "minor",
    "misses",
    "moment",
    "more",
    "most",
    "much",
    "must",
    "name",
    "needs",
    "neither",
    "never",
    "nevertheless",
    "next",
    "nice",
    "none",
    "normally",
    "north",
    "note",
    "nothing",
    "notice",
    "often",
    "once",
    "only",
    "open",
    "opposite",
    "orders",
    "other",
    "others",
    "otherwise",
    "ourselves",
    "outside",
    "over",
    "part",
    "particular",
    "perhaps",
    "piece",
    "place",
    "please",
    "point",
    "possible",
    "power",
    "prefer",
    "present",
    "probably",
    "problem",
    "provides",
    "puts",
    "quite",
    "rate",
    "rather",
    "reachable",
    "reason",
    "receive",
    "record",
    "regard",
    "region",
    "remaining",
    "remove",
    "replaced",
    "report",
    "request",
    "return",
    "right",
    "roughly",
    "same",
    "several",
    "shall",
    "shift",
    "should",
    "shows",
    "side",
    "since",
    "single",
    "smaller",
    "snapshot",
    "some",
    "something",
    "sometimes",
    "somewhere",
    "source",
    "special",
    "spell",
    "start",
    "state",
    "still",
    "stop",
    "stress",
    "such",
    "takes",
    "tested",
    "than",
    "themselves",
    "thing",
    "think",
    "those",
    "though",
    "thought",
    "three",
    "through",
    "thus",
    "today",
    "together",
    "toward",
    "tried",
    "tries",
    "truly",
    "turned",
    "typical",
    "under",
    "understand",
    "unless",
    "until",
    "upon",
    "used",
    "user",
    "uses",
    "using",
    "usually",
    "valid",
    "value",
    "various",
    "very",
    "views",
    "visible",
    "wait",
    "want",
    "way",
    "well",
    "went",
    "whatever",
    "whether",
    "while",
    "whole",
    "whose",
    "why",
    "window",
    "within",
    "without",
    "words",
    "works",
    "would",
    "written",
    "wrote",
    "year",
    // words that show up in code, comments and docs
    "abort",
    "abortable",
    "abortive",
    "absent",
    "abstract",
    "accept",
    "access",
    "account",
    "active",
    "actual",
    "add",
    "added",
    "address",
    "allow",
    "allowed",
    "allowing",
    "args",
    "array",
    "arrays",
    "arrow",
    "assert",
    "assign",
    "assume",
    "attempt",
    "attempts",
    "auth",
    "author",
    "available",
    "avoid",
    "backend",
    "back",
    "base",
    "batch",
    "binary",
    "bit",
    "bits",
    "block",
    "blocks",
    "bool",
    "boolean",
    "bottom",
    "break",
    "buffer",
    "buffers",
    "builder",
    "byte",
    "bytes",
    "cache",
    "call",
    "called",
    "calling",
    "calls",
    "cancel",
    "canonical",
    "careful",
    "case",
    "cases",
    "chain",
    "char",
    "character",
    "characters",
    "charset",
    "chunk",
    "chunks",
    "cli",
    "client",
    "clip",
    "clipboard",
    "clone",
    "code",
    "column",
    "columns",
    "comes",
    "command",
    "commands",
    "commit",
    "compare",
    "complete",
    "component",
    "compute",
    "computed",
    "concat",
    "condition",
    "config",
    "configs",
    "const",
    "consume",
    "contact",
    "content",
    "contents",
    "context",
    "continue",
    "contract",
    "convert",
    "copy",
    "core",
    "count",
    "counter",
    "counts",
    "crate",
    "crlf",
    "cursor",
    "cursors",
    "cycle",
    "data",
    "database",
    "debug",
    "decode",
    "decoded",
    "decrease",
    "default",
    "define",
    "defined",
    "delay",
    "delete",
    "deleted",
    "depend",
    "depth",
    "derive",
    "derived",
    "desc",
    "destroy",
    "detect",
    "detection",
    "dir",
    "dirs",
    "doc",
    "docs",
    "document",
    "documents",
    "domain",
    "edge",
    "elem",
    "element",
    "elements",
    "email",
    "embed",
    "empty",
    "enable",
    "enabled",
    "encode",
    "encoded",
    "end",
    "endpoint",
    "ends",
    "entry",
    "enum",
    "env",
    "eof",
    "equal",
    "equivalent",
    "err",
    "error",
    "errors",
    "escape",
    "escaped",
    "event",
    "events",
    "exact",
    "example",
    "examples",
    "except",
    "exchange",
    "exec",
    "expect",
    "expire",
    "explode",
    "export",
    "expr",
    "extension",
    "external",
    "fallback",
    "fast",
    "fetch",
    "field",
    "fields",
    "file",
    "files",
    "fill",
    "filter",
    "final",
    "find",
    "finish",
    "fire",
    "flag",
    "flags",
    "float",
    "flow",
    "focus",
    "folder",
    "font",
    "force",
    "format",
    "formatter",
    "found",
    "frame",
    "frames",
    "free",
    "fresh",
    "full",
    "function",
    "future",
    "gap",
    "generates",
    "generic",
    "get",
    "gets",
    "getter",
    "global",
    "glyph",
    "graph",
    "grep",
    "handler",
    "hard",
    "has",
    "head",
    "header",
    "help",
    "hex",
    "highlight",
    "hint",
    "hints",
    "hit",
    "hits",
    "hook",
    "host",
    "html",
    "http",
    "https",
    "huge",
    "icon",
    "id",
    "ident",
    "identifier",
    "identifiers",
    "idx",
    "ignore",
    "ignored",
    "image",
    "impl",
    "import",
    "important",
    "in",
    "include",
    "index",
    "info",
    "init",
    "initial",
    "input",
    "insert",
    "install",
    "instance",
    "int",
    "integer",
    "internal",
    "iter",
    "iterable",
    "iterate",
    "iterator",
    "job",
    "join",
    "json",
    "keys",
    "lang",
    "large",
    "last",
    "latest",
    "layer",
    "layout",
    "leader",
    "len",
    "length",
    "level",
    "lib",
    "limit",
    "line",
    "lines",
    "link",
    "lint",
    "list",
    "lists",
    "load",
    "local",
    "location",
    "lock",
    "log",
    "logic",
    "look",
    "loop",
    "loss",
    "lower",
    "machine",
    "main",
    "make",
    "map",
    "markup",
    "match",
    "matches",
    "max",
    "member",
    "memory",
    "menu",
    "merge",
    "mesh",
    "method",
    "min",
    "mode",
    "model",
    "module",
    "mount",
    "mouse",
    "move",
    "names",
    "navigation",
    "need",
    "nested",
    "network",
    "node",
    "nodes",
    "none",
    "normal",
    "not",
    "number",
    "numeric",
    "object",
    "offset",
    "once",
    "open",
    "operation",
    "option",
    "options",
    "order",
    "origin",
    "out",
    "output",
    "over",
    "overflow",
    "own",
    "pack",
    "pad",
    "page",
    "panic",
    "param",
    "parameter",
    "params",
    "parent",
    "parse",
    "parsed",
    "parser",
    "part",
    "partial",
    "pass",
    "passed",
    "path",
    "paths",
    "pattern",
    "pause",
    "peer",
    "pending",
    "percent",
    "perform",
    "persist",
    "pick",
    "picker",
    "pixel",
    "place",
    "plain",
    "pop",
    "position",
    "positive",
    "prefix",
    "prepare",
    "print",
    "priority",
    "proc",
    "process",
    "program",
    "project",
    "prompt",
    "prop",
    "properties",
    "property",
    "proto",
    "push",
    "put",
    "query",
    "queue",
    "quote",
    "quoted",
    "raw",
    "read",
    "ready",
    "realm",
    "redraw",
    "reduce",
    "ref",
    "reference",
    "region",
    "register",
    "registry",
    "reject",
    "rel",
    "release",
    "reload",
    "remote",
    "remove",
    "rename",
    "render",
    "repeated",
    "replace",
    "reply",
    "repo",
    "report",
    "request",
    "require",
    "reset",
    "resize",
    "resolve",
    "resource",
    "response",
    "result",
    "resume",
    "retry",
    "return",
    "reuse",
    "reverse",
    "revert",
    "rewrite",
    "role",
    "root",
    "rotate",
    "row",
    "rows",
    "rpc",
    "rule",
    "run",
    "runtime",
    "safe",
    "save",
    "scan",
    "scenario",
    "scene",
    "schema",
    "scope",
    "screen",
    "scroll",
    "search",
    "section",
    "seek",
    "seg",
    "segment",
    "select",
    "self",
    "sep",
    "sequence",
    "server",
    "session",
    "set",
    "sets",
    "setup",
    "shell",
    "shift",
    "show",
    "side",
    "signal",
    "size",
    "slice",
    "slot",
    "snapshot",
    "sort",
    "source",
    "space",
    "span",
    "spec",
    "speed",
    "spell",
    "split",
    "stack",
    "start",
    "state",
    "static",
    "stats",
    "status",
    "step",
    "stop",
    "storage",
    "store",
    "string",
    "strings",
    "strong",
    "struct",
    "style",
    "sub",
    "submit",
    "subset",
    "substr",
    "substring",
    "suffix",
    "suggest",
    "suggests",
    "summary",
    "supports",
    "sync",
    "syntax",
    "system",
    "tab",
    "table",
    "tail",
    "take",
    "target",
    "task",
    "team",
    "temp",
    "template",
    "term",
    "terminal",
    "test",
    "tested",
    "text",
    "texts",
    "theme",
    "thing",
    "tile",
    "time",
    "timeout",
    "title",
    "todo",
    "token",
    "tokens",
    "top",
    "topic",
    "total",
    "trace",
    "track",
    "trailing",
    "transaction",
    "trim",
    "true",
    "trunc",
    "try",
    "tu",
    "tuple",
    "turn",
    "type",
    "types",
    "ui",
    "undefined",
    "unit",
    "union",
    "unknown",
    "unlink",
    "unset",
    "unsigned",
    "unsync",
    "update",
    "upper",
    "url",
    "uri",
    "usage",
    "use",
    "used",
    "useful",
    "user",
    "users",
    "uses",
    "using",
    "util",
    "utility",
    "value",
    "values",
    "var",
    "valid",
    "validate",
    "value",
    "variable",
    "vec",
    "vector",
    "verify",
    "version",
    "view",
    "views",
    "virtual",
    "visit",
    "void",
    "warn",
    "warning",
    "watch",
    "weak",
    "width",
    "widget",
    "word",
    "words",
    "work",
    "world",
    "wrap",
    "write",
    "written",
    "yield",
    "and",
    "are",
    "as",
    "at",
    "be",
    "but",
    "by",
    "for",
    "from",
    "had",
    "has",
    "have",
    "here",
    "into",
    "is",
    "it",
    "its",
    "of",
    "on",
    "or",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "to",
    "was",
    "we",
    "were",
    "what",
    "when",
    "which",
    "who",
    "will",
    "with",
    "would",
    "you",
    "your",
    // words that show up in code comments and docs
    "abort",
    "abortable",
    "abortive",
    "about",
    "above",
    "absent",
    "abstract",
    "accept",
    "access",
    "account",
    "active",
    "actual",
    "add",
    "added",
    "address",
    "after",
    "against",
    "all",
    "allow",
    "allowed",
    "allowing",
    "allows",
    "almost",
    "alone",
    "along",
    "already",
    "always",
    "am",
    "amount",
    "an",
    "anal",
    "analyze",
    "another",
    "any",
    "appear",
    "apply",
    "approach",
    "arg",
    "args",
    "argument",
    "arguments",
    "array",
    "arrays",
    "arrow",
    "assert",
    "assign",
    "assume",
    "attempt",
    "attempts",
    "auth",
    "author",
    "available",
    "avoid",
    "back",
    "backend",
    "background",
    "bad",
    "base",
    "based",
    "basic",
    "because",
    "become",
    "been",
    "before",
    "begin",
    "beginning",
    "behind",
    "being",
    "below",
    "best",
    "better",
    "between",
    "big",
    "bind",
    "bit",
    "bits",
    "block",
    "blocked",
    "blocking",
    "blocks",
    "bool",
    "boot",
    "both",
    "bound",
    "buffer",
    "buffers",
    "bug",
    "bugs",
    "build",
    "builder",
    "built",
    "byte",
    "bytes",
    "call",
    "called",
    "calling",
    "calls",
    "came",
    "can",
    "cannot",
    "case",
    "cases",
    "cause",
    "cell",
    "center",
    "certain",
    "change",
    "changed",
    "changes",
    "changing",
    "channel",
    "char",
    "check",
    "checked",
    "checking",
    "checks",
    "choose",
    "chunk",
    "chunks",
    "claim",
    "class",
    "clean",
    "clear",
    "cleared",
    "clears",
    "click",
    "client",
    "close",
    "closed",
    "code",
    "collect",
    "column",
    "combine",
    "comes",
    "command",
    "commands",
    "common",
    "compare",
    "complete",
    "completed",
    "component",
    "compute",
    "concat",
    "config",
    "configure",
    "configured",
    "confirm",
    "connect",
    "consider",
    "consists",
    "contain",
    "container",
    "content",
    "contents",
    "context",
    "continue",
    "contract",
    "control",
    "convert",
    "copy",
    "copying",
    "core",
    "correct",
    "correspond",
    "count",
    "counter",
    "create",
    "created",
    "creates",
    "creating",
    "current",
    "currently",
    "cursor",
    "custom",
    "data",
    "database",
    "date",
    "day",
    "default",
    "defaults",
    "define",
    "defined",
    "delay",
    "delete",
    "deleted",
    "deletes",
    "demo",
    "depend",
    "describe",
    "described",
    "design",
    "detail",
    "detect",
    "determine",
    "develop",
    "diagnostic",
    "diagnostics",
    "diff",
    "differ",
    "different",
    "differentiate",
    "display",
    "distinct",
    "do",
    "document",
    "documentation",
    "does",
    "done",
    "down",
    "download",
    "drive",
    "drop",
    "dropped",
    "during",
    "each",
    "early",
    "edit",
    "edited",
    "editing",
    "edition",
    "editor",
    "editors",
    "edges",
    "element",
    "empty",
    "enable",
    "enabled",
    "end",
    "ended",
    "ends",
    "enforce",
    "engine",
    "enough",
    "ensure",
    "entire",
    "entry",
    "equal",
    "error",
    "errors",
    "escape",
    "especially",
    "establish",
    "even",
    "event",
    "events",
    "every",
    "exact",
    "example",
    "except",
    "exchange",
    "exist",
    "existing",
    "exit",
    "expect",
    "expected",
    "experience",
    "explain",
    "explicit",
    "explicitly",
    "export",
    "extend",
    "extension",
    "external",
    "fact",
    "fallback",
    "false",
    "feature",
    "fetch",
    "field",
    "fields",
    "file",
    "files",
    "fill",
    "final",
    "finally",
    "find",
    "finding",
    "fine",
    "first",
    "fix",
    "fixed",
    "flags",
    "flow",
    "follow",
    "following",
    "follows",
    "font",
    "forbid",
    "force",
    "form",
    "format",
    "formatted",
    "formatting",
    "found",
    "frame",
    "free",
    "from",
    "full",
    "function",
    "functions",
    "further",
    "game",
    "gave",
    "general",
    "generate",
    "generic",
    "get",
    "gets",
    "getting",
    "given",
    "global",
    "good",
    "got",
    "grammar",
    "grant",
    "graph",
    "greater",
    "group",
    "grow",
    "handle",
    "handled",
    "handler",
    "handles",
    "happen",
    "hard",
    "has",
    "hat",
    "have",
    "head",
    "header",
    "help",
    "her",
    "here",
    "hers",
    "high",
    "higher",
    "him",
    "his",
    "hold",
    "home",
    "host",
    "how",
    "however",
    "huge",
    "human",
    "hundred",
    "idea",
    "identify",
    "if",
    "ignore",
    "ignored",
    "image",
    "immediate",
    "implement",
    "implementation",
    "implemented",
    "important",
    "improve",
    "include",
    "included",
    "includes",
    "including",
    "increase",
    "index",
    "info",
    "information",
    "initial",
    "initialize",
    "init",
    "inline",
    "input",
    "insert",
    "inserted",
    "inserting",
    "insertion",
    "inside",
    "install",
    "instance",
    "instead",
    "int",
    "integer",
    "internal",
    "into",
    "introduce",
    "issue",
    "issues",
    "item",
    "items",
    "its",
    "itself",
    "join",
    "just",
    "keep",
    "kept",
    "key",
    "keys",
    "kind",
    "kinds",
    "know",
    "known",
    "large",
    "largest",
    "last",
    "late",
    "later",
    "latest",
    "layout",
    "lead",
    "leading",
    "leaf",
    "least",
    "leave",
    "leaving",
    "left",
    "length",
    "less",
    "let",
    "letter",
    "level",
    "library",
    "like",
    "limit",
    "limited",
    "line",
    "lines",
    "link",
    "list",
    "lists",
    "little",
    "load",
    "loaded",
    "local",
    "location",
    "lock",
    "log",
    "logic",
    "long",
    "look",
    "looking",
    "looks",
    "loop",
    "lose",
    "loss",
    "lost",
    "lot",
    "low",
    "lower",
    "main",
    "maintain",
    "major",
    "make",
    "makes",
    "making",
    "manual",
    "many",
    "map",
    "mapped",
    "matches",
    "match",
    "max",
    "maybe",
    "mean",
    "means",
    "measure",
    "media",
    "member",
    "memory",
    "menu",
    "message",
    "messages",
    "method",
    "methods",
    "might",
    "mime",
    "minimal",
    "minimum",
    "minor",
    "miss",
    "mode",
    "model",
    "models",
    "modern",
    "modify",
    "more",
    "morning",
    "most",
    "move",
    "moved",
    "moves",
    "moving",
    "much",
    "must",
    "name",
    "named",
    "names",
    "need",
    "needed",
    "needs",
    "network",
    "never",
    "new",
    "newer",
    "newline",
    "next",
    "nice",
    "night",
    "no",
    "node",
    "nodes",
    "none",
    "nor",
    "normal",
    "normally",
    "not",
    "note",
    "notes",
    "nothing",
    "now",
    "number",
    "numbers",
    "object",
    "objects",
    "occupy",
    "offset",
    "offsets",
    "once",
    "one",
    "only",
    "open",
    "opened",
    "opening",
    "opens",
    "operate",
    "operation",
    "operations",
    "operator",
    "operators",
    "option",
    "optional",
    "options",
    "order",
    "ordered",
    "origin",
    "original",
    "other",
    "others",
    "otherwise",
    "our",
    "out",
    "outside",
    "over",
    "own",
    "page",
    "pages",
    "pane",
    "panes",
    "part",
    "partial",
    "particular",
    "partly",
    "parts",
    "pass",
    "passed",
    "past",
    "pattern",
    "patterns",
    "pause",
    "peak",
    "pending",
    "perform",
    "perhaps",
    "period",
    "persist",
    "persistent",
    "pick",
    "piece",
    "place",
    "placed",
    "plain",
    "plan",
    "plans",
    "point",
    "points",
    "possible",
    "post",
    "prefer",
    "prepare",
    "present",
    "pretty",
    "previous",
    "previous",
    "prevent",
    "print",
    "printed",
    "printing",
    "prints",
    "priority",
    "probably",
    "problem",
    "problems",
    "process",
    "produce",
    "produced",
    "program",
    "project",
    "provide",
    "public",
    "pull",
    "purpose",
    "push",
    "put",
    "puts",
    "quality",
    "query",
    "queue",
    "quit",
    "quite",
    "quote",
    "quoted",
    "quotes",
    "race",
    "radio",
    "raise",
    "range",
    "ranges",
    "rate",
    "rather",
    "raw",
    "read",
    "reader",
    "reading",
    "ready",
    "real",
    "really",
    "reason",
    "receive",
    "record",
    "records",
    "red",
    "reduce",
    "refer",
    "reference",
    "refresh",
    "register",
    "registers",
    "regular",
    "related",
    "relations",
    "release",
    "reload",
    "remove",
    "removed",
    "removes",
    "rename",
    "render",
    "rendered",
    "rendering",
    "repeat",
    "replace",
    "replaced",
    "report",
    "represent",
    "request",
    "requests",
    "require",
    "required",
    "requires",
    "research",
    "reset",
    "resize",
    "resized",
    "resolve",
    "resource",
    "respect",
    "response",
    "restore",
    "result",
    "results",
    "return",
    "returned",
    "returning",
    "returns",
    "reuse",
    "review",
    "rich",
    "right",
    "rim",
    "ripe",
    "rise",
    "risk",
    "river",
    "role",
    "roll",
    "root",
    "row",
    "rows",
    "run",
    "running",
    "runs",
    "safe",
    "same",
    "save",
    "saved",
    "say",
    "scheme",
    "school",
    "screen",
    "scroll",
    "scrolled",
    "scrolling",
    "search",
    "searched",
    "searches",
    "second",
    "seconds",
    "section",
    "sections",
    "see",
    "seek",
    "seem",
    "seen",
    "select",
    "selected",
    "selecting",
    "selection",
    "selections",
    "self",
    "sell",
    "send",
    "sending",
    "sense",
    "sensitive",
    "sent",
    "separate",
    "sequence",
    "serial",
    "series",
    "service",
    "session",
    "set",
    "sets",
    "setting",
    "settings",
    "seven",
    "several",
    "shall",
    "shape",
    "share",
    "shared",
    "sharp",
    "shift",
    "ship",
    "short",
    "shorten",
    "should",
    "show",
    "showed",
    "shown",
    "shows",
    "side",
    "significant",
    "similar",
    "simple",
    "since",
    "single",
    "sit",
    "site",
    "size",
    "sizes",
    "slow",
    "small",
    "smallest",
    "smooth",
    "so",
    "social",
    "soft",
    "software",
    "sold",
    "solution",
    "some",
    "something",
    "sometimes",
    "sort",
    "source",
    "space",
    "spaces",
    "special",
    "specific",
    "split",
    "splits",
    "spool",
    "square",
    "stack",
    "stage",
    "stand",
    "standard",
    "start",
    "started",
    "starting",
    "state",
    "statement",
    "states",
    "static",
    "station",
    "status",
    "step",
    "steps",
    "stop",
    "stopped",
    "store",
    "stored",
    "stores",
    "story",
    "straight",
    "strange",
    "stream",
    "street",
    "strength",
    "string",
    "strings",
    "strong",
    "structure",
    "style",
    "styles",
    "subject",
    "success",
    "such",
    "suggest",
    "suggested",
    "suggests",
    "summary",
    "support",
    "supported",
    "supports",
    "sure",
    "surface",
    "switch",
    "symbol",
    "system",
    "table",
    "tables",
    "take",
    "takes",
    "taking",
    "target",
    "task",
    "tasks",
    "teach",
    "tech",
    "tell",
    "template",
    "terminal",
    "test",
    "tested",
    "testing",
    "tests",
    "text",
    "than",
    "the",
    "theirs",
    "theme",
    "then",
    "theory",
    "there",
    "these",
    "they",
    "thick",
    "thin",
    "thing",
    "things",
    "think",
    "third",
    "those",
    "though",
    "thought",
    "thousand",
    "three",
    "through",
    "throw",
    "thus",
    "time",
    "times",
    "tiny",
    "title",
    "to",
    "today",
    "together",
    "told",
    "tone",
    "too",
    "took",
    "tool",
    "tools",
    "top",
    "topic",
    "total",
    "touch",
    "toward",
    "track",
    "trade",
    "train",
    "transact",
    "transfer",
    "translate",
    "transport",
    "travel",
    "tree",
    "trees",
    "trigger",
    "trim",
    "true",
    "trust",
    "try",
    "trying",
    "turn",
    "turned",
    "turning",
    "turns",
    "two",
    "type",
    "types",
    "typical",
    "unable",
    "unified",
    "until",
    "up",
    "update",
    "updated",
    "upon",
    "user",
    "users",
    "usual",
    "usually",
    "util",
    "utility",
    "validate",
    "value",
    "values",
    "var",
    "variable",
    "variables",
    "vec",
    "vector",
    "verify",
    "version",
    "very",
    "view",
    "views",
    "visit",
    "void",
    "vote",
    "wait",
    "walk",
    "want",
    "wanted",
    "warm",
    "was",
    "watch",
    "water",
    "way",
    "ways",
    "we",
    "well",
    "went",
    "were",
    "west",
    "what",
    "wheel",
    "when",
    "where",
    "whether",
    "which",
    "while",
    "white",
    "who",
    "whole",
    "whom",
    "why",
    "wide",
    "width",
    "wild",
    "will",
    "window",
    "windows",
    "wish",
    "within",
    "without",
    "word",
    "words",
    "work",
    "working",
    "works",
    "world",
    "would",
    "write",
    "writer",
    "writes",
    "writing",
    "written",
    "wrong",
    "wrote",
    "year",
    "years",
    "yet",
    "you",
    "young",
    "your",
    "yours",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_and_unknown_words() {
        let config = SpellConfig {
            min_len: 2,
            ..SpellConfig::default()
        };
        let text = "compute the vlue\n";
        let diagnostics = check(text, &config);
        assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
        assert!(diagnostics[0].message.contains("vlue"));
        assert!(!diagnostics[0].suggestions.is_empty());
    }

    #[test]
    fn identifiers_and_numbers_are_ignored() {
        let config = SpellConfig::default();
        let text = "let x = compute_total(42); // https://example.com/path\n";
        assert!(check(text, &config).is_empty());
    }

    #[test]
    fn suggestions_are_closest_words() {
        let list = dictionary();
        let suggestions = list.suggestions("compute", 3);
        assert!(suggestions.iter().any(|word| word == "compute"));
        let near = list.suggestions("compte", 5);
        assert!(near.iter().any(|word| word == "compute"), "got {near:?}");
    }

    #[test]
    fn bounded_distance_stops_early() {
        assert_eq!(bounded_distance("abc", "abd", 1), Some(1));
        assert_eq!(bounded_distance("abc", "xyz", 1), None);
        assert_eq!(bounded_distance("same", "same", 2), None); // identical is not a suggestion
    }

    #[test]
    fn word_tokenizer_handles_apostrophes() {
        let tokens: Vec<String> = words("don't stop here")
            .into_iter()
            .map(|(word, _, _)| word)
            .collect();
        assert_eq!(tokens, ["don't", "stop", "here"]);
    }

    #[test]
    fn possessives_are_not_corrected_to_their_own_spelling() {
        // `line's` used to be flagged with `line` among the suggestions, which
        // reads as being told to fix a word into the same spelling.  A clitic is
        // not part of the word, so the possessive of a known word is known too.
        let config = SpellConfig {
            min_len: 2,
            ..SpellConfig::default()
        };
        let text = "the line's pane's don't stop\n";
        let diagnostics = check(text, &config);
        assert!(diagnostics.is_empty(), "got {diagnostics:?}");

        // `z=` on one of those still says "it's fine" rather than offering a fix.
        assert_eq!(dictionary().suggestions("line's", 3), ["line's"]);
    }

    #[test]
    fn unknown_words_with_a_clitic_are_still_flagged() {
        // Accepting `line's` must not accept every apostrophe: the base still has
        // to be a word, and the misspelling is never suggested back verbatim.
        let config = SpellConfig {
            min_len: 2,
            ..SpellConfig::default()
        };
        let diagnostics = check("the znort's here\n", &config);
        assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
        let flagged = &"the znort's here"[diagnostics[0].range.start..diagnostics[0].range.end];
        assert_eq!(flagged, "znort's");
        for suggestion in &diagnostics[0].suggestions {
            assert_ne!(suggestion, flagged, "{suggestion} is not a correction");
        }
    }

    #[test]
    fn scoped_checking_ignores_words_outside_the_prose_spans() {
        // `only-comments` means a word is checked only when it starts inside a
        // comment or string.  The code identifier `vlue` sits outside the span
        // and must be skipped; the misspelling inside the comment must not.
        let config = SpellConfig {
            min_len: 2,
            ..SpellConfig::default()
        };
        let text = "let vlue = 1; // the vlue is here\n";
        let comment = text.find("//").unwrap();
        let prose = [crate::selection::Span::new(comment, text.len())];
        let diagnostics = check_scoped(text, &config, Some(&prose));
        assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
        assert_eq!(diagnostics[0].range.start, comment + "// the ".len());
    }

    #[test]
    fn unscoped_checking_still_sees_the_whole_buffer() {
        // `None` is the "check everything" path, for markdown and plain text.
        let config = SpellConfig {
            min_len: 2,
            ..SpellConfig::default()
        };
        let text = "let vlue = 1;\n";
        let diagnostics = check_scoped(text, &config, None);
        assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
    }

    #[test]
    fn case_sensitivity_changes_which_words_are_flagged() {
        // `ignore-camel-case` also skips capitalised words, so it is off here:
        // the point is what case sensitivity does on its own.
        let base = SpellConfig {
            min_len: 2,
            only_comments: false,
            ignore_camel_case: false,
            ..SpellConfig::default()
        };
        // `World` is the dictionary word `world` capitalised.  Lowercasing
        // before the lookup (the default) accepts it; matching case-sensitively
        // rejects it, since the dictionary is all lowercase.
        let text = "World world\n";
        let lenient = check_scoped(text, &base, None);
        assert!(
            lenient.is_empty(),
            "case-insensitive checking accepts a capitalised word: {lenient:?}"
        );

        let strict = SpellConfig {
            case_sensitive: true,
            ..base
        };
        let diagnostics = check_scoped(text, &strict, None);
        assert_eq!(
            diagnostics.len(),
            1,
            "only `World` is rejected: {diagnostics:?}"
        );
        assert_eq!(
            &text[diagnostics[0].range.start..diagnostics[0].range.end],
            "World"
        );
    }

    #[test]
    fn pipe_report_is_read_for_both_engines() {
        // aspell and hunspell share the ispell report: `&` with corrections, `#`
        // without.  The version banner, the `*` lines for accepted words and the
        // offsets — which count into what vortex sent, not the buffer — are all
        // noise here.
        let report = "@(#) International Ispell Version 3.2.06 (but really Hunspell 1.7.6)\n\
                      *\n\
                      & teh 4 0: the, then, tea, tech\n\
                      # zzqx 1 0\n";
        let misses = parse_pipe_report(report);
        assert_eq!(misses.len(), 2, "got {misses:?}");
        assert_eq!(misses[0].word, "teh");
        assert_eq!(misses[0].suggestions, ["the", "then", "tea", "tech"]);
        assert_eq!(misses[1].word, "zzqx");
        assert!(misses[1].suggestions.is_empty(), "no corrections offered");
    }

    #[test]
    fn pipe_report_trims_suggestions_to_the_list_limit() {
        let words: Vec<String> = (0..12).map(|number| number.to_string()).collect();
        let long = format!("& teh 9 0: {}", words.join(", "));
        let misses = parse_pipe_report(&long);
        assert_eq!(misses[0].suggestions.len(), SUGGESTION_LIMIT, "{misses:?}");
    }

    #[test]
    fn command_selects_the_engine() {
        let named = |command: &str| {
            resolve_engine(&SpellConfig {
                command: command.into(),
                ..SpellConfig::default()
            })
        };
        // `off` means "do not shell out", which is testable on any machine; the
        // master switch for checking at all stays `[spell] enable`.
        assert_eq!(named("off"), Ok(Engine::Builtin));
        assert_eq!(named("builtin"), Ok(Engine::Builtin));
        assert_eq!(named(""), Ok(Engine::Builtin));
        // `auto` always produces something: a machine with neither engine gets the
        // embedded list rather than an error on every idle tick.
        assert!(named("auto").is_ok(), "auto must never fail");
        let unknown = named("enchant").expect_err("a typo is refused");
        assert!(unknown.contains("enchant"), "{unknown}");
    }

    #[test]
    fn only_a_named_language_reaches_the_engine() {
        // `auto` leaves the choice to aspell/hunspell, whose dictionary names vary
        // by distribution (`en_US`, `en_GB-ize`, …).
        assert!(!lets_choose("auto"));
        assert!(!lets_choose(""));
        assert!(lets_choose("en_GB"));
    }

    /// A run started through the editor's entry point reports on the event channel,
    /// whichever engine it ended up using.
    async fn one_run(config: SpellConfig) -> SpellResult {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_spell(
            EventTx::from_sender(tx),
            BufferId(1),
            7,
            "let compute_total = 1; // the vlue is here\n".to_string(),
            None,
            config,
        );
        let event = rx.recv().await.expect("a result is sent");
        match event {
            crate::event::Event::Async(AsyncEvent::Spelled(result)) => result,
            other => panic!("expected a spell result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_built_in_list_reports_through_the_same_channel() {
        let result = one_run(SpellConfig {
            command: "off".into(),
            min_len: 2,
            ..SpellConfig::default()
        })
        .await;
        assert_eq!(result.version, 7);
        assert_eq!(result.error, None, "{result:?}");
        let text = "let compute_total = 1; // the vlue is here\n";
        let flagged: Vec<&str> = result
            .diagnostics
            .iter()
            .map(|diagnostic| &text[diagnostic.range.start..diagnostic.range.end])
            .collect();
        assert!(flagged.contains(&"vlue"), "got {flagged:?}");
    }

    #[tokio::test]
    async fn a_misconfigured_command_reports_instead_of_checking() {
        // Silence here would look like a buffer with nothing wrong in it.
        let result = one_run(SpellConfig {
            command: "enchant".into(),
            ..SpellConfig::default()
        })
        .await;
        assert!(result.diagnostics.is_empty(), "{result:?}");
        let error = result.error.expect("the bad value is named");
        assert!(error.contains("enchant"), "{error}");
    }

    /// The real engine, when one is installed: the words are ours and so are the
    /// offsets, which is the property a fixture cannot check.
    #[tokio::test]
    async fn external_engine_flags_prose_and_keeps_buffer_offsets() {
        let config = SpellConfig {
            command: "auto".into(),
            min_len: 2,
            ..SpellConfig::default()
        };
        let engine = resolve_engine(&config).expect("auto always resolves");
        if engine == Engine::Builtin {
            eprintln!("neither aspell nor hunspell is installed; nothing to check");
            return;
        }

        // `vlue` inside the comment is a misspelling either way; `compute_total`
        // outside it must never reach the engine at all.
        let text = "let compute_total = 1; // the vlue is here\n";
        let comment = text.find("//").unwrap();
        let prose = [crate::selection::Span::new(comment, text.len())];
        let diagnostics = check_with_engine(&engine, text, &config, Some(&prose))
            .await
            .expect("the installed engine runs");

        let flagged: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| &text[diagnostic.range.start..diagnostic.range.end])
            .collect();
        assert!(flagged.contains(&"vlue"), "got {flagged:?}");
        assert!(!flagged.contains(&"compute_total"), "got {flagged:?}");
        for diagnostic in &diagnostics {
            let span = diagnostic.range;
            assert!(
                text.is_char_boundary(span.start) && text.is_char_boundary(span.end),
                "{span:?} is not on a char boundary"
            );
            assert_eq!(diagnostic.source, DiagnosticSource::Spell);
        }
    }

    /// One unknown token, nothing else in the buffer: the report has to be matched
    /// back to the single occurrence rather than dropped for want of context.
    #[tokio::test]
    async fn external_engine_flags_a_lone_unknown_token() {
        let config = SpellConfig {
            command: "auto".into(),
            min_len: 2,
            ..SpellConfig::default()
        };
        let engine = resolve_engine(&config).expect("auto always resolves");
        if engine == Engine::Builtin {
            eprintln!("no external engine installed; nothing to check");
            return;
        }
        let diagnostics = check_with_engine(&engine, "// zzzqqxx\n", &config, None)
            .await
            .expect("the installed engine runs");
        assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
    }
}
