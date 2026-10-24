//! In-memory notebook model.
//!
//! Every multiline field (cell source, stream text, text-like mime payloads)
//! is a single `String` here; the on-disk line arrays are an encoding detail
//! handled by the parser and writer.

use serde_json::Value;

use crate::format::FormatHint;

/// A JSON object with insertion order preserved (`serde_json` is built with
/// `preserve_order` in this workspace).
pub type Map = serde_json::Map<String, Value>;

/// A Jupyter notebook (nbformat 4.x).
#[derive(Clone, Debug, PartialEq)]
pub struct Notebook {
    pub nbformat: u32,
    pub nbformat_minor: u32,
    /// Notebook metadata (`kernelspec`, `language_info`, anything else), in file order.
    pub metadata: Map,
    pub cells: Vec<Cell>,
    /// Unknown top-level keys (invalid per the schema, but kept so saves never lose data).
    pub extra: Map,
    /// How the file this notebook was read from was formatted. [`crate::to_string`]
    /// reuses it so re-saving an untouched notebook reproduces its bytes.
    pub format: FormatHint,
}

/// One notebook cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// nbformat >= 4.5 cell id. `None` for older notebooks (and must stay `None`
    /// for them: an `id` key is a schema violation before 4.5).
    pub id: Option<String>,
    pub cell_type: CellType,
    pub source: String,
    pub metadata: Map,
    /// Code cells only (always `None` for markdown/raw).
    pub execution_count: Option<i64>,
    /// Code cells only (always empty for markdown/raw).
    pub outputs: Vec<Output>,
    /// Markdown/raw cell attachments: `{ "name.png": { "image/png": "<base64>" } }`.
    /// Text-like mime payloads inside are joined into single strings.
    pub attachments: Option<Value>,
    /// Keys the schema does not define for this cell type (e.g. `outputs` on a
    /// markdown cell written by some tool). Written back verbatim.
    pub extra: Map,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellType {
    Code,
    Markdown,
    Raw,
}

impl CellType {
    pub fn as_str(self) -> &'static str {
        match self {
            CellType::Code => "code",
            CellType::Markdown => "markdown",
            CellType::Raw => "raw",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<CellType> {
        match s {
            "code" => Some(CellType::Code),
            "markdown" => Some(CellType::Markdown),
            "raw" => Some(CellType::Raw),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamName {
    Stdout,
    Stderr,
}

impl StreamName {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamName::Stdout => "stdout",
            StreamName::Stderr => "stderr",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<StreamName> {
        match s {
            "stdout" => Some(StreamName::Stdout),
            "stderr" => Some(StreamName::Stderr),
            _ => None,
        }
    }
}

/// One code-cell output.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Stream {
        name: StreamName,
        text: String,
    },
    DisplayData {
        data: MimeBundle,
        metadata: Map,
        /// Runtime-only (`display_id` for `update_display_data`). Never written
        /// to disk: nbformat's schema forbids it and Jupyter drops it on save.
        transient: Option<Value>,
    },
    ExecuteResult {
        execution_count: Option<i64>,
        data: MimeBundle,
        metadata: Map,
    },
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
    /// An output whose `output_type` is unknown, or whose shape does not fit the
    /// typed variants (e.g. a stream named neither stdout nor stderr). Kept as the
    /// raw JSON object (including `output_type`) and written back verbatim.
    Unknown(Map),
}

impl Output {
    pub fn output_type(&self) -> &str {
        match self {
            Output::Stream { .. } => "stream",
            Output::DisplayData { .. } => "display_data",
            Output::ExecuteResult { .. } => "execute_result",
            Output::Error { .. } => "error",
            Output::Unknown(map) => map.get("output_type").and_then(Value::as_str).unwrap_or(""),
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self, Output::Stream { .. })
    }

    pub fn is_display_data(&self) -> bool {
        matches!(self, Output::DisplayData { .. })
    }

    pub fn is_execute_result(&self) -> bool {
        matches!(self, Output::ExecuteResult { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Output::Error { .. })
    }

    /// The mime bundle of a display_data / execute_result output.
    pub fn data(&self) -> Option<&MimeBundle> {
        match self {
            Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => Some(data),
            _ => None,
        }
    }

    pub fn data_mut(&mut self) -> Option<&mut MimeBundle> {
        match self {
            Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => Some(data),
            _ => None,
        }
    }

    /// Output metadata of a display_data / execute_result output.
    pub fn metadata(&self) -> Option<&Map> {
        match self {
            Output::DisplayData { metadata, .. } | Output::ExecuteResult { metadata, .. } => {
                Some(metadata)
            }
            _ => None,
        }
    }

    /// `transient.display_id` of a display_data output, if any.
    pub fn display_id(&self) -> Option<&str> {
        match self {
            Output::DisplayData { transient: Some(t), .. } => {
                t.get("display_id").and_then(Value::as_str)
            }
            _ => None,
        }
    }

    pub fn execution_count(&self) -> Option<i64> {
        match self {
            Output::ExecuteResult { execution_count, .. } => *execution_count,
            _ => None,
        }
    }
}

/// A mime bundle (`data` of display_data / execute_result), in file order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MimeBundle(pub Vec<(String, MimeData)>);

/// One mime payload.
#[derive(Clone, Debug, PartialEq)]
pub enum MimeData {
    /// Text payload, already joined from nbformat's line arrays. Base64 image
    /// data (`image/png`, `image/jpeg`, ...) is also `Text`.
    Text(String),
    /// JSON payload: `application/json`, `application/*+json` (widgets, vega,
    /// plotly), or any non-string value.
    Json(Value),
}

impl MimeData {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            MimeData::Text(s) => Some(s),
            MimeData::Json(Value::String(s)) => Some(s),
            MimeData::Json(_) => None,
        }
    }

    pub fn as_json(&self) -> Option<&Value> {
        match self {
            MimeData::Json(v) => Some(v),
            MimeData::Text(_) => None,
        }
    }
}

impl MimeBundle {
    pub fn new() -> Self {
        MimeBundle(Vec::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, mime: &str) -> Option<&MimeData> {
        self.0.iter().find(|(k, _)| k == mime).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, mime: &str) -> Option<&mut MimeData> {
        self.0.iter_mut().find(|(k, _)| k == mime).map(|(_, v)| v)
    }

    /// Text payload for `mime`, if present and textual.
    pub fn text(&self, mime: &str) -> Option<&str> {
        self.get(mime).and_then(MimeData::as_text)
    }

    pub fn contains(&self, mime: &str) -> bool {
        self.get(mime).is_some()
    }

    /// Insert or replace (replacing keeps the original position).
    pub fn insert(&mut self, mime: impl Into<String>, data: MimeData) {
        let mime = mime.into();
        match self.0.iter_mut().find(|(k, _)| *k == mime) {
            Some(slot) => slot.1 = data,
            None => self.0.push((mime, data)),
        }
    }

    pub fn remove(&mut self, mime: &str) -> Option<MimeData> {
        let i = self.0.iter().position(|(k, _)| k == mime)?;
        Some(self.0.remove(i).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &MimeData)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn mime_types(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    /// The best representation this bundle offers, given the mime types the
    /// renderer supports in priority order (most preferred first). Returns the
    /// first entry of `supported` present in the bundle.
    ///
    /// ```
    /// # use herse_nb::{MimeBundle, MimeData};
    /// let mut b = MimeBundle::new();
    /// b.insert("text/plain", MimeData::Text("<Figure>".into()));
    /// b.insert("image/png", MimeData::Text("iVBOR...".into()));
    /// let (mime, _) = b.richest(&["text/html", "image/png", "text/plain"]).unwrap();
    /// assert_eq!(mime, "image/png");
    /// ```
    pub fn richest<'a>(&'a self, supported: &[&str]) -> Option<(&'a str, &'a MimeData)> {
        supported.iter().find_map(|want| {
            self.0.iter().find(|(k, _)| k == want).map(|(k, v)| (k.as_str(), v))
        })
    }
}

impl FromIterator<(String, MimeData)> for MimeBundle {
    fn from_iter<T: IntoIterator<Item = (String, MimeData)>>(iter: T) -> Self {
        MimeBundle(iter.into_iter().collect())
    }
}

/// Whether nbformat treats a mime type's payload as JSON (left alone, never
/// joined/split): `application/json` and `application/*+json`.
pub fn is_json_mime(mime: &str) -> bool {
    mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
}

/// Whether nbformat splits a mime type's string payload into a line array on
/// write (`text/*`, `image/svg+xml`, `application/javascript`).
pub fn is_split_mime(mime: &str) -> bool {
    mime.starts_with("text/") || mime == "image/svg+xml" || mime == "application/javascript"
}

/// A fresh nbformat 4.5 cell id: 8 lowercase hex chars, like nbformat's
/// `uuid.uuid4().hex[:8]`.
pub fn new_cell_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    format!("{:08x}", h.finish() as u32)
}

impl Cell {
    /// An empty cell without an id. Use [`Notebook::new_cell`] to get one with
    /// an id when the notebook's format calls for it.
    pub fn new(cell_type: CellType) -> Cell {
        Cell {
            id: None,
            cell_type,
            source: String::new(),
            metadata: Map::new(),
            execution_count: None,
            outputs: Vec::new(),
            attachments: None,
            extra: Map::new(),
        }
    }

    /// An empty cell with a fresh nbformat 4.5 id.
    pub fn with_new_id(cell_type: CellType) -> Cell {
        Cell { id: Some(new_cell_id()), ..Cell::new(cell_type) }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Cell {
        self.source = source.into();
        self
    }

    pub fn is_code(&self) -> bool {
        self.cell_type == CellType::Code
    }

    /// Clear outputs and execution count (code cells).
    pub fn clear_outputs(&mut self) {
        self.outputs.clear();
        self.execution_count = None;
    }
}

impl Notebook {
    /// A new nbformat 4.5 notebook with one empty code cell, like Jupyter's
    /// "New Notebook".
    pub fn new_empty(kernel_name: &str, display_name: &str, language: &str) -> Notebook {
        let mut kernelspec = Map::new();
        kernelspec.insert("display_name".into(), Value::String(display_name.into()));
        kernelspec.insert("language".into(), Value::String(language.into()));
        kernelspec.insert("name".into(), Value::String(kernel_name.into()));
        let mut language_info = Map::new();
        language_info.insert("name".into(), Value::String(language.into()));
        let mut metadata = Map::new();
        metadata.insert("kernelspec".into(), Value::Object(kernelspec));
        metadata.insert("language_info".into(), Value::Object(language_info));
        Notebook {
            nbformat: 4,
            nbformat_minor: 5,
            metadata,
            cells: vec![Cell::with_new_id(CellType::Code)],
            extra: Map::new(),
            format: FormatHint::jupyter(),
        }
    }

    /// Whether cells in this notebook carry ids (nbformat >= 4.5).
    pub fn uses_cell_ids(&self) -> bool {
        self.nbformat > 4 || (self.nbformat == 4 && self.nbformat_minor >= 5)
    }

    /// A new empty cell for this notebook: with a fresh id (unique within the
    /// notebook) when the notebook is nbformat >= 4.5, without one otherwise.
    pub fn new_cell(&self, cell_type: CellType) -> Cell {
        let mut cell = Cell::new(cell_type);
        if self.uses_cell_ids() {
            cell.id = Some(self.fresh_cell_id());
        }
        cell
    }

    /// A cell id not used by any cell of this notebook.
    pub fn fresh_cell_id(&self) -> String {
        loop {
            let id = new_cell_id();
            if !self.cells.iter().any(|c| c.id.as_deref() == Some(id.as_str())) {
                return id;
            }
        }
    }

    /// Upgrade a 4.0–4.4 notebook to 4.5: bump `nbformat_minor` and give every
    /// cell without an id a fresh one (what Jupyter does when it upgrades).
    pub fn upgrade_to_4_5(&mut self) {
        if self.nbformat == 4 && self.nbformat_minor < 5 {
            self.nbformat_minor = 5;
        }
        if !self.uses_cell_ids() {
            return;
        }
        for i in 0..self.cells.len() {
            if self.cells[i].id.is_none() {
                let id = self.fresh_cell_id();
                self.cells[i].id = Some(id);
            }
        }
    }

    /// `metadata.kernelspec.display_name`.
    pub fn kernel_display_name(&self) -> Option<&str> {
        self.metadata.get("kernelspec")?.get("display_name")?.as_str()
    }

    /// `metadata.kernelspec.name`.
    pub fn kernel_name(&self) -> Option<&str> {
        self.metadata.get("kernelspec")?.get("name")?.as_str()
    }

    /// Notebook language: `metadata.language_info.name`, else `metadata.kernelspec.language`.
    pub fn language(&self) -> Option<&str> {
        self.metadata
            .get("language_info")
            .and_then(|l| l.get("name"))
            .and_then(Value::as_str)
            .or_else(|| self.metadata.get("kernelspec")?.get("language")?.as_str())
    }

    /// Give fresh ids to cells whose id repeats an earlier cell's (invalid per
    /// nbformat 4.5, common in copy-pasted notebooks). Jupyter does this on
    /// read; `parse` does not, so untouched files re-save identically.
    /// Returns how many ids changed.
    pub fn repair_duplicate_cell_ids(&mut self) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut dups = Vec::new();
        for (i, c) in self.cells.iter().enumerate() {
            if let Some(id) = &c.id
                && !seen.insert(id.clone())
            {
                dups.push(i);
            }
        }
        for &i in &dups {
            let id = self.fresh_cell_id();
            self.cells[i].id = Some(id);
        }
        dups.len()
    }

    /// Index of the cell with this id.
    pub fn cell_index(&self, id: &str) -> Option<usize> {
        self.cells.iter().position(|c| c.id.as_deref() == Some(id))
    }
}
