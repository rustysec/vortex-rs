//! Events: terminal input plus results from background jobs.
//!
//! Terminal input is read on a dedicated blocking thread and pushed into a
//! `tokio` unbounded channel, so the async main loop never blocks on the tty.
//! Long running work (ripgrep, formatters, tree-sitter parses, spell checking)
//! reports back through the same channel as [`Event::Async`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
};
use tokio::sync::mpsc;

use crate::buffer::BufferId;
use crate::format::FormatResult;
use crate::lsp::LspEvent;
use crate::search::SearchEvent;
use crate::spell::SpellResult;
use crate::syntax::TreeSlot;

/// Identifies a background job so late results can be matched with their prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

impl JobId {
    pub fn new(id: u64) -> Self {
        JobId(id)
    }
}

/// Something the editor reacts to.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Paste(String),
    Resize {
        width: u16,
        height: u16,
    },
    Mouse(MouseEvent),
    /// Periodic wake-up: message timeouts, format debounce, key-sequence expiry.
    Tick,
    Async(AsyncEvent),
    /// The event loop should stop.
    Quit,
}

/// Results delivered by background tasks.
#[derive(Debug, Clone)]
pub enum AsyncEvent {
    /// A tree-sitter parse finished for `buffer` at `version`.
    ///
    /// The tree itself travels through `slot`: `tree_sitter::Tree` is `Send` but
    /// not `Sync`, so it cannot be shared — only handed over.
    Parsed {
        buffer: BufferId,
        version: u64,
        slot: TreeSlot,
    },
    /// A formatter finished.
    Formatted(FormatResult),
    /// ripgrep (or the internal walker) produced results.
    Search(SearchEvent),
    /// The spell checker finished.
    Spelled(SpellResult),
    /// Language server traffic (scaffolded).
    Lsp(LspEvent),
    /// The git branch probe finished (`None` outside a repository).
    GitBranch(Option<String>),
    /// A one-off message from a background task.
    Message(String),
    /// A background task failed in a way the user should hear about.
    Failed { what: String, error: String },
}

/// Cheaply cloneable sender used by tasks and the terminal reader.
#[derive(Clone)]
pub struct EventTx {
    tx: mpsc::UnboundedSender<Event>,
}

impl EventTx {
    /// Wrap a channel the caller already owns — used by tests and by anything
    /// that builds its own event stream.
    pub fn from_sender(tx: mpsc::UnboundedSender<Event>) -> Self {
        Self { tx }
    }

    pub fn send(&self, event: Event) {
        // The only failure mode is a dead main loop: nothing useful left to do.
        let _ = self.tx.send(event);
    }

    pub fn send_async(&self, event: AsyncEvent) {
        self.send(Event::Async(event));
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Owns the terminal reader thread and the receiving end of the event channel.
pub struct Events {
    rx: Option<mpsc::UnboundedReceiver<Event>>,
    tx: EventTx,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Events {
    /// Start reading terminal events on a background thread.
    pub fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = EventTx { tx };
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let tx = tx.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut reports = ReportFilter::default();
                while !stop.load(Ordering::Relaxed) {
                    match event::poll(Duration::from_millis(80)) {
                        Ok(true) => match event::read() {
                            Ok(ev) => {
                                for out in reports.feed(ev) {
                                    tx.send(out);
                                }
                            }
                            Err(err) => {
                                tx.send(Event::Async(AsyncEvent::Failed {
                                    what: "terminal input".into(),
                                    error: err.to_string(),
                                }));
                                return;
                            }
                        },
                        Ok(false) => continue,
                        Err(ref err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(err) => {
                            tx.send(Event::Async(AsyncEvent::Failed {
                                what: "terminal input".into(),
                                error: err.to_string(),
                            }));
                            return;
                        }
                    }
                }
            })
        };

        Self {
            rx: Some(rx),
            tx,
            stop,
            reader: Some(reader),
        }
    }

    pub fn sender(&self) -> EventTx {
        self.tx.clone()
    }

    /// Next event, if one is already queued.
    pub fn try_recv(&mut self) -> Option<Event> {
        self.rx.as_mut()?.try_recv().ok()
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.as_mut()?.recv().await
    }

    /// Move the receiver out, so a `select!` branch can poll it while the handler
    /// holds `&mut App`.
    pub fn take_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.rx.take()
    }

    /// Put it back so the reader thread is joined on drop.
    pub fn put_receiver(&mut self, receiver: mpsc::UnboundedReceiver<Event>) {
        self.rx = Some(receiver);
    }

    /// Stop the reader thread.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn translate(ev: CrosstermEvent) -> Option<Event> {
    match ev {
        CrosstermEvent::Key(key) => Some(Event::Key(key)),
        CrosstermEvent::Paste(text) => Some(Event::Paste(text)),
        CrosstermEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        CrosstermEvent::Mouse(mouse) => Some(Event::Mouse(mouse)),
        // Focus events carry no meaning here.
        CrosstermEvent::FocusGained | CrosstermEvent::FocusLost => None,
    }
}

/// How much of a suspected report to hold before deciding it is not one.
///
/// A colour report is ~30 bytes; anything longer than this is the user typing
/// after an `Alt-]`, not a terminal answering a query.
const MAX_REPORT_BYTES: usize = 64;

/// Swallows a terminal query report that arrives after
/// [`crate::term::query_colors`] stopped reading.
///
/// crossterm has no OSC parsing, so it hands a reply back as ordinary keys:
/// `ESC ]` becomes `Alt-]` and every byte of `11;rgb:f8f8/f8f8/f2f2` a
/// character.  In a prompt those characters type themselves into the command
/// line; in normal mode they are *commands* — `r` replaces the character under
/// the caret with a `g` — so a reply that lands late can quietly edit the file.
///
/// The run is held rather than dropped on sight: it goes back unchanged unless
/// the bytes really do parse as a colour report, which means an `Alt-]` chord
/// followed by anything else costs nothing but a keystroke of latency.  What
/// this cannot see is a reply cut in half — the tail of one has no `ESC ]` to
/// mark it, and looks exactly like typing.  [`crate::term::query_colors`] keeps
/// those from reaching here in the first place.
#[derive(Debug, Default)]
struct ReportFilter {
    /// Raw bytes of the run in flight, as the terminal sent them.
    bytes: Vec<u8>,
    /// The keys those bytes came in as, kept in order in case they go back.
    pending: Vec<KeyEvent>,
}

impl ReportFilter {
    /// One parsed event in, the events to forward out (in order, possibly none).
    fn feed(&mut self, ev: CrosstermEvent) -> Vec<Event> {
        let CrosstermEvent::Key(key) = ev else {
            // Anything that is not a key ends the run: it cannot be part of a
            // report, so give the held keys back before handling it.
            let mut out = self.flush();
            out.extend(translate(ev));
            return out;
        };
        if self.bytes.is_empty() {
            if !is_report_open(key) {
                return vec![Event::Key(key)];
            }
            self.bytes = b"\x1b]".to_vec();
            self.pending = vec![key];
            return Vec::new();
        }
        let Some(kind) = report_byte(key) else {
            // Not a byte a report can contain: it was typing after all.
            let mut out = self.flush();
            out.push(Event::Key(key));
            return out;
        };
        let terminated = match kind {
            ReportByte::One(0x07) => {
                self.bytes.push(0x07);
                true
            }
            ReportByte::One(byte) => {
                self.bytes.push(byte);
                false
            }
            ReportByte::StringTerminator => {
                self.bytes.extend_from_slice(b"\x1b\\");
                true
            }
        };
        self.pending.push(key);
        if !terminated && self.bytes.len() < MAX_REPORT_BYTES {
            return Vec::new();
        }
        let report = crate::term::parse_report(&self.bytes).is_some();
        let keys = self.flush();
        if report { Vec::new() } else { keys }
    }

    /// Hand the held keys back and forget the run.
    fn flush(&mut self) -> Vec<Event> {
        self.bytes.clear();
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(Event::Key)
            .collect()
    }
}

/// The opening of a report: `ESC ]`, which crossterm reads as `Alt-]`.
fn is_report_open(key: KeyEvent) -> bool {
    key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Char(']'))
}

/// The byte or bytes a key stands for inside a report.
#[derive(Debug, Clone, Copy)]
enum ReportByte {
    /// A character of the payload, the BEL terminator, or a lone `ESC`.
    One(u8),
    /// The string terminator `ESC \`, which crossterm reads as one `Alt-\`.
    StringTerminator,
}

/// The byte (or bytes) a key stands for inside a report, or `None` when the key
/// cannot belong to one at all.
fn report_byte(key: KeyEvent) -> Option<ReportByte> {
    match key.code {
        // The BEL terminator, which crossterm reads as Ctrl-G.
        KeyCode::Char('g') if key.modifiers == KeyModifiers::CONTROL => Some(ReportByte::One(0x07)),
        KeyCode::Char('\\') if key.modifiers == KeyModifiers::ALT => {
            Some(ReportByte::StringTerminator)
        }
        KeyCode::Esc if key.modifiers.is_empty() => Some(ReportByte::One(0x1b)),
        KeyCode::Char(ch)
            if key.modifiers.is_empty()
                || (key.modifiers == KeyModifiers::SHIFT && ch.is_ascii_uppercase()) =>
        {
            // Digits, the hex channels, `rgb:`, `#rrggbb` and the separators.
            if ch.is_ascii_alphanumeric() || matches!(ch, ':' | '/' | ';' | '#' | '.' | '-') {
                Some(ReportByte::One(ch as u8))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, modifiers))
    }

    /// The keys crossterm makes of a run of plain bytes: one `Char` per byte,
    /// with SHIFT on capitals (its `char_code_to_event`).
    fn typed(text: &str) -> Vec<CrosstermEvent> {
        text.chars()
            .map(|ch| {
                let modifiers = if ch.is_ascii_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::empty()
                };
                key(KeyCode::Char(ch), modifiers)
            })
            .collect()
    }

    /// `ESC ] 11 ; rgb:… BEL`, the reply to the background colour query.
    /// `ESC ]` arrives as a single `Alt-]`, BEL as Ctrl-G.
    fn background_report() -> Vec<CrosstermEvent> {
        let mut events = vec![key(KeyCode::Char(']'), KeyModifiers::ALT)];
        events.extend(typed("11;rgb:f8f8/f8f8/f2f2"));
        events.push(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        events
    }

    fn feed(events: impl IntoIterator<Item = CrosstermEvent>) -> Vec<Event> {
        let mut filter = ReportFilter::default();
        events.into_iter().flat_map(|ev| filter.feed(ev)).collect()
    }

    fn codes(events: &[Event]) -> Vec<KeyCode> {
        events
            .iter()
            .filter_map(|ev| match ev {
                Event::Key(key) => Some(key.code),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_late_colour_report_is_not_typing() {
        assert!(
            feed(background_report()).is_empty(),
            "the reply to the colour query must not reach the editor"
        );
    }

    #[test]
    fn a_report_closed_with_the_string_terminator_is_swallowed_too() {
        let mut events = vec![key(KeyCode::Char(']'), KeyModifiers::ALT)];
        events.extend(typed("10;rgb:f8f8/f8f8/f2f2"));
        events.push(key(KeyCode::Char('\\'), KeyModifiers::ALT));
        assert!(
            feed(events).is_empty(),
            "the foreground reply goes the same way"
        );
    }

    #[test]
    fn keys_around_a_report_still_arrive_in_order() {
        let mut events = typed("i");
        events.extend(background_report());
        events.extend(typed("x"));
        assert_eq!(
            codes(&feed(events)),
            vec![KeyCode::Char('i'), KeyCode::Char('x')]
        );
    }

    #[test]
    fn an_alt_bracket_that_is_not_a_report_is_given_back() {
        // `Alt-]` is a chord somebody can bind: it goes back untouched, along
        // with the key that showed it was not a reply.
        let mut events = vec![key(KeyCode::Char(']'), KeyModifiers::ALT)];
        events.push(key(KeyCode::Char(' '), KeyModifiers::empty()));
        assert_eq!(
            codes(&feed(events)),
            vec![KeyCode::Char(']'), KeyCode::Char(' ')]
        );
    }

    #[test]
    fn anything_that_is_not_a_key_ends_the_run() {
        let events = vec![
            key(KeyCode::Char(']'), KeyModifiers::ALT),
            CrosstermEvent::Resize(80, 24),
        ];
        let out = feed(events);
        assert!(
            matches!(out.first(), Some(Event::Key(_))),
            "the held chord comes back before the resize: {out:?}"
        );
        assert!(matches!(out.get(1), Some(Event::Resize { .. })));
    }
}
