//! Events: terminal input plus results from background jobs.
//!
//! Terminal input is read on a dedicated blocking thread and pushed into a
//! `tokio` unbounded channel, so the async main loop never blocks on the tty.
//! Long running work (ripgrep, formatters, tree-sitter parses, spell checking)
//! reports back through the same channel as [`Event::Async`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent};
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
                while !stop.load(Ordering::Relaxed) {
                    match event::poll(Duration::from_millis(80)) {
                        Ok(true) => match event::read() {
                            Ok(ev) => {
                                if let Some(ev) = translate(ev) {
                                    tx.send(ev);
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
        // Focus and (half) scrolled/unsupported events carry no meaning here.
        // Focus events carry no meaning here.
        CrosstermEvent::FocusGained | CrosstermEvent::FocusLost => None,
    }
}
