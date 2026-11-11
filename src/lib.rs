//! vortex — a modal, async, grammar-aware terminal code editor.
//!
//! See `plan/init.md` for the product spec. The crate is organised as:
//!
//! * [`crate::app`] — event loop and top level state
//! * [`crate::buffer`] — rope backed text buffers with undo history
//! * [`crate::layout`] — split tree (explicit / auto / golden)
//! * [`crate::input`] — modal key handling (normal, insert, visual, command)
//! * [`crate::command`] — `:`-command mode
//! * [`crate::ui`] — rendering: panes, borders, status area, prompts
//! * [`crate::syntax`] — tree-sitter parsing + highlighting (async)
//! * [`crate::search`] — ripgrep backed file/content search (async)
//! * [`crate::format`] — async language formatters
//! * [`crate::lsp`] — scaffolding for a language server client

pub mod action;
pub mod app;
pub mod buffer;
pub mod command;
pub mod completion;
pub mod config;
pub mod diagnostic;
pub mod editor;
pub mod error;
pub mod event;
pub mod format;
pub mod fuzzy;
pub mod indent;
pub mod input;
pub mod lang;
pub mod layout;
pub mod lsp;
pub mod prompt;
pub mod search;
pub mod selection;
pub mod spell;
pub mod status;
pub mod syntax;
pub mod term;
pub mod theme;
pub mod ui;

pub use error::{Error, Result};
