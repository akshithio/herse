//! Which reader a file needs, from its name (and, for `.db`, its first bytes).

use std::path::Path;

/// File extensions Herse opens as data (lowercase, without the dot). `.gz`
/// counts when it follows `csv` or `tsv`.
pub const EXTENSIONS: &[&str] = &[
    "csv", "tsv", "tab", "parquet", "pq", "arrow", "feather", "ipc", "json", "jsonl", "ndjson", "xlsx", "xlsm",
    "xls", "xlsb", "ods", "sqlite", "sqlite3", "db",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// Comma (or sniffed) separated; `gz` when compressed.
    Csv { gz: bool },
    Tsv { gz: bool },
    Parquet,
    /// Arrow IPC (Feather v2).
    Arrow,
    /// A JSON document: an array of records, or an object of columns.
    Json,
    /// One JSON value per line.
    JsonLines,
    /// A workbook (calamine reads xlsx, xlsm, xls, xlsb, ods).
    Excel,
    Sqlite,
}

impl Format {
    /// The format of `path`, from its extension. `.db` is SQLite only when
    /// the file says so.
    pub fn of(path: &Path) -> Option<Format> {
        let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        let (stem, gz) = match name.strip_suffix(".gz") {
            Some(stem) => (stem.to_string(), true),
            None => (name, false),
        };
        let ext = stem.rsplit_once('.')?.1;
        Some(match (ext, gz) {
            ("csv", gz) => Format::Csv { gz },
            ("tsv" | "tab", gz) => Format::Tsv { gz },
            (_, true) => return None,
            ("parquet" | "pq", _) => Format::Parquet,
            ("arrow" | "feather" | "ipc", _) => Format::Arrow,
            ("json", _) => Format::Json,
            ("jsonl" | "ndjson", _) => Format::JsonLines,
            ("xlsx" | "xlsm" | "xls" | "xlsb" | "ods", _) => Format::Excel,
            ("sqlite" | "sqlite3", _) => Format::Sqlite,
            ("db", _) if is_sqlite_file(path) => Format::Sqlite,
            _ => return None,
        })
    }

    /// Does Herse open `path` as data? (Cheap: the name only, except `.db`.)
    pub fn is_data_path(path: &Path) -> bool {
        Format::of(path).is_some()
    }

    /// Workbooks and databases hold several tables.
    pub fn has_tables(self) -> bool {
        matches!(self, Format::Excel | Format::Sqlite)
    }

    /// A short name for the format ("CSV", "Parquet").
    pub fn label(self) -> &'static str {
        match self {
            Format::Csv { .. } => "CSV",
            Format::Tsv { .. } => "TSV",
            Format::Parquet => "Parquet",
            Format::Arrow => "Arrow",
            Format::Json => "JSON",
            Format::JsonLines => "JSON Lines",
            Format::Excel => "Excel",
            Format::Sqlite => "SQLite",
        }
    }
}

fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read as _;
    let mut head = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
}

/// What a reader learned about a file that a loading cell needs to repeat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sniffed {
    /// The CSV separator, when it isn't the format's usual one.
    pub separator: Option<u8>,
    /// Numbers are written with a decimal comma (`3,14`), as with `;`
    /// separators in much of Europe.
    pub decimal_comma: bool,
    /// The sheet or table shown.
    pub table: Option<String>,
    /// How a JSON document holds its table.
    pub json: Option<JsonShape>,
}

/// The ways a JSON file holds a table that Herse reads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum JsonShape {
    /// `[{"a": 1}, …]` (pandas `orient="records"`).
    Records,
    /// `{"a": [1, 2], …}`.
    ColumnLists,
    /// `{"a": {"0": 1, …}, …}` (pandas' default, `orient="columns"`).
    ColumnsByRow,
    /// `{"columns": […], "data": [[…]]}` (pandas `orient="split"`).
    Split,
    /// The records under one key: `{"results": [{…}]}`.
    Wrapped(String),
    /// Anything else: one row.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_from_names() {
        let f = |p: &str| Format::of(Path::new(p));
        assert_eq!(f("a/b/Sales.CSV"), Some(Format::Csv { gz: false }));
        assert_eq!(f("x.tsv.gz"), Some(Format::Tsv { gz: true }));
        assert_eq!(f("x.json.gz"), None);
        assert_eq!(f("x.parquet"), Some(Format::Parquet));
        assert_eq!(f("x.feather"), Some(Format::Arrow));
        assert_eq!(f("x.ndjson"), Some(Format::JsonLines));
        assert_eq!(f("x.xlsx"), Some(Format::Excel));
        assert_eq!(f("x.ipynb"), None);
        assert_eq!(f("csv"), None);
        // `.db` must really be SQLite.
        assert_eq!(f("/nonexistent/x.db"), None);
    }
}
