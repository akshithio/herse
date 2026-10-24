use std::fmt;

/// Why a notebook could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not valid JSON (or not UTF-8). `line`/`column` are 1-based.
    Json { line: usize, column: usize, message: String },
    /// Valid JSON, but an nbformat major version this crate cannot read.
    UnsupportedVersion { nbformat: u64, nbformat_minor: u64 },
    /// Valid JSON, but not shaped like a notebook (e.g. top level is not an object).
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Json { line, column, message } => {
                write!(f, "not valid notebook JSON (line {line}, column {column}): {message}")
            }
            Error::UnsupportedVersion { nbformat, nbformat_minor } => write!(
                f,
                "nbformat v{nbformat}.{nbformat_minor} is not supported; convert it with \
                 `jupyter nbconvert --to notebook --nbformat 4`"
            ),
            Error::Invalid(msg) => write!(f, "not a valid notebook: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Build a `Json` error from a byte offset into `input`.
    pub(crate) fn json_at(input: &[u8], offset: usize, message: impl Into<String>) -> Error {
        let offset = offset.min(input.len());
        let before = &input[..offset];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let line_start = before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let column = String::from_utf8_lossy(&before[line_start..]).chars().count() + 1;
        Error::Json { line, column, message: message.into() }
    }
}
