//! Error and result types.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while running vortex.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("terminal error: {0}")]
    Terminal(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("invalid buffer index")]
    InvalidBuffer,

    #[error("invalid pane")]
    InvalidPane,

    #[error("{0}")]
    Message(String),
}

impl Error {
    /// A user facing message: shown in the status area instead of crashing.
    pub fn message(msg: impl Into<String>) -> Self {
        Error::Message(msg.into())
    }
}

/// Errors that are *expected* during editing (bad command, no formatter, …).
///
/// These never terminate the editor; they flash a message in the status area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserError(pub String);

impl UserError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UserError {}

impl From<Error> for UserError {
    fn from(e: Error) -> Self {
        Self(e.to_string())
    }
}

pub type UserResult<T> = std::result::Result<T, UserError>;
