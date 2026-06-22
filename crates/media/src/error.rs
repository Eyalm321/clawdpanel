//! A small string-backed error, matching Go's habit of plain `error` values
//! whose `.Error()` text flows straight into [`Event::err`](crate::Event).

use std::fmt;

/// Crate-wide error. Carries a message (surfaced verbatim in error events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

/// Sentinel for backends on an unsupported platform (Go `audio.ErrUnsupported`).
pub const UNSUPPORTED: &str = "audio: unsupported platform";

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }
    pub fn unsupported() -> Self {
        Error(UNSUPPORTED.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}
impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
