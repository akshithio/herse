//! Error type.

use std::fmt;

/// Everything that can go wrong in a collaboration session.
#[derive(Debug)]
pub enum Error {
    /// A join link that does not parse (the message says why).
    BadLink(String),
    /// The host rejected our credentials: the link's secret is wrong, or the
    /// session it belonged to has ended and a new one runs on the same port.
    AuthFailed,
    /// The peer is not a Herse collaboration host, or speaks another protocol
    /// version.
    Protocol(String),
    /// Could not reach the host on any of the link's addresses.
    Unreachable(String),
    /// Network or socket error.
    Io(std::io::Error),
    /// This session is view-only: local edits and comments are refused.
    ReadOnly,
    /// A local operation that does not apply to the current document (unknown
    /// cell id, out-of-range index or byte offset, offset inside a UTF-8
    /// character, ...). Nothing was changed.
    InvalidOp(String),
    /// Unknown comment thread or message id.
    UnknownComment(String),
    /// The session has ended (or was never fully started).
    Closed,
}

/// `Result<T, herse_collab::Error>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadLink(why) => write!(f, "invalid join link: {why}"),
            Error::AuthFailed => write!(f, "the host rejected this link (wrong secret or the session has ended)"),
            Error::Protocol(why) => write!(f, "protocol error: {why}"),
            Error::Unreachable(why) => write!(f, "could not reach the host: {why}"),
            Error::Io(e) => write!(f, "network error: {e}"),
            Error::ReadOnly => write!(f, "this session is view-only"),
            Error::InvalidOp(why) => write!(f, "invalid edit: {why}"),
            Error::UnknownComment(id) => write!(f, "no comment thread or message {id:?}"),
            Error::Closed => write!(f, "the collaboration session has ended"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidOp(msg.into())
}
