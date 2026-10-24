//! Source-formatting detection and write options.

use std::collections::HashMap;
use std::sync::Arc;

/// Indentation of the JSON text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Indent {
    /// Everything on one line (Python `indent=None`, JS `JSON.stringify(x)`).
    Compact,
    /// One line per item, `n` spaces per level (`n` may be 0: Python `indent=0`).
    Spaces(u8),
    /// One line per item, `n` tabs per level.
    Tabs(u8),
}

/// How multiline text was cut into line arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineSplit {
    /// Python `str.splitlines(True)` (nbformat).
    Python,
    /// At `\n` only (JS writers: VS Code, Colab).
    Newline,
}

/// How a notebook file was formatted, detected by [`crate::parse`] and reused
/// by [`crate::to_string`] so untouched notebooks re-save byte-identically.
///
/// [`FormatHint::jupyter`] (also `Default`) is exactly what nbformat writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatHint {
    pub indent: Indent,
    /// Line breaks are `\r\n` (file was touched by a Windows tool).
    pub crlf: bool,
    /// All object keys sorted (nbformat / VS Code). When `false`, keys are
    /// written in the order they were read.
    pub sort_keys: bool,
    /// Non-ASCII written as `\uXXXX` escapes (Python `ensure_ascii=True`).
    pub ensure_ascii: bool,
    /// File ends with a newline (nbformat writes one).
    pub trailing_newline: bool,
    /// File starts with a UTF-8 byte order mark.
    pub bom: bool,
    /// `": "` between key and value (`false`: `":"`).
    pub colon_space: bool,
    /// `", "` between items (Python 2 era `json.dumps(indent=..)` leaves a
    /// trailing space at line ends; compact Python output uses `", "`).
    pub comma_space: bool,
    /// Multiline fields (source, stream text, text mimes) written as line
    /// arrays (nbformat) rather than single strings.
    pub split_lines: bool,
    /// Where lines were cut when `split_lines`.
    pub line_split: LineSplit,
    /// `(nbformat, nbformat_minor)` of the file when it was converted on read
    /// (nbformat 3 -> 4.5). Saving writes nbformat 4; the app may want to say so.
    pub converted_from: Option<(u32, u32)>,
    /// Key order templates for unsorted files (first order seen per object kind).
    pub(crate) key_order: Option<Arc<KeyOrder>>,
    /// Number spellings our formatter would not reproduce (JS-style `1e-7`,
    /// integers beyond 64 bits), keyed by the parsed f64's bits.
    pub(crate) numbers: Option<Arc<NumberSpellings>>,
    /// Multiline fields stored differently from the file's prevailing style
    /// (a plain string among line arrays, chunks not cut at line breaks, a
    /// base64 image split into lines), keyed by content.
    pub(crate) shapes: Option<Arc<Shapes>>,
    /// Strings whose escapes the writer would spell differently (`\/`,
    /// uppercase hex, needless `\u` escapes), keyed by decoded content.
    pub(crate) strings: Option<Arc<RawStrings>>,
}

impl Default for FormatHint {
    fn default() -> Self {
        FormatHint::jupyter()
    }
}

impl FormatHint {
    /// nbformat's own output: `json.dumps(nb, sort_keys=True, indent=1,
    /// separators=(",", ": "), ensure_ascii=False) + "\n"`.
    pub fn jupyter() -> FormatHint {
        FormatHint {
            indent: Indent::Spaces(1),
            crlf: false,
            sort_keys: true,
            ensure_ascii: false,
            trailing_newline: true,
            bom: false,
            colon_space: true,
            comma_space: false,
            split_lines: true,
            line_split: LineSplit::Python,
            converted_from: None,
            key_order: None,
            numbers: None,
            shapes: None,
            strings: None,
        }
    }

    /// Whether this hint is (formatting-wise) exactly nbformat's output.
    pub fn is_jupyter(&self) -> bool {
        self.indent == Indent::Spaces(1)
            && !self.crlf
            && self.sort_keys
            && !self.ensure_ascii
            && self.trailing_newline
            && !self.bom
            && self.colon_space
            && !self.comma_space
            && self.split_lines
            && self.line_split == LineSplit::Python
    }

    /// Number of multiline fields whose on-disk shape is remembered individually.
    pub fn shape_exceptions(&self) -> usize {
        self.shapes.as_ref().map_or(0, |s| s.map.len())
    }

    /// Number of non-canonical number spellings remembered from the source file.
    pub fn number_spellings(&self) -> usize {
        self.numbers.as_ref().map_or(0, |n| n.map.len())
    }
}

/// On-disk shape of one multiline field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// One JSON string.
    String,
    /// A line array cut the file's usual way.
    Lines,
    /// A line array with these chunk byte lengths.
    Chunks(Box<[u32]>),
}

/// Shapes by `(content_hash, byte length)` of the joined text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shapes {
    pub(crate) map: HashMap<(u64, usize), Shape>,
}

/// Raw JSON text (with quotes) of strings by `(content_hash, byte length)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawStrings {
    pub(crate) map: HashMap<(u64, usize), Box<str>>,
}

/// Remembered original spellings of numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NumberSpellings {
    /// f64 bits -> (original text, is an integer literal too large for i64/u64).
    pub(crate) map: HashMap<u64, (Box<str>, bool)>,
}

/// Object kinds whose key order is remembered for unsorted files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ObjKind {
    Notebook = 0,
    CodeCell,
    MarkdownCell,
    RawCell,
    Stream,
    DisplayData,
    ExecuteResult,
    Error,
}

pub(crate) const OBJ_KINDS: usize = 8;

/// Per-kind key order: the first key order seen for that kind of object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyOrder {
    pub(crate) kinds: [Option<Vec<Box<str>>>; OBJ_KINDS],
    /// Cells whose key order differs from their kind's template (files
    /// assembled by several tools), by cell id, or index for cells without one.
    pub(crate) cells: HashMap<CellRef, Vec<Box<str>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CellRef {
    Id(Box<str>),
    Index(usize),
}

impl KeyOrder {
    pub(crate) fn template(&self, kind: ObjKind) -> Option<&[Box<str>]> {
        self.kinds[kind as usize].as_deref()
    }

    pub(crate) fn cell(&self, id: Option<&str>, index: usize) -> Option<&[Box<str>]> {
        if self.cells.is_empty() {
            return None;
        }
        let key = match id {
            Some(id) => CellRef::Id(id.into()),
            None => CellRef::Index(index),
        };
        self.cells.get(&key).map(Vec::as_slice)
    }
}

/// Options for [`crate::to_string_with`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteOptions {
    /// Formatting to use; `None` = the notebook's own [`crate::Notebook::format`].
    pub format: Option<FormatHint>,
    /// Behave like nbformat's read + write cycle: drop what nbformat strips
    /// (`metadata.signature`, `metadata.orig_nbformat`,
    /// `metadata.orig_nbformat_minor`, `metadata.trusted` on cells) and
    /// normalize [`crate::Output::Unknown`] objects the way nbformat's
    /// `rejoin_lines` / `split_lines` would (otherwise they are written verbatim).
    pub strip_transient: bool,
    /// Re-emit numbers in their original spelling when it differs from Python's
    /// `repr` (e.g. `1e-7` written by a JS tool). Integers too large for 64
    /// bits always keep their digits regardless (Python keeps them exactly).
    pub keep_number_spellings: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions::preserve()
    }
}

impl WriteOptions {
    /// Reproduce the source file's formatting (what [`crate::to_string`] does).
    pub fn preserve() -> WriteOptions {
        WriteOptions { format: None, strip_transient: false, keep_number_spellings: true }
    }

    /// Exactly `nbformat.write(nbformat.read(f, 4), out)`: Jupyter-canonical.
    pub fn jupyter() -> WriteOptions {
        WriteOptions {
            format: Some(FormatHint::jupyter()),
            strip_transient: true,
            keep_number_spellings: false,
        }
    }
}
