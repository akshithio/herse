//! Readers: a file in, a stream of [`Event`]s out (columns, then chunks of
//! rows as they are read), so a caller can show the first rows of a large
//! file at once and the rest as it comes.

use std::{
    fmt::Write as _,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{Chunk, ChunkBuilder, Column, Format, JsonShape, Sniffed, Value};

/// What a reader reports while it reads.
#[derive(Debug)]
pub enum Event {
    /// The columns: once before the first chunk, and again whenever a later
    /// row reveals another one (a new key in JSON Lines, an extra CSV field).
    Columns(Vec<Column>),
    /// More rows, in order.
    Chunk(Arc<Chunk>),
    /// How far through the file, 0–1, when the reader can tell.
    Progress(f64),
}

/// The caller's answer to an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading (the tab was closed).
    Stop,
}

/// A finished read.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub rows: usize,
    pub sniffed: Sniffed,
    /// Every sheet or table in the file, when it has several.
    pub tables: Vec<String>,
    /// Things the reader worked around, in sentences ("12 lines had more
    /// fields than the header").
    pub warnings: Vec<String>,
    /// The caller said [`Flow::Stop`].
    pub stopped: bool,
}

/// Why a file couldn't be read, in a sentence or two for the reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadError(pub String);

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReadError {}

fn err(what: impl std::fmt::Display) -> ReadError {
    ReadError(what.to_string())
}

type Sink<'a> = &'a mut dyn FnMut(Event) -> Flow;

/// Read `path` (its `table`, for a workbook or database; the first one when
/// `None`), sending what it holds to `sink` as it goes.
pub fn read(path: &Path, table: Option<&str>, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    let format = Format::of(path).ok_or_else(|| err("Herse doesn't know how to read this kind of file."))?;
    match format {
        Format::Csv { gz } => read_csv(path, None, gz, sink),
        Format::Tsv { gz } => read_csv(path, Some(b'\t'), gz, sink),
        Format::Parquet => read_parquet(path, sink),
        Format::Arrow => read_arrow_ipc(path, sink),
        Format::Json => read_json(path, sink),
        Format::JsonLines => read_json_lines(path, sink),
        Format::Excel => read_excel(path, table, sink),
        Format::Sqlite => read_sqlite(path, table, sink),
    }
}

/// The sheets of a workbook or the tables (and views) of a database; empty
/// for formats with one table.
pub fn list_tables(path: &Path) -> Result<Vec<String>, ReadError> {
    match Format::of(path) {
        Some(Format::Excel) => {
            use calamine::Reader as _;
            let wb = calamine::open_workbook_auto(path).map_err(|e| err(format!("Couldn't open the workbook: {e}")))?;
            Ok(wb.sheet_names())
        }
        Some(Format::Sqlite) => sqlite_tables(&open_sqlite(path)?),
        _ => Ok(Vec::new()),
    }
}

/// When to hand a chunk over: the first one small and soon (so the first
/// rows paint at once), then every 64k rows, 16 MB or 150 ms.
struct Pacer {
    first: bool,
    since: Instant,
}

impl Pacer {
    fn new() -> Self {
        Pacer { first: true, since: Instant::now() }
    }

    fn due(&mut self, b: &ChunkBuilder) -> bool {
        let rows = b.rows();
        if self.first {
            return rows >= 512;
        }
        rows >= 65_536 || (rows & 1023 == 0 && (b.bytes() >= 16 << 20 || self.since.elapsed() >= Duration::from_millis(150)))
    }

    fn flushed(&mut self) {
        self.first = false;
        self.since = Instant::now();
    }
}

/// Hand `builder`'s rows over and start a new chunk.
fn flush(builder: &mut ChunkBuilder, columns: usize, sink: Sink<'_>) -> Flow {
    if builder.rows() == 0 {
        return Flow::Continue;
    }
    let full = std::mem::replace(builder, ChunkBuilder::new(columns));
    sink(Event::Chunk(full.finish()))
}

// ---------------------------------------------------------------------- CSV

/// Counts bytes taken from the file, for progress through compressed input.
struct Counting<R> {
    inner: R,
    read: Arc<std::sync::atomic::AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

fn read_csv(path: &Path, separator: Option<u8>, gz: bool, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    let file = File::open(path).map_err(|e| err(format!("Couldn't open the file: {e}")))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0).max(1);
    let taken = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counting = Counting { inner: file, read: taken.clone() };
    let raw: Box<dyn Read> = if gz {
        Box::new(flate2::read::MultiGzDecoder::new(BufReader::with_capacity(1 << 20, counting)))
    } else {
        Box::new(counting)
    };
    let mut input = BufReader::with_capacity(1 << 20, raw);
    let head = input.fill_buf().map_err(|e| err(format!("Couldn't read the file: {e}")))?;
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return Err(err("This file is UTF-16 text. Save it as UTF-8 (in Excel: CSV UTF-8) to open it here."));
    }
    let bom = if head.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    let sep = separator.unwrap_or_else(|| sniff_separator(&head[bom..]));
    input.consume(bom);

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sep)
        .buffer_capacity(1 << 20)
        .from_reader(input);
    let mut record = csv::ByteRecord::new();
    let mut loaded = Loaded {
        sniffed: Sniffed { separator: (sep != b',' && separator.is_none()).then_some(sep), ..Default::default() },
        ..Default::default()
    };
    let mut scratch = String::new();

    // The header row names the columns.
    let mut columns: Vec<Column> = Vec::new();
    if reader.read_byte_record(&mut record).map_err(csv_error)? {
        for (i, field) in record.iter().enumerate() {
            let name = decode(field, &mut scratch);
            columns.push(Column::new(if name.trim().is_empty() { format!("Unnamed: {i}") } else { name.to_string() }));
        }
    }
    if sink(Event::Columns(columns.clone())) == Flow::Stop {
        loaded.stopped = true;
        return Ok(loaded);
    }

    let mut builder = ChunkBuilder::new(columns.len());
    let mut pacer = Pacer::new();
    let mut long_lines = 0usize;
    loop {
        let more = match reader.read_byte_record(&mut record) {
            Ok(more) => more,
            Err(e) => {
                // A broken line (an unclosed quote at the end, invalid
                // bytes): keep what was read and say where it stopped.
                loaded.warnings.push(format!("Stopped at line {}: {}", line_of(&e).unwrap_or(loaded.rows + 2), csv_error(e)));
                break;
            }
        };
        if !more {
            break;
        }
        // A line with only a separator-less empty field is a blank line.
        if record.len() == 1 && record[0].is_empty() && columns.len() > 1 {
            continue;
        }
        if record.len() > columns.len() {
            long_lines += 1;
            while columns.len() < record.len() {
                columns.push(Column::new(format!("Unnamed: {}", columns.len())));
                builder.add_column();
            }
            if sink(Event::Columns(columns.clone())) == Flow::Stop {
                loaded.stopped = true;
                return Ok(loaded);
            }
        }
        for (i, field) in record.iter().enumerate() {
            let text = decode(field, &mut scratch);
            // `;`-separated files write `3,14` for 3.14.
            match (sep == b';').then(|| decimal_comma(text)).flatten() {
                Some(x) => {
                    loaded.sniffed.decimal_comma = true;
                    builder.push(i, Value::Number(x, text));
                }
                None => builder.push(i, Value::Text(text)),
            }
        }
        builder.end_row();
        loaded.rows += 1;
        if pacer.due(&builder) {
            let done = taken.load(std::sync::atomic::Ordering::Relaxed) as f64 / total as f64;
            if flush(&mut builder, columns.len(), sink) == Flow::Stop || sink(Event::Progress(done.min(1.))) == Flow::Stop {
                loaded.stopped = true;
                return Ok(loaded);
            }
            pacer.flushed();
        }
    }
    flush(&mut builder, columns.len(), sink);
    sink(Event::Progress(1.));
    if long_lines > 0 {
        loaded.warnings.push(format!(
            "{} had more fields than the header; the extra fields are in the Unnamed columns.",
            plural(long_lines, "line")
        ));
    }
    Ok(loaded)
}

fn csv_error(e: csv::Error) -> ReadError {
    match e.kind() {
        csv::ErrorKind::Io(io) => err(format!("Couldn't read the file: {io}")),
        _ => err(e),
    }
}

fn line_of(e: &csv::Error) -> Option<usize> {
    e.position().map(|p| p.line() as usize)
}

/// A field's text: UTF-8 as is, anything else as Latin-1 (Windows-1252
/// files from Excel are the common case).
fn decode<'a>(field: &'a [u8], scratch: &'a mut String) -> &'a str {
    match std::str::from_utf8(field) {
        Ok(s) => s,
        Err(_) => {
            scratch.clear();
            scratch.extend(field.iter().map(|&b| b as char));
            scratch.as_str()
        }
    }
}

/// `-12,5` as -12.5: digits, one comma, digits.
fn decimal_comma(text: &str) -> Option<f64> {
    let t = text.trim();
    let (int, frac) = t.split_once(',')?;
    let digits = int.strip_prefix('-').unwrap_or(int);
    let ok = !digits.is_empty() && !frac.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && frac.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| format!("{int}.{frac}").parse().ok()).flatten()
}

/// The separator the first lines use most consistently: `,` `;` tab or `|`.
fn sniff_separator(head: &[u8]) -> u8 {
    let candidates = *b",;\t|";
    let mut counts: Vec<[usize; 4]> = Vec::new();
    let mut current = [0usize; 4];
    let mut quoted = false;
    for &b in head.iter().take(64 << 10) {
        match b {
            b'"' => quoted = !quoted,
            b'\n' if !quoted => {
                counts.push(current);
                current = [0; 4];
                if counts.len() == 20 {
                    break;
                }
            }
            _ if !quoted => {
                if let Some(k) = candidates.iter().position(|&c| c == b) {
                    current[k] += 1;
                }
            }
            _ => {}
        }
    }
    if current.iter().any(|&c| c > 0) && counts.len() < 20 {
        counts.push(current);
    }
    let mut best = (0usize, b',');
    for (k, &c) in candidates.iter().enumerate() {
        let per_line: Vec<usize> = counts.iter().map(|l| l[k]).collect();
        let Some(&first) = per_line.first() else { continue };
        let consistent = per_line.iter().all(|&n| n == first);
        let score = if consistent { first * 1000 } else { per_line.iter().copied().min().unwrap_or(0) };
        if score > best.0 {
            best = (score, c);
        }
    }
    best.1
}

// ------------------------------------------------------------ Parquet, Arrow

fn read_parquet(path: &Path, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = File::open(path).map_err(|e| err(format!("Couldn't open the file: {e}")))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| err(format!("This isn't a Parquet file Herse can read: {e}")))?;
    let total = builder.metadata().file_metadata().num_rows().max(1) as f64;
    let schema = builder.schema().clone();
    let reader = builder
        .with_batch_size(8192)
        .build()
        .map_err(|e| err(format!("Couldn't read the Parquet file: {e}")))?;
    read_batches(&schema, reader.map(|b| b.map_err(|e| e.to_string())), Some(total), sink)
}

fn read_arrow_ipc(path: &Path, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    let file = File::open(path).map_err(|e| err(format!("Couldn't open the file: {e}")))?;
    match arrow_ipc::reader::FileReader::try_new(BufReader::new(file), None) {
        Ok(reader) => {
            let schema = reader.schema();
            read_batches(&schema, reader.map(|b| b.map_err(|e| e.to_string())), None, sink)
        }
        // Not the file format: maybe the streaming one (`.arrows`).
        Err(file_err) => {
            let file = File::open(path).map_err(|e| err(format!("Couldn't open the file: {e}")))?;
            let reader = arrow_ipc::reader::StreamReader::try_new(BufReader::new(file), None)
                .map_err(|_| err(format!("This isn't an Arrow file Herse can read: {file_err}")))?;
            let schema = reader.schema();
            read_batches(&schema, reader.map(|b| b.map_err(|e| e.to_string())), None, sink)
        }
    }
}

fn read_batches(
    schema: &arrow_schema::Schema,
    batches: impl Iterator<Item = Result<arrow_array::RecordBatch, String>>,
    total_rows: Option<f64>,
    sink: Sink<'_>,
) -> Result<Loaded, ReadError> {
    let columns: Vec<Column> = schema
        .fields()
        .iter()
        .map(|f| Column { name: f.name().clone(), dtype: Some(arrow_dtype(f.data_type())), index: is_index_name(f.name()) })
        .collect();
    let n = columns.len();
    let mut loaded = Loaded::default();
    if sink(Event::Columns(columns)) == Flow::Stop {
        loaded.stopped = true;
        return Ok(loaded);
    }
    let mut builder = ChunkBuilder::new(n);
    let mut pacer = Pacer::new();
    let mut text = String::new();
    for batch in batches {
        let batch = match batch {
            Ok(b) => b,
            Err(e) => {
                if loaded.rows == 0 {
                    return Err(err(format!("Couldn't read the file: {e}")));
                }
                loaded.warnings.push(format!("Stopped after {} rows: {e}", loaded.rows));
                break;
            }
        };
        append_batch(&mut builder, &batch, &mut text);
        loaded.rows += batch.num_rows();
        if pacer.first || pacer.due(&builder) {
            if flush(&mut builder, n, sink) == Flow::Stop {
                loaded.stopped = true;
                return Ok(loaded);
            }
            if let Some(total) = total_rows
                && sink(Event::Progress((loaded.rows as f64 / total).min(1.))) == Flow::Stop
            {
                loaded.stopped = true;
                return Ok(loaded);
            }
            pacer.flushed();
        }
    }
    flush(&mut builder, n, sink);
    sink(Event::Progress(1.));
    Ok(loaded)
}

/// pandas writes a non-default index to Parquet as `__index_level_0__`.
fn is_index_name(name: &str) -> bool {
    name.starts_with("__index_level_")
}

/// A short, polars-like name for an Arrow type.
fn arrow_dtype(t: &arrow_schema::DataType) -> String {
    use arrow_schema::DataType as T;
    match t {
        T::Null => "null".into(),
        T::Boolean => "bool".into(),
        T::Int8 => "int8".into(),
        T::Int16 => "int16".into(),
        T::Int32 => "int32".into(),
        T::Int64 => "int64".into(),
        T::UInt8 => "uint8".into(),
        T::UInt16 => "uint16".into(),
        T::UInt32 => "uint32".into(),
        T::UInt64 => "uint64".into(),
        T::Float16 => "float16".into(),
        T::Float32 => "float32".into(),
        T::Float64 => "float64".into(),
        T::Utf8 | T::LargeUtf8 | T::Utf8View => "str".into(),
        T::Binary | T::LargeBinary | T::BinaryView | T::FixedSizeBinary(_) => "binary".into(),
        T::Date32 | T::Date64 => "date".into(),
        T::Time32(_) | T::Time64(_) => "time".into(),
        T::Timestamp(unit, tz) => {
            let unit = match unit {
                arrow_schema::TimeUnit::Second => "s",
                arrow_schema::TimeUnit::Millisecond => "ms",
                arrow_schema::TimeUnit::Microsecond => "us",
                arrow_schema::TimeUnit::Nanosecond => "ns",
            };
            match tz {
                Some(tz) => format!("datetime[{unit}, {tz}]"),
                None => format!("datetime[{unit}]"),
            }
        }
        T::Duration(_) => "duration".into(),
        T::Interval(_) => "interval".into(),
        T::Decimal32(p, s) | T::Decimal64(p, s) => format!("decimal({p}, {s})"),
        T::Decimal128(p, s) | T::Decimal256(p, s) => format!("decimal({p}, {s})"),
        T::List(f) | T::LargeList(f) | T::ListView(f) | T::LargeListView(f) | T::FixedSizeList(f, _) => {
            format!("list[{}]", arrow_dtype(f.data_type()))
        }
        T::Struct(_) => "struct".into(),
        T::Map(..) => "map".into(),
        T::Union(..) => "union".into(),
        T::Dictionary(_, v) => match v.as_ref() {
            T::Utf8 | T::LargeUtf8 | T::Utf8View => "cat".into(),
            other => arrow_dtype(other),
        },
        T::RunEndEncoded(_, v) => arrow_dtype(v.data_type()),
    }
}

fn append_batch(builder: &mut ChunkBuilder, batch: &arrow_array::RecordBatch, text: &mut String) {
    use arrow_array::{Array, cast::AsArray, types::Float64Type};
    use arrow_schema::DataType as T;
    let rows = batch.num_rows();
    let start = builder.rows();
    let options = arrow_cast::display::FormatOptions::default().with_null("");
    for (c, array) in batch.columns().iter().enumerate() {
        let array: &dyn Array = array.as_ref();
        let numeric = array.data_type().is_numeric();
        // Numbers come from a cast to f64 (ints, floats, decimals); the
        // text from Arrow's own formatting.
        let nums = numeric.then(|| arrow_cast::cast(array, &T::Float64).ok()).flatten();
        let nums = nums.as_ref().map(|a| a.as_primitive::<Float64Type>());
        match array.data_type() {
            T::Utf8 => {
                let a = array.as_string::<i32>();
                for i in 0..rows {
                    builder.push(c, if a.is_null(i) { Value::Null("") } else { Value::Str(a.value(i)) });
                }
            }
            T::LargeUtf8 => {
                let a = array.as_string::<i64>();
                for i in 0..rows {
                    builder.push(c, if a.is_null(i) { Value::Null("") } else { Value::Str(a.value(i)) });
                }
            }
            T::Utf8View => {
                let a = array.as_string_view();
                for i in 0..rows {
                    builder.push(c, if a.is_null(i) { Value::Null("") } else { Value::Str(a.value(i)) });
                }
            }
            T::Boolean => {
                let a = array.as_boolean();
                for i in 0..rows {
                    builder.push(
                        c,
                        if a.is_null(i) {
                            Value::Null("")
                        } else if a.value(i) {
                            Value::Bool(true, "true")
                        } else {
                            Value::Bool(false, "false")
                        },
                    );
                }
            }
            _ => match arrow_cast::display::ArrayFormatter::try_new(array, &options) {
                Ok(formatter) => {
                    for i in 0..rows {
                        if array.is_null(i) {
                            builder.push(c, Value::Null(""));
                            continue;
                        }
                        text.clear();
                        let _ = write!(text, "{}", formatter.value(i));
                        match nums {
                            Some(n) => builder.push(c, Value::Number(n.value(i), text)),
                            None => builder.push(c, Value::Str(text)),
                        }
                    }
                }
                Err(_) => {
                    let what = format!("({})", arrow_dtype(array.data_type()));
                    for _ in 0..rows {
                        builder.push(c, Value::Str(&what));
                    }
                }
            },
        }
    }
    // Rows are complete only once every column has its value.
    for _ in 0..rows {
        builder.end_row();
    }
    debug_assert_eq!(builder.rows(), start + rows);
}

// --------------------------------------------------------------------- JSON

fn read_json(path: &Path, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("Couldn't read the file: {e}")))?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&bytes);
    let doc: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        // JSON Lines saved as .json is common: try it that way.
        Err(e) if e.line() > 1 && bytes.iter().filter(|&&b| b == b'\n').count() > 1 => {
            return read_json_lines(path, sink).map_err(|_| err(format!("This isn't valid JSON: {e}")));
        }
        Err(e) => return Err(err(format!("This isn't valid JSON: {e}"))),
    };
    let (shape, json) = records_of(doc);
    let mut loaded = Loaded { sniffed: Sniffed { json: Some(json), ..Default::default() }, ..Default::default() };
    let mut records = Records::new();
    match shape {
        Shape::Records(items) => {
            for item in items {
                if records.push(item, sink) == Flow::Stop {
                    loaded.stopped = true;
                    return Ok(loaded);
                }
            }
        }
        Shape::Columns(cols) => {
            let names: Vec<Column> = cols.iter().map(|(name, _)| Column::new(name.clone())).collect();
            let n = names.len();
            sink(Event::Columns(names));
            let rows = cols.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
            let mut builder = ChunkBuilder::new(n);
            let mut text = String::new();
            for r in 0..rows {
                for (c, (_, values)) in cols.iter().enumerate() {
                    match values.get(r) {
                        Some(v) => push_json(&mut builder, c, v, &mut text),
                        None => builder.push(c, Value::Null("")),
                    }
                }
                builder.end_row();
                if builder.rows() == 65_536 && flush(&mut builder, n, sink) == Flow::Stop {
                    loaded.stopped = true;
                    return Ok(loaded);
                }
            }
            flush(&mut builder, n, sink);
            loaded.rows = rows;
            sink(Event::Progress(1.));
            return Ok(loaded);
        }
    }
    loaded.rows = records.finish(sink);
    Ok(loaded)
}

enum Shape {
    /// One row per value (objects become rows by key).
    Records(Vec<serde_json::Value>),
    /// Column name → values (`{"a": [1, 2]}`, pandas' `orient="columns"`).
    Columns(Vec<(String, Vec<serde_json::Value>)>),
}

/// The common ways a JSON file holds a table.
fn records_of(doc: serde_json::Value) -> (Shape, JsonShape) {
    use serde_json::Value as J;
    match doc {
        J::Array(items) => (Shape::Records(items), JsonShape::Records),
        J::Object(mut map) => {
            // pandas orient="split": {"columns": [...], "data": [[...]]}.
            if let (Some(J::Array(cols)), Some(J::Array(data))) = (map.get("columns"), map.get("data")) {
                let names: Vec<String> = cols.iter().map(json_key).collect();
                let rows = data
                    .iter()
                    .map(|row| match row {
                        J::Array(vals) => J::Object(names.iter().cloned().zip(vals.iter().cloned()).collect()),
                        other => other.clone(),
                    })
                    .collect();
                return (Shape::Records(rows), JsonShape::Split);
            }
            // Every value a list (column-oriented) or an object keyed by row
            // (pandas orient="columns").
            if !map.is_empty() && map.values().all(|v| v.is_array()) && map.len() > 1 {
                return (
                    Shape::Columns(map.into_iter().map(|(k, v)| (k, if let J::Array(a) = v { a } else { Vec::new() })).collect()),
                    JsonShape::ColumnLists,
                );
            }
            if !map.is_empty() && map.values().all(|v| v.is_object()) {
                let mut index: Vec<String> = Vec::new();
                for v in map.values() {
                    if let J::Object(rows) = v {
                        for k in rows.keys() {
                            if !index.contains(k) {
                                index.push(k.clone());
                            }
                        }
                    }
                }
                // Only when the inner keys look like row labels.
                if index.len() > 1 || map.len() > 1 {
                    let mut cols = vec![(String::new(), index.iter().map(|k| J::String(k.clone())).collect())];
                    for (name, v) in map {
                        let J::Object(rows) = v else { continue };
                        cols.push((name, index.iter().map(|k| rows.get(k).cloned().unwrap_or(J::Null)).collect()));
                    }
                    return (Shape::Columns(cols), JsonShape::ColumnsByRow);
                }
            }
            // A wrapper around the records: {"data": [...]}, {"results": [...]}.
            let key = map
                .iter()
                .filter_map(|(k, v)| v.as_array().filter(|a| a.first().is_some_and(J::is_object)).map(|a| (k.clone(), a.len())))
                .max_by_key(|(_, n)| *n)
                .map(|(k, _)| k);
            if let Some(key) = key
                && let Some(J::Array(items)) = map.remove(&key)
            {
                return (Shape::Records(items), JsonShape::Wrapped(key));
            }
            (Shape::Records(vec![J::Object(map)]), JsonShape::Other)
        }
        other => (Shape::Records(vec![other]), JsonShape::Other),
    }
}

fn json_key(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn read_json_lines(path: &Path, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    let file = File::open(path).map_err(|e| err(format!("Couldn't open the file: {e}")))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0).max(1) as f64;
    let mut input = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    let mut loaded = Loaded::default();
    let mut records = Records::new();
    let mut bad = 0usize;
    let mut first_bad: Option<(usize, String)> = None;
    let mut done = 0u64;
    let mut line_no = 0usize;
    loop {
        line.clear();
        let n = input.read_until(b'\n', &mut line).map_err(|e| err(format!("Couldn't read the file: {e}")))?;
        if n == 0 {
            break;
        }
        done += n as u64;
        line_no += 1;
        let text = line.trim_ascii();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_slice::<serde_json::Value>(text) {
            Ok(v) => {
                let flushed = records.builder.rows() == 0;
                if records.push(v, sink) == Flow::Stop {
                    loaded.stopped = true;
                    return Ok(loaded);
                }
                if !flushed && records.builder.rows() == 0 && sink(Event::Progress(done as f64 / total)) == Flow::Stop {
                    loaded.stopped = true;
                    return Ok(loaded);
                }
            }
            Err(e) => {
                bad += 1;
                first_bad.get_or_insert((line_no, e.to_string()));
            }
        }
    }
    loaded.rows = records.finish(sink);
    if let Some((line, why)) = first_bad {
        if loaded.rows == 0 {
            return Err(err(format!("Line {line} isn't valid JSON: {why}")));
        }
        loaded.warnings.push(if bad == 1 {
            format!("Line {line} skipped: not valid JSON ({why}).")
        } else {
            format!("{bad} lines skipped: not valid JSON (the first, line {line}: {why}).")
        });
    }
    Ok(loaded)
}

/// JSON values becoming rows: objects by key (columns in order of first
/// appearance), anything else in one `value` column.
struct Records {
    columns: Vec<Column>,
    index: std::collections::HashMap<String, usize>,
    builder: ChunkBuilder,
    pacer: Pacer,
    rows: usize,
    text: String,
    announced: usize,
}

impl Records {
    fn new() -> Self {
        Records {
            columns: Vec::new(),
            index: Default::default(),
            builder: ChunkBuilder::new(0),
            pacer: Pacer::new(),
            rows: 0,
            text: String::new(),
            announced: usize::MAX,
        }
    }

    fn column(&mut self, name: &str) -> usize {
        if let Some(&c) = self.index.get(name) {
            return c;
        }
        let c = self.columns.len();
        self.columns.push(Column::new(name));
        self.index.insert(name.to_string(), c);
        self.builder.add_column();
        c
    }

    fn push(&mut self, item: serde_json::Value, sink: Sink<'_>) -> Flow {
        match item {
            serde_json::Value::Object(map) => {
                for (k, v) in &map {
                    let c = self.column(k);
                    push_json(&mut self.builder, c, v, &mut self.text);
                }
            }
            other => {
                let c = self.column("value");
                push_json(&mut self.builder, c, &other, &mut self.text);
            }
        }
        self.builder.end_row();
        self.rows += 1;
        if self.pacer.due(&self.builder) {
            if self.announce(sink) == Flow::Stop || flush(&mut self.builder, self.columns.len(), sink) == Flow::Stop {
                return Flow::Stop;
            }
            self.pacer.flushed();
        }
        Flow::Continue
    }

    fn announce(&mut self, sink: Sink<'_>) -> Flow {
        if self.announced == self.columns.len() {
            return Flow::Continue;
        }
        self.announced = self.columns.len();
        sink(Event::Columns(self.columns.clone()))
    }

    fn finish(mut self, sink: Sink<'_>) -> usize {
        self.announce(sink);
        flush(&mut self.builder, self.columns.len(), sink);
        sink(Event::Progress(1.));
        self.rows
    }
}

fn push_json(builder: &mut ChunkBuilder, c: usize, v: &serde_json::Value, text: &mut String) {
    use serde_json::Value as J;
    match v {
        J::Null => builder.push(c, Value::Null("")),
        J::Bool(true) => builder.push(c, Value::Bool(true, "true")),
        J::Bool(false) => builder.push(c, Value::Bool(false, "false")),
        J::Number(n) => {
            text.clear();
            let _ = write!(text, "{n}");
            builder.push(c, Value::Number(n.as_f64().unwrap_or(f64::NAN), text));
        }
        J::String(s) => builder.push(c, Value::Str(s)),
        other => {
            text.clear();
            let _ = write!(text, "{other}");
            builder.push(c, Value::Str(text));
        }
    }
}

// -------------------------------------------------------------------- Excel

fn read_excel(path: &Path, table: Option<&str>, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    use calamine::{Data, Reader as _};
    let mut wb = calamine::open_workbook_auto(path).map_err(|e| err(format!("Couldn't open the workbook: {e}")))?;
    let sheets = wb.sheet_names();
    let sheet = match table {
        Some(t) if sheets.iter().any(|s| s == t) => t.to_string(),
        Some(t) => return Err(err(format!("The workbook has no sheet named “{t}”."))),
        None => sheets.first().cloned().ok_or_else(|| err("The workbook has no sheets."))?,
    };
    let range = wb.worksheet_range(&sheet).map_err(|e| err(format!("Couldn't read the sheet “{sheet}”: {e}")))?;
    let mut loaded = Loaded {
        tables: sheets.clone(),
        sniffed: Sniffed { table: (sheets.len() > 1 || sheets.first() != Some(&sheet)).then(|| sheet.clone()), ..Default::default() },
        ..Default::default()
    };
    let mut rows = range.rows();
    let mut text = String::new();
    let columns: Vec<Column> = match rows.next() {
        Some(header) => header
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let name = d.to_string();
                Column::new(if name.trim().is_empty() { format!("Unnamed: {i}") } else { name })
            })
            .collect(),
        None => Vec::new(),
    };
    let n = columns.len();
    if sink(Event::Columns(columns)) == Flow::Stop {
        loaded.stopped = true;
        return Ok(loaded);
    }
    let total = range.height().max(1) as f64;
    let mut builder = ChunkBuilder::new(n);
    let mut pacer = Pacer::new();
    for row in rows {
        for (c, d) in row.iter().enumerate().take(n) {
            text.clear();
            let v = match d {
                Data::Empty => Value::Null(""),
                Data::Int(i) => {
                    let _ = write!(text, "{i}");
                    Value::Number(*i as f64, &text)
                }
                Data::Float(f) => {
                    format_float(*f, &mut text);
                    Value::Number(*f, &text)
                }
                Data::Bool(true) => Value::Bool(true, "TRUE"),
                Data::Bool(false) => Value::Bool(false, "FALSE"),
                Data::String(s) => Value::Str(s),
                Data::DateTime(dt) => {
                    let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
                    if dt.is_duration() {
                        let _ = write!(text, "{h:02}:{mi:02}:{s:02}");
                    } else if (h, mi, s, ms) == (0, 0, 0, 0) {
                        let _ = write!(text, "{y:04}-{mo:02}-{d:02}");
                    } else {
                        let _ = write!(text, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
                    }
                    Value::Str(&text)
                }
                Data::DateTimeIso(s) | Data::DurationIso(s) => Value::Str(s),
                Data::Error(e) => {
                    let _ = write!(text, "{e}");
                    Value::Str(&text)
                }
            };
            builder.push(c, v);
        }
        builder.end_row();
        loaded.rows += 1;
        if pacer.due(&builder) {
            if flush(&mut builder, n, sink) == Flow::Stop || sink(Event::Progress(loaded.rows as f64 / total)) == Flow::Stop {
                loaded.stopped = true;
                return Ok(loaded);
            }
            pacer.flushed();
        }
    }
    flush(&mut builder, n, sink);
    sink(Event::Progress(1.));
    Ok(loaded)
}

/// A float as a spreadsheet shows it: whole numbers without `.0`.
fn format_float(f: f64, out: &mut String) {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        let _ = write!(out, "{}", f as i64);
    } else {
        let _ = write!(out, "{f:?}");
    }
}

// ------------------------------------------------------------------- SQLite

fn open_sqlite(path: &Path) -> Result<rusqlite::Connection, ReadError> {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| err(format!("Couldn't open the database: {e}")))
}

fn sqlite_tables(conn: &rusqlite::Connection) -> Result<Vec<String>, ReadError> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY type = 'view', name")
        .map_err(|e| err(format!("This isn't a SQLite database Herse can read: {e}")))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| err(format!("Couldn't list the tables: {e}")))?
        .filter_map(Result::ok)
        .collect();
    Ok(names)
}

fn read_sqlite(path: &Path, table: Option<&str>, sink: Sink<'_>) -> Result<Loaded, ReadError> {
    use rusqlite::types::ValueRef;
    let conn = open_sqlite(path)?;
    let tables = sqlite_tables(&conn)?;
    let name = match table {
        Some(t) if tables.iter().any(|n| n == t) => t.to_string(),
        Some(t) => return Err(err(format!("The database has no table named “{t}”."))),
        None => tables.first().cloned().ok_or_else(|| err("The database has no tables."))?,
    };
    let mut loaded = Loaded {
        tables: tables.clone(),
        sniffed: Sniffed { table: Some(name.clone()), ..Default::default() },
        ..Default::default()
    };
    let sql = format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""));
    let mut stmt = conn.prepare(&sql).map_err(|e| err(format!("Couldn't read the table “{name}”: {e}")))?;
    let columns: Vec<Column> = stmt
        .columns()
        .iter()
        .map(|c| Column { name: c.name().to_string(), dtype: c.decl_type().map(|t| t.to_ascii_lowercase()), index: false })
        .collect();
    // Declared text stays text even when it looks like a number.
    let texty: Vec<bool> = columns
        .iter()
        .map(|c| c.dtype.as_deref().is_some_and(|t| t.contains("char") || t.contains("text") || t.contains("clob")))
        .collect();
    let n = columns.len();
    if sink(Event::Columns(columns)) == Flow::Stop {
        loaded.stopped = true;
        return Ok(loaded);
    }
    let mut rows = stmt.query([]).map_err(|e| err(format!("Couldn't read the table “{name}”: {e}")))?;
    let mut builder = ChunkBuilder::new(n);
    let mut pacer = Pacer::new();
    let mut text = String::new();
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(e) => {
                loaded.warnings.push(format!("Stopped after {} rows: {e}", loaded.rows));
                break;
            }
        };
        for (c, &declared_text) in texty.iter().enumerate() {
            text.clear();
            let v = match row.get_ref(c) {
                Ok(ValueRef::Null) | Err(_) => Value::Null(""),
                Ok(ValueRef::Integer(i)) => {
                    let _ = write!(text, "{i}");
                    Value::Number(i as f64, &text)
                }
                Ok(ValueRef::Real(f)) => {
                    let _ = write!(text, "{f:?}");
                    Value::Number(f, &text)
                }
                Ok(ValueRef::Text(t)) => {
                    let s = match std::str::from_utf8(t) {
                        Ok(s) => s,
                        Err(_) => {
                            text.extend(t.iter().map(|&b| b as char));
                            &text
                        }
                    };
                    if declared_text { Value::Str(s) } else { Value::Text(s) }
                }
                Ok(ValueRef::Blob(b)) => {
                    let _ = write!(text, "<{} bytes>", b.len());
                    Value::Str(&text)
                }
            };
            builder.push(c, v);
        }
        builder.end_row();
        loaded.rows += 1;
        if pacer.due(&builder) {
            if flush(&mut builder, n, sink) == Flow::Stop {
                loaded.stopped = true;
                return Ok(loaded);
            }
            pacer.flushed();
        }
    }
    flush(&mut builder, n, sink);
    sink(Event::Progress(1.));
    Ok(loaded)
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_separators() {
        assert_eq!(sniff_separator(b"a,b,c\n1,2,3\n"), b',');
        assert_eq!(sniff_separator(b"a;b;c\n1,5;2,5;3\n"), b';');
        assert_eq!(sniff_separator(b"a\tb\n1\t2\n"), b'\t');
        assert_eq!(sniff_separator(b"\"x,y\";z\n\"1,2\";3\n"), b';');
        assert_eq!(sniff_separator(b"single\n1\n"), b',');
    }

    #[test]
    fn decimal_commas() {
        assert_eq!(decimal_comma("16,06"), Some(16.06));
        assert_eq!(decimal_comma("-0,5"), Some(-0.5));
        for t in ["1,234,567", "a,b", ",5", "5,", "1.5", "12"] {
            assert_eq!(decimal_comma(t), None, "{t}");
        }
    }

    #[test]
    fn floats_like_a_spreadsheet() {
        let mut s = String::new();
        format_float(3.0, &mut s);
        assert_eq!(s, "3");
        s.clear();
        format_float(0.1, &mut s);
        assert_eq!(s, "0.1");
    }
}
