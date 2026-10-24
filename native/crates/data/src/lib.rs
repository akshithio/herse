//! `herse-data`: tables to look at. Data files (CSV, TSV, Parquet, Arrow,
//! JSON, JSON Lines, Excel, SQLite) and notebook DataFrame outputs become one
//! [`Frame`] that the app draws, sorts, filters and searches.
//!
//! A frame is a list of immutable [`Chunk`]s behind `Arc`s, appended as a
//! reader streams through a file: the app shows the first rows of a 100 MB
//! CSV while the rest is still being read, and a snapshot of a frame
//! (`Frame::clone`) is a few pointers, cheap to hand to a background sort.
//!
//! Each chunk stores a column as one text arena (what the cell shows) plus,
//! while every value in it is a number, the parsed numbers: drawing reads
//! `&str`s without formatting anything, and sorting and statistics read
//! `f64`s without parsing anything.
//!
//! ```
//! let mut frame = herse_data::Frame::default();
//! frame.set_columns(vec![herse_data::Column::new("city"), herse_data::Column::new("people")]);
//! let mut b = herse_data::ChunkBuilder::new(2);
//! b.push_row([herse_data::Value::Text("Oslo"), herse_data::Value::Text("709000")]);
//! b.push_row([herse_data::Value::Text("Bergen"), herse_data::Value::Null("")]);
//! frame.push_chunk(b.finish());
//! assert_eq!(frame.text(1, 0), "Bergen");
//! assert_eq!(frame.number(0, 1), Some(709000.0));
//! assert!(frame.is_null(1, 1));
//! assert_eq!(frame.kind(1), herse_data::Kind::Int);
//! ```
//!
//! Everything here is synchronous and `Send`: run readers and view
//! computations on a background thread.
#![forbid(unsafe_code)]

use std::sync::Arc;

mod export;
mod formats;
mod notebook;
mod profile;
mod read;
mod table;
mod view;

pub use export::{to_csv, to_markdown, to_tsv};
pub use formats::{EXTENSIONS, Format, JsonShape, Sniffed};
pub use notebook::{Library, loading_code};
pub use profile::{Categories, Histogram, Profile, Shape, TOP, TimeKind, format_time, parse_time, profile, profile_like};
pub use read::{Event, Flow, Loaded, ReadError, list_tables, read};
pub use table::{FromTable, from_table};
pub use view::{Filter, FilterOp, Order, ViewSpec, compute_order, search_hits};

/// A column's name and what its source said about it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Column {
    pub name: String,
    /// The type the source declared (Parquet, Arrow, SQLite, a polars or R
    /// dtype row), shown under the name. `None`: inferred from the values.
    pub dtype: Option<String>,
    /// A row label (a pandas index): drawn bold, and kept at the left.
    pub index: bool,
}

impl Column {
    pub fn new(name: impl Into<String>) -> Self {
        Column { name: name.into(), dtype: None, index: false }
    }
}

/// One value as a reader hands it over.
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    /// Missing; the text is what the source wrote (`""`, `NA`, `null`).
    Null(&'a str),
    /// Untyped text (CSV, an HTML table): a number if it parses as one.
    Text(&'a str),
    /// Text that is a string whatever it looks like (a JSON string, a
    /// Parquet `utf8` column).
    Str(&'a str),
    /// A number and how to show it.
    Number(f64, &'a str),
    /// A boolean and how to show it (`True`, `true`).
    Bool(bool, &'a str),
}

/// What a column holds, from its values (or its declared type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Kind {
    /// No values yet, or only missing ones.
    #[default]
    Empty,
    Int,
    Float,
    Bool,
    Text,
}

impl Kind {
    pub fn is_numeric(self) -> bool {
        matches!(self, Kind::Int | Kind::Float)
    }
    /// The short name shown under a column without a declared type.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Empty => "null",
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::Bool => "bool",
            Kind::Text => "str",
        }
    }
}

/// Counts and extremes of a column (or of one chunk of it).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Stats {
    /// Rows.
    pub count: usize,
    pub nulls: usize,
    /// Values that are numbers.
    pub numbers: usize,
    pub bools: usize,
    /// Of the numbers: are they all whole?
    pub fractional: bool,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    /// Widest value, in characters (capped at [`WIDTH_CAP`]).
    pub width: u16,
}

/// Values wider than this many characters don't widen their column further.
pub const WIDTH_CAP: u16 = 60;

impl Stats {
    fn merge(&mut self, o: &Stats) {
        if o.numbers > 0 {
            if self.numbers == 0 {
                self.min = o.min;
                self.max = o.max;
            } else {
                self.min = self.min.min(o.min);
                self.max = self.max.max(o.max);
            }
        }
        self.count += o.count;
        self.nulls += o.nulls;
        self.numbers += o.numbers;
        self.bools += o.bools;
        self.fractional |= o.fractional;
        self.sum += o.sum;
        self.width = self.width.max(o.width);
    }

    /// Non-missing values.
    pub fn values(&self) -> usize {
        self.count - self.nulls
    }

    pub fn kind(&self) -> Kind {
        let values = self.values();
        if values == 0 {
            Kind::Empty
        } else if self.numbers == values {
            if self.fractional { Kind::Float } else { Kind::Int }
        } else if self.bools == values {
            Kind::Bool
        } else {
            Kind::Text
        }
    }

    /// The mean of the numbers, when there are any.
    pub fn mean(&self) -> Option<f64> {
        (self.numbers > 0).then(|| self.sum / self.numbers as f64)
    }
}

/// One column of one chunk.
#[derive(Debug, Default)]
struct Cells {
    /// Every value's text, end to end.
    text: String,
    /// Where each value's text ends in `text`.
    ends: Vec<u32>,
    /// One bit per row, set when missing; `None` when nothing is.
    nulls: Option<Vec<u64>>,
    /// The values as numbers (NaN where missing), while every value is one.
    nums: Option<Vec<f64>>,
    stats: Stats,
}

impl Cells {
    fn get(&self, row: usize) -> &str {
        let start = if row == 0 { 0 } else { self.ends[row - 1] as usize };
        &self.text[start..self.ends[row] as usize]
    }
    fn is_null(&self, row: usize) -> bool {
        self.nulls.as_ref().is_some_and(|bits| bits[row / 64] >> (row % 64) & 1 == 1)
    }
    fn number(&self, row: usize) -> Option<f64> {
        let v = *self.nums.as_ref()?.get(row)?;
        (!v.is_nan() || !self.is_null(row)).then_some(v)
    }
}

/// Rows `start..start + rows` of a frame: immutable once built.
#[derive(Debug, Default)]
pub struct Chunk {
    rows: usize,
    columns: Vec<Cells>,
}

impl Chunk {
    pub fn rows(&self) -> usize {
        self.rows
    }
    /// Bytes of text held (for progress and memory notes).
    pub fn text_bytes(&self) -> usize {
        self.columns.iter().map(|c| c.text.len()).sum()
    }
}

/// Builds a [`Chunk`] row by row (or column by column).
pub struct ChunkBuilder {
    rows: usize,
    columns: Vec<Cells>,
}

impl ChunkBuilder {
    pub fn new(columns: usize) -> Self {
        ChunkBuilder {
            rows: 0,
            columns: (0..columns).map(|_| Cells { nums: Some(Vec::new()), ..Default::default() }).collect(),
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Text bytes so far.
    pub fn bytes(&self) -> usize {
        self.columns.iter().map(|c| c.text.len()).sum()
    }

    /// A column the source only now revealed (a key first seen on a later
    /// JSON line): earlier rows of this chunk are missing in it.
    pub fn add_column(&mut self) {
        let mut cells = Cells { nums: Some(Vec::new()), ..Default::default() };
        for _ in 0..self.rows {
            push_value(&mut cells, Value::Null(""));
        }
        self.columns.push(cells);
    }

    /// Append one row; missing trailing values are null, extra ones dropped.
    pub fn push_row<'a>(&mut self, values: impl IntoIterator<Item = Value<'a>>) {
        let mut n = 0;
        for (cells, v) in self.columns.iter_mut().zip(values) {
            push_value(cells, v);
            n += 1;
        }
        for cells in &mut self.columns[n..] {
            push_value(cells, Value::Null(""));
        }
        self.rows += 1;
    }

    /// Append one value to column `col` (fill every column, then call
    /// [`ChunkBuilder::end_row`]).
    pub fn push(&mut self, col: usize, v: Value<'_>) {
        push_value(&mut self.columns[col], v);
    }

    /// Finish a row built with [`ChunkBuilder::push`]: columns left short
    /// get a missing value.
    pub fn end_row(&mut self) {
        self.rows += 1;
        let rows = self.rows;
        for cells in &mut self.columns {
            while cells.ends.len() < rows {
                push_value(cells, Value::Null(""));
            }
        }
    }

    pub fn finish(mut self) -> Arc<Chunk> {
        for c in &mut self.columns {
            if c.stats.numbers == 0 {
                c.nums = None;
            }
            c.text.shrink_to_fit();
        }
        Arc::new(Chunk { rows: self.rows, columns: self.columns })
    }
}

fn push_value(c: &mut Cells, v: Value<'_>) {
    let row = c.ends.len();
    let (text, num, null, boolean) = match v {
        Value::Null(t) => (t, None, true, false),
        Value::Text(t) => {
            if is_null_token(t) {
                (t, None, true, false)
            } else if let Some(x) = parse_number(t) {
                (t, Some(x), false, false)
            } else {
                (t, None, false, parse_bool(t).is_some())
            }
        }
        Value::Str(t) => (t, None, false, false),
        Value::Number(x, t) => (t, Some(x), false, false),
        Value::Bool(_, t) => (t, None, false, true),
    };
    c.text.push_str(text);
    c.ends.push(u32::try_from(c.text.len()).unwrap_or(u32::MAX));
    let s = &mut c.stats;
    s.count += 1;
    let width = text_width(text);
    s.width = s.width.max(width);
    if null {
        s.nulls += 1;
        let bits = c.nulls.get_or_insert_with(Vec::new);
        if bits.len() <= row / 64 {
            bits.resize(row / 64 + 1, 0);
        }
        bits[row / 64] |= 1 << (row % 64);
    }
    if boolean {
        s.bools += 1;
    }
    match (num, c.nums.as_mut()) {
        (Some(x), Some(nums)) => {
            if s.numbers == 0 {
                s.min = x;
                s.max = x;
            } else {
                s.min = s.min.min(x);
                s.max = s.max.max(x);
            }
            s.numbers += 1;
            s.sum += x;
            s.fractional |= x.fract() != 0.0 || text.contains(['.', 'e', 'E']) && x.is_finite();
            nums.push(x);
        }
        (None, Some(nums)) if null => nums.push(f64::NAN),
        // A value that isn't a number: this chunk's column is text from here.
        (None, Some(_)) => {
            c.nums = None;
            s.numbers = 0;
            s.sum = 0.;
        }
        (_, None) => {}
    }
}

fn text_width(text: &str) -> u16 {
    let line = text.split('\n').next().unwrap_or("");
    if line.len() > WIDTH_CAP as usize * 4 {
        return WIDTH_CAP;
    }
    (line.chars().count() as u16).min(WIDTH_CAP)
}

/// pandas' default missing-value markers (`read_csv`'s `na_values`).
pub fn is_null_token(t: &str) -> bool {
    matches!(
        t,
        "" | "NA"
            | "N/A"
            | "n/a"
            | "NaN"
            | "nan"
            | "-NaN"
            | "-nan"
            | "NULL"
            | "null"
            | "None"
            | "<NA>"
            | "NaT"
            | "#N/A"
            | "#N/A N/A"
            | "#NA"
            | "-1.#IND"
            | "-1.#QNAN"
            | "1.#IND"
            | "1.#QNAN"
    )
}

/// A number as pandas would read one: decimal or scientific, with `inf`.
/// Thousands separators make text, as they do in `read_csv`.
pub fn parse_number(t: &str) -> Option<f64> {
    let t = t.trim_matches(' ');
    let b = t.as_bytes();
    let first = *b.first()?;
    // Fast reject: most text doesn't start like a number.
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.' | b'i' | b'I')) {
        return None;
    }
    if matches!(first, b'i' | b'I') || (b.len() > 1 && matches!(b[1], b'i' | b'I')) {
        let rest = t.trim_start_matches(['-', '+']);
        if !(rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity")) {
            return None;
        }
    }
    t.parse::<f64>().ok()
}

pub fn parse_bool(t: &str) -> Option<bool> {
    match t {
        "True" | "true" | "TRUE" => Some(true),
        "False" | "false" | "FALSE" => Some(false),
        _ => None,
    }
}

/// A table being read or already read: columns plus chunks of rows.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    columns: Vec<Column>,
    chunks: Vec<Arc<Chunk>>,
    /// First row of each chunk.
    starts: Vec<usize>,
    rows: usize,
}

impl Frame {
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Set (or grow) the columns. Chunks built before a column existed read
    /// as missing in it.
    pub fn set_columns(&mut self, columns: Vec<Column>) {
        self.columns = columns;
    }

    pub fn push_chunk(&mut self, chunk: Arc<Chunk>) {
        if chunk.rows == 0 {
            return;
        }
        self.starts.push(self.rows);
        self.rows += chunk.rows;
        self.chunks.push(chunk);
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Bytes of text held.
    pub fn text_bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.text_bytes()).sum()
    }

    fn locate(&self, row: usize) -> (&Chunk, usize) {
        let ix = self.starts.partition_point(|&s| s <= row) - 1;
        (&self.chunks[ix], row - self.starts[ix])
    }

    fn cells(&self, row: usize, col: usize) -> Option<(&Cells, usize)> {
        if row >= self.rows {
            return None;
        }
        let (chunk, r) = self.locate(row);
        chunk.columns.get(col).map(|c| (c, r))
    }

    /// What the cell shows. Empty for a column the row's chunk predates.
    pub fn text(&self, row: usize, col: usize) -> &str {
        self.cells(row, col).map_or("", |(c, r)| c.get(r))
    }

    pub fn is_null(&self, row: usize, col: usize) -> bool {
        self.cells(row, col).is_none_or(|(c, r)| c.is_null(r))
    }

    /// The value as a number, when its chunk's column is all numbers.
    pub fn number(&self, row: usize, col: usize) -> Option<f64> {
        self.cells(row, col).and_then(|(c, r)| c.number(r))
    }

    /// Statistics over every row read so far.
    pub fn stats(&self, col: usize) -> Stats {
        let mut s = Stats::default();
        for chunk in &self.chunks {
            match chunk.columns.get(col) {
                Some(c) => s.merge(&c.stats),
                None => s.merge(&Stats { count: chunk.rows, nulls: chunk.rows, ..Default::default() }),
            }
        }
        s
    }

    /// A column's kind. Numbers in one chunk and text in another make text.
    pub fn kind(&self, col: usize) -> Kind {
        self.stats(col).kind()
    }

    /// Iterate one column's chunks: `(first row, cells)`.
    pub(crate) fn column_chunks(&self, col: usize) -> impl Iterator<Item = (usize, Option<&Cells>, usize)> {
        self.chunks.iter().zip(&self.starts).map(move |(chunk, &start)| (start, chunk.columns.get(col), chunk.rows))
    }

    /// The column's widest value in characters, header included.
    pub fn width_chars(&self, col: usize) -> u16 {
        let name = self.columns.get(col).map_or(0, |c| text_width(&c.name));
        let dtype = self.columns.get(col).and_then(|c| c.dtype.as_deref()).map_or(0, text_width);
        self.stats(col).width.max(name).max(dtype)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_nulls_and_text() {
        let mut b = ChunkBuilder::new(3);
        b.push_row([Value::Text("1"), Value::Text("x"), Value::Text("1.5")]);
        b.push_row([Value::Text("NA"), Value::Text("2"), Value::Text("")]);
        b.push_row([Value::Text("3"), Value::Text("y"), Value::Text("-2e3")]);
        let mut f = Frame::default();
        f.set_columns(vec![Column::new("a"), Column::new("b"), Column::new("c")]);
        f.push_chunk(b.finish());
        assert_eq!(f.kind(0), Kind::Int);
        assert_eq!(f.kind(1), Kind::Text);
        assert_eq!(f.kind(2), Kind::Float);
        assert_eq!(f.number(0, 0), Some(1.));
        assert_eq!(f.number(1, 0), None);
        assert!(f.is_null(1, 0) && f.is_null(1, 2) && !f.is_null(1, 1));
        assert_eq!(f.number(1, 1), None, "a text column has no numbers");
        let s = f.stats(2);
        assert_eq!((s.count, s.nulls, s.min, s.max), (3, 1, -2000., 1.5));
        assert_eq!(f.stats(0).mean(), Some(2.));
    }

    #[test]
    fn chunks_and_growing_columns() {
        let mut f = Frame::default();
        f.set_columns(vec![Column::new("a")]);
        let mut b = ChunkBuilder::new(1);
        b.push_row([Value::Text("1")]);
        f.push_chunk(b.finish());
        f.set_columns(vec![Column::new("a"), Column::new("b")]);
        let mut b = ChunkBuilder::new(2);
        b.push_row([Value::Text("2"), Value::Str("z")]);
        b.push_row([Value::Text("x")]);
        f.push_chunk(b.finish());
        assert_eq!(f.rows(), 3);
        assert_eq!(f.text(0, 1), "");
        assert!(f.is_null(0, 1));
        assert_eq!(f.text(1, 1), "z");
        assert!(f.is_null(2, 1));
        // "x" in the second chunk makes the column text.
        assert_eq!(f.kind(0), Kind::Text);
        assert_eq!(f.stats(1).nulls, 2);
    }

    #[test]
    fn number_parsing_matches_pandas() {
        for (t, want) in [("12", Some(12.)), ("-1.5e3", Some(-1500.)), (".5", Some(0.5)), ("inf", Some(f64::INFINITY))] {
            assert_eq!(parse_number(t), want, "{t}");
        }
        for t in ["1,234", "abc", "12a", "info", "", "-", "1.2.3"] {
            assert_eq!(parse_number(t), None, "{t}");
        }
        assert!(is_null_token("NaN") && is_null_token("") && !is_null_token("0"));
    }

    #[test]
    fn bools() {
        let mut b = ChunkBuilder::new(1);
        b.push_row([Value::Text("True")]);
        b.push_row([Value::Text("false")]);
        let mut f = Frame::default();
        f.set_columns(vec![Column::new("ok")]);
        f.push_chunk(b.finish());
        assert_eq!(f.kind(0), Kind::Bool);
    }
}
