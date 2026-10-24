//! The shared document: schema and `Notebook` <-> doc conversion.
//!
//! # Schema
//!
//! The layout is JupyterLab's real-time collaboration schema: `jupyter_ydoc`'s
//! `YNotebook` as of its document version 2.0.0 (the one current
//! jupyter-collaboration servers and `@jupyter/ydoc` use), so a Herse doc can
//! later be bridged to a Jupyter server. The older document version 1.0.0
//! (jupyter_ydoc 2.x) differs only in storing outputs as plain JSON values
//! and having no `execution_state`; Herse reads both forms.
//!
//! ```text
//! meta:  Y.Map  { nbformat: int, nbformat_minor: int, metadata: Y.Map<key, JSON> }
//! cells: Y.Array< Y.Map {
//!            id: string, cell_type: "code" | "markdown" | "raw",
//!            source: Y.Text, metadata: Y.Map<key, JSON>,
//!            attachments?: JSON,                       // when the cell has any
//!            execution_count: int | null,              // code cells
//!            execution_state: "idle" | "running" ...,  // code cells (runtime only)
//!            outputs: Y.Array< Y.Map {                 // code cells
//!                output_type, ...nbformat keys as JSON, // stream `text` is a Y.Text
//!            } >,
//!            <unknown cell keys>: JSON,                // nbformat-invalid keys, kept
//!        } >
//! ```
//!
//! Herse adds three root maps that Jupyter ignores:
//!
//! * `herse` — `{ version: 1, extra?: "<JSON of unknown top-level notebook keys>" }`.
//! * `herse:comments` — comment threads (see [`crate::comments`]); they are
//!   *not* kept in `metadata` while live, so concurrent comments merge.
//! * `herse:fidelity` — exact-JSON records (`"c\x1f<cell id>\x1f<slot>"` /
//!   `"nb\x1fmeta"` -> JSON text) for values whose object key order or
//!   integers beyond ±2^31 the Yjs `Any` encoding would not reproduce. A
//!   record is only written when needed (unsorted keys, big integers) and is
//!   only trusted while it still matches the live value, so a stale record can
//!   never resurrect old content.
//!
//! # Deviations from jupyter_ydoc
//!
//! * No defaults are invented: jupyter_ydoc adds empty `kernelspec` /
//!   `language_info` metadata and a blank cell to empty notebooks; Herse keeps
//!   the notebook exactly as it is.
//! * Text offsets are UTF-8 bytes on the Rust side (the doc uses
//!   `OffsetKind::Bytes`); the wire format is the standard Yjs encoding, so
//!   this is invisible to UTF-16 (JavaScript) peers.
//! * Display-data `transient` (runtime display ids) is kept in the output map.
//! * Unknown top-level notebook keys live in the `herse` root instead of being
//!   dropped.
//! * Cell ids are always present in the doc (jupyter_ydoc assigns uuids too);
//!   like jupyter_ydoc, they are stripped again when reading a notebook whose
//!   nbformat is older than 4.5.

use std::collections::HashMap;
use std::sync::Arc;

use herse_nb::{Cell, CellType, FormatHint, Map, MimeBundle, MimeData, Notebook, Output, StreamName, Value, is_json_mime};
use yrs::types::text::TextPrelim;
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, GetString, In, Map as _, MapPrelim, MapRef, Number, OffsetKind, Options, Out,
    ReadTxn, Text, TextRef, Transact, TransactionMut,
};

use crate::comments;
use crate::error::{Result, invalid};
use crate::json::{from_any, needs_record, reconcile, to_any};

pub(crate) const CELLS: &str = "cells";
pub(crate) const META: &str = "meta";
pub(crate) const HERSE: &str = "herse";
pub(crate) const FIDELITY: &str = "herse:fidelity";
pub(crate) const COMMENTS: &str = "herse:comments";

/// Herse doc schema version (in the `herse` root).
pub(crate) const SCHEMA_VERSION: i64 = 1;

const SEP: char = '\u{1f}';
const NB_META_RECORD: &str = "nb\u{1f}meta";

/// The root types of a notebook doc.
#[derive(Clone)]
pub(crate) struct Roots {
    pub cells: ArrayRef,
    pub meta: MapRef,
    pub herse: MapRef,
    pub fidelity: MapRef,
    pub comments: MapRef,
}

impl Roots {
    pub fn new(doc: &Doc) -> Roots {
        Roots {
            cells: doc.get_or_insert_array(CELLS),
            meta: doc.get_or_insert_map(META),
            herse: doc.get_or_insert_map(HERSE),
            fidelity: doc.get_or_insert_map(FIDELITY),
            comments: doc.get_or_insert_map(COMMENTS),
        }
    }
}

/// A doc configured the way every Herse peer configures it: UTF-8 byte
/// offsets for text, deleted content garbage-collected.
pub(crate) fn new_doc() -> Doc {
    Doc::with_options(Options { offset_kind: OffsetKind::Bytes, skip_gc: false, ..Options::default() })
}

/// A cell that got a new id when its notebook entered a shared document:
/// cells without an id get a fresh UUID, and a cell repeating an earlier
/// cell's id gets a fresh one (ids must be unique to address cells).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignedId {
    /// Index of the cell in the notebook.
    pub index: usize,
    /// The id the cell has in the shared document.
    pub id: String,
    /// The id it had before (`None`: it had none; `Some`: it was a duplicate).
    pub previous: Option<String>,
}

/// The id every cell of `nb` will have in a shared document, plus the
/// cells whose id had to be invented or replaced.
pub fn assign_cell_ids(nb: &Notebook) -> (Vec<String>, Vec<AssignedId>) {
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::with_capacity(nb.cells.len());
    let mut assigned = Vec::new();
    for (index, cell) in nb.cells.iter().enumerate() {
        let id = match &cell.id {
            Some(id) if !id.is_empty() && !seen.contains(id.as_str()) => id.clone(),
            previous => {
                let id = crate::util::new_uuid();
                assigned.push(AssignedId { index, id: id.clone(), previous: previous.clone() });
                id
            }
        };
        seen.insert(id.clone());
        ids.push(id);
    }
    (ids, assigned)
}

/// Build a shared document holding `nb`. Lossless: [`notebook_from_doc`]
/// gives back `nb` (apart from the ids listed in the returned
/// [`AssignedId`]s, and [`Notebook::format`], which is not part of the doc).
pub fn notebook_to_doc(nb: &Notebook) -> (Doc, Vec<AssignedId>) {
    let doc = new_doc();
    let roots = Roots::new(&doc);
    let (ids, assigned) = assign_cell_ids(nb);
    {
        let mut txn = doc.transact_mut();
        write_notebook(&mut txn, &roots, nb, &ids);
    }
    (doc, assigned)
}

/// Read the notebook a shared document holds (format hint: nbformat's own).
pub fn notebook_from_doc(doc: &Doc) -> Notebook {
    let roots = Roots::new(doc);
    let txn = doc.transact();
    read_notebook(&txn, &roots, FormatHint::jupyter())
}

// ---------------------------------------------------------------------------
// Fidelity records

pub(crate) fn cell_record_key(id: &str, slot: &str) -> String {
    format!("c{SEP}{id}{SEP}{slot}")
}

fn output_slot(index: usize) -> String {
    format!("out{SEP}{index}")
}

/// Write (or clear) the fidelity record for `value`.
fn put_record(txn: &mut TransactionMut, roots: &Roots, key: &str, value: &Value) {
    if needs_record(value) {
        let text = serde_json::to_string(value).unwrap_or_default();
        roots.fidelity.insert(txn, key, Any::String(Arc::from(text)));
    } else if roots.fidelity.contains_key(txn, key) {
        roots.fidelity.remove(txn, key);
    }
}

fn clear_record(txn: &mut TransactionMut, roots: &Roots, key: &str) {
    if roots.fidelity.contains_key(txn, key) {
        roots.fidelity.remove(txn, key);
    }
}

fn get_record<T: ReadTxn>(txn: &T, roots: &Roots, key: &str) -> Option<Value> {
    match roots.fidelity.get(txn, key)? {
        Out::Any(Any::String(s)) => serde_json::from_str(&s).ok(),
        _ => None,
    }
}

fn apply_record<T: ReadTxn>(txn: &T, roots: &Roots, key: &str, decoded: Value) -> Value {
    match get_record(txn, roots, key) {
        Some(record) => reconcile(decoded, &record),
        None => decoded,
    }
}

/// Remove every record of cell `id` (cell deleted).
pub(crate) fn clear_cell_records(txn: &mut TransactionMut, roots: &Roots, id: &str) {
    let prefix = format!("c{SEP}{id}{SEP}");
    let keys: Vec<String> = roots.fidelity.keys(txn).filter(|k| k.starts_with(&prefix)).map(str::to_owned).collect();
    for k in keys {
        roots.fidelity.remove(txn, &k);
    }
}

/// Remove output records of cell `id` at index >= `from`.
fn clear_output_records_from(txn: &mut TransactionMut, roots: &Roots, id: &str, from: usize) {
    let prefix = format!("c{SEP}{id}{SEP}out{SEP}");
    let keys: Vec<String> = roots
        .fidelity
        .keys(txn)
        .filter(|k| k.strip_prefix(&prefix).and_then(|i| i.parse::<usize>().ok()).is_some_and(|i| i >= from))
        .map(str::to_owned)
        .collect();
    for k in keys {
        roots.fidelity.remove(txn, &k);
    }
}

// ---------------------------------------------------------------------------
// Writing

/// Fill an empty doc with `nb` (cells get `ids`, one per cell).
pub(crate) fn write_notebook(txn: &mut TransactionMut, roots: &Roots, nb: &Notebook, ids: &[String]) {
    roots.meta.insert(txn, "nbformat", Any::Number(Number::Int(nb.nbformat as i64)));
    roots.meta.insert(txn, "nbformat_minor", Any::Number(Number::Int(nb.nbformat_minor as i64)));
    let (stored, threads) = split_comments(&nb.metadata);
    roots.meta.insert(txn, "metadata", map_prelim(&stored));
    put_record(txn, roots, NB_META_RECORD, &Value::Object(nb.metadata.clone()));

    roots.herse.insert(txn, "version", Any::Number(Number::Int(SCHEMA_VERSION)));
    set_notebook_extra(txn, roots, &nb.extra);

    for thread in &threads {
        comments::write_thread(txn, &roots.comments, thread);
    }

    // Front insertion is O(1) in yrs while appending walks the whole list,
    // so build the list back to front.
    for (cell, id) in nb.cells.iter().zip(ids).rev() {
        insert_cell(txn, roots, 0, cell, id);
    }
}

/// Notebook metadata without `herse.comments` (they live in their own root
/// while a session runs), and the parsed threads. Comments that do not parse
/// are left in the metadata untouched.
pub(crate) fn split_comments(metadata: &Map) -> (Map, Vec<comments::Thread>) {
    let Some(threads) = comments::threads_from_metadata(metadata) else {
        return (metadata.clone(), Vec::new());
    };
    let mut stored = metadata.clone();
    if let Some(Value::Object(herse)) = stored.get_mut("herse") {
        herse.shift_remove("comments");
        if herse.is_empty() {
            stored.shift_remove("herse");
        }
    }
    (stored, threads)
}

pub(crate) fn set_notebook_extra(txn: &mut TransactionMut, roots: &Roots, extra: &Map) {
    if extra.is_empty() {
        if roots.herse.contains_key(txn, "extra") {
            roots.herse.remove(txn, "extra");
        }
    } else {
        let text = serde_json::to_string(extra).unwrap_or_default();
        roots.herse.insert(txn, "extra", Any::String(Arc::from(text)));
    }
}

fn map_prelim(m: &Map) -> MapPrelim {
    m.iter().map(|(k, v)| (k.as_str(), In::Any(to_any(v)))).collect()
}

/// Insert `cell` (with id `id`) at `index` of the cells array, with its
/// fidelity records. Returns the new cell map.
pub(crate) fn insert_cell(txn: &mut TransactionMut, roots: &Roots, index: u32, cell: &Cell, id: &str) -> MapRef {
    let mut entries: Vec<(Arc<str>, In)> = vec![
        ("id".into(), In::Any(Any::String(id.into()))),
        ("cell_type".into(), In::Any(Any::String(cell.cell_type.as_str().into()))),
        ("source".into(), In::from(TextPrelim::new(cell.source.as_str()))),
        ("metadata".into(), In::Map(map_prelim(&cell.metadata))),
    ];
    if let Some(att) = &cell.attachments {
        entries.push(("attachments".into(), In::Any(to_any(att))));
    }
    if cell.cell_type == CellType::Code {
        entries.push(("execution_count".into(), In::Any(opt_int(cell.execution_count))));
        entries.push(("execution_state".into(), In::Any(Any::String("idle".into()))));
        entries.push(("outputs".into(), In::Array(ArrayPrelim::default())));
    }
    for (k, v) in &cell.extra {
        if !entries.iter().any(|(e, _)| &**e == k.as_str()) {
            entries.push((k.as_str().into(), In::Any(to_any(v))));
        }
    }
    let map = roots.cells.insert(txn, index, MapPrelim::from_iter(entries));
    if cell.cell_type == CellType::Code
        && !cell.outputs.is_empty()
        && let Some(Out::YArray(arr)) = map.get(txn, "outputs")
    {
        insert_outputs(txn, &arr, 0, &cell.outputs);
    }
    write_cell_records(txn, roots, cell, id);
    map
}

/// Insert `outputs` at `at`, back to front (each insert at the same index
/// only walks `at` items; appending one by one would be quadratic).
fn insert_outputs(txn: &mut TransactionMut, arr: &ArrayRef, at: u32, outputs: &[Output]) {
    for o in outputs.iter().rev() {
        arr.insert(txn, at, output_prelim(o));
    }
}

fn write_cell_records(txn: &mut TransactionMut, roots: &Roots, cell: &Cell, id: &str) {
    put_record(txn, roots, &cell_record_key(id, "meta"), &Value::Object(cell.metadata.clone()));
    if let Some(att) = &cell.attachments {
        put_record(txn, roots, &cell_record_key(id, "att"), att);
    }
    if !cell.extra.is_empty() {
        put_record(txn, roots, &cell_record_key(id, "extra"), &Value::Object(cell.extra.clone()));
    }
    for (i, o) in cell.outputs.iter().enumerate() {
        put_output_record(txn, roots, id, i, o);
    }
}

fn put_output_record(txn: &mut TransactionMut, roots: &Roots, id: &str, index: usize, o: &Output) {
    let key = cell_record_key(id, &output_slot(index));
    if o.is_stream() {
        clear_record(txn, roots, &key);
    } else {
        put_record(txn, roots, &key, &Value::Object(output_json(o)));
    }
}

fn opt_int(v: Option<i64>) -> Any {
    v.map_or(Any::Null, |n| Any::Number(Number::Int(n)))
}

/// An output as nbformat's JSON object (plus `transient` on display data).
pub(crate) fn output_json(o: &Output) -> Map {
    let mut m = Map::new();
    match o {
        Output::Stream { name, text } => {
            m.insert("name".into(), Value::String(name.as_str().into()));
            m.insert("output_type".into(), Value::String("stream".into()));
            m.insert("text".into(), Value::String(text.clone()));
        }
        Output::DisplayData { data, metadata, transient } => {
            m.insert("data".into(), Value::Object(bundle_json(data)));
            m.insert("metadata".into(), Value::Object(metadata.clone()));
            m.insert("output_type".into(), Value::String("display_data".into()));
            if let Some(t) = transient {
                m.insert("transient".into(), t.clone());
            }
        }
        Output::ExecuteResult { execution_count, data, metadata } => {
            m.insert("data".into(), Value::Object(bundle_json(data)));
            m.insert("execution_count".into(), execution_count.map_or(Value::Null, Value::from));
            m.insert("metadata".into(), Value::Object(metadata.clone()));
            m.insert("output_type".into(), Value::String("execute_result".into()));
        }
        Output::Error { ename, evalue, traceback } => {
            m.insert("ename".into(), Value::String(ename.clone()));
            m.insert("evalue".into(), Value::String(evalue.clone()));
            m.insert("output_type".into(), Value::String("error".into()));
            m.insert("traceback".into(), Value::Array(traceback.iter().cloned().map(Value::String).collect()));
        }
        Output::Unknown(map) => return map.clone(),
    }
    m
}

fn bundle_json(b: &MimeBundle) -> Map {
    b.0.iter()
        .map(|(mime, data)| {
            let v = match data {
                MimeData::Text(s) => Value::String(s.clone()),
                MimeData::Json(v) => v.clone(),
            };
            (mime.clone(), v)
        })
        .collect()
}

/// An output as a Y.Map: JSON values, except a stream's text, which is a
/// Y.Text (appends stay cheap), as in jupyter_ydoc.
pub(crate) fn output_prelim(o: &Output) -> MapPrelim {
    match o {
        Output::Stream { name, text } => MapPrelim::from_iter([
            ("output_type", In::Any(Any::String("stream".into()))),
            ("name", In::Any(Any::String(name.as_str().into()))),
            ("text", In::from(TextPrelim::new(text.as_str()))),
        ]),
        other => map_prelim(&output_json(other)),
    }
}

// ---------------------------------------------------------------------------
// Reading

/// Read the whole notebook (`format` becomes its [`Notebook::format`]).
pub(crate) fn read_notebook<T: ReadTxn>(txn: &T, roots: &Roots, format: FormatHint) -> Notebook {
    let (nbformat, nbformat_minor) = read_nbformat(txn, roots);
    let strip_ids = nbformat == 4 && nbformat_minor <= 4;
    let cells = roots
        .cells
        .iter(txn)
        .filter_map(|out| match out {
            Out::YMap(m) => Some(read_cell(txn, roots, &m, strip_ids).0),
            _ => None,
        })
        .collect();
    Notebook {
        nbformat,
        nbformat_minor,
        metadata: read_notebook_metadata(txn, roots),
        cells,
        extra: read_notebook_extra(txn, roots),
        format,
    }
}

pub(crate) fn read_nbformat<T: ReadTxn>(txn: &T, roots: &Roots) -> (u32, u32) {
    let num = |key: &str, default: u32| match roots.meta.get(txn, key) {
        Some(Out::Any(Any::Number(n))) => n.as_i64().and_then(|n| u32::try_from(n).ok()).unwrap_or(default),
        _ => default,
    };
    (num("nbformat", 4), num("nbformat_minor", 5))
}

/// Notebook metadata, with the live comment threads stored back under
/// `herse.comments` (so it is ready to save).
pub(crate) fn read_notebook_metadata<T: ReadTxn>(txn: &T, roots: &Roots) -> Map {
    let mut metadata = match roots.meta.get(txn, "metadata") {
        Some(out) => match out_to_json(txn, &out) {
            Value::Object(m) => m,
            _ => Map::new(),
        },
        None => Map::new(),
    };
    let threads = comments::read_threads(txn, &roots.comments);
    if !threads.is_empty() {
        comments::threads_into_metadata(&mut metadata, &threads);
        metadata.sort_keys();
    }
    match apply_record(txn, roots, NB_META_RECORD, Value::Object(metadata)) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

pub(crate) fn read_notebook_extra<T: ReadTxn>(txn: &T, roots: &Roots) -> Map {
    match roots.herse.get(txn, "extra") {
        Some(Out::Any(Any::String(s))) => serde_json::from_str(&s).unwrap_or_default(),
        _ => Map::new(),
    }
}

/// The id of a cell map (`""` if it has none, which only foreign peers write).
pub(crate) fn cell_id<T: ReadTxn>(txn: &T, map: &MapRef) -> String {
    match map.get(txn, "id") {
        Some(Out::Any(Any::String(s))) => s.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn cell_type_of<T: ReadTxn>(txn: &T, map: &MapRef) -> CellType {
    match map.get(txn, "cell_type") {
        Some(Out::Any(Any::String(s))) => CellType::from_str_opt(&s).unwrap_or(CellType::Raw),
        _ => CellType::Raw,
    }
}

/// Keys of a cell map that are not cell fields (for this cell type).
fn is_cell_field(key: &str, cell_type: CellType) -> bool {
    match key {
        "id" | "cell_type" | "source" | "metadata" | "attachments" | "execution_state" => true,
        "execution_count" | "outputs" => cell_type == CellType::Code,
        _ => false,
    }
}

/// Read one cell; also returns its doc id.
pub(crate) fn read_cell<T: ReadTxn>(txn: &T, roots: &Roots, map: &MapRef, strip_id: bool) -> (Cell, String) {
    let id = cell_id(txn, map);
    let cell_type = cell_type_of(txn, map);
    let mut cell = Cell::new(cell_type);
    cell.id = (!strip_id && !id.is_empty()).then(|| id.clone());
    cell.source = read_source(txn, map);
    cell.metadata = read_cell_metadata(txn, roots, map, &id);
    cell.attachments = read_attachments(txn, roots, map, &id);
    if cell_type == CellType::Code {
        cell.execution_count = read_execution_count(txn, map);
        cell.outputs = read_outputs(txn, roots, map, &id);
    }
    cell.extra = read_cell_extra(txn, roots, map, &id, cell_type);
    (cell, id)
}

pub(crate) fn read_source<T: ReadTxn>(txn: &T, map: &MapRef) -> String {
    match map.get(txn, "source") {
        Some(Out::YText(t)) => t.get_string(txn),
        Some(out) => herse_multiline(&out_to_json(txn, &out)).unwrap_or_default(),
        None => String::new(),
    }
}

pub(crate) fn read_cell_metadata<T: ReadTxn>(txn: &T, roots: &Roots, map: &MapRef, id: &str) -> Map {
    let decoded = match map.get(txn, "metadata") {
        Some(out) => out_to_json(txn, &out),
        None => Value::Object(Map::new()),
    };
    match apply_record(txn, roots, &cell_record_key(id, "meta"), decoded) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

pub(crate) fn read_attachments<T: ReadTxn>(txn: &T, roots: &Roots, map: &MapRef, id: &str) -> Option<Value> {
    match map.get(txn, "attachments") {
        None | Some(Out::Any(Any::Null | Any::Undefined)) => None,
        Some(out) => Some(apply_record(txn, roots, &cell_record_key(id, "att"), out_to_json(txn, &out))),
    }
}

pub(crate) fn read_execution_count<T: ReadTxn>(txn: &T, map: &MapRef) -> Option<i64> {
    match map.get(txn, "execution_count") {
        Some(Out::Any(Any::Number(n))) => n.as_i64(),
        _ => None,
    }
}

pub(crate) fn read_execution_state<T: ReadTxn>(txn: &T, map: &MapRef) -> Option<String> {
    match map.get(txn, "execution_state") {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

pub(crate) fn read_outputs<T: ReadTxn>(txn: &T, roots: &Roots, map: &MapRef, id: &str) -> Vec<Output> {
    match map.get(txn, "outputs") {
        Some(Out::YArray(arr)) => {
            arr.iter(txn).enumerate().map(|(i, out)| read_output_value(txn, roots, id, i, &out)).collect()
        }
        Some(Out::Any(Any::Array(items))) => items
            .iter()
            .enumerate()
            .map(|(i, a)| read_output_value(txn, roots, id, i, &Out::Any(a.clone())))
            .collect(),
        _ => Vec::new(),
    }
}

/// Output `index` of a cell's outputs array.
pub(crate) fn read_output<T: ReadTxn>(txn: &T, roots: &Roots, map: &MapRef, id: &str, index: usize) -> Option<Output> {
    match map.get(txn, "outputs")? {
        Out::YArray(arr) => arr.get(txn, index as u32).map(|out| read_output_value(txn, roots, id, index, &out)),
        _ => None,
    }
}

fn read_output_value<T: ReadTxn>(txn: &T, roots: &Roots, id: &str, index: usize, out: &Out) -> Output {
    let json = apply_record(txn, roots, &cell_record_key(id, &output_slot(index)), out_to_json(txn, out));
    output_from_json(json)
}

fn read_cell_extra<T: ReadTxn>(txn: &T, roots: &Roots, map: &MapRef, id: &str, cell_type: CellType) -> Map {
    let mut extra: Vec<(String, Value)> = map
        .iter(txn)
        .filter(|(k, _)| !is_cell_field(k, cell_type))
        .map(|(k, out)| (k.to_owned(), out_to_json(txn, &out)))
        .collect();
    if extra.is_empty() {
        return Map::new();
    }
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    match apply_record(txn, roots, &cell_record_key(id, "extra"), Value::Object(extra.into_iter().collect())) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Any doc value as JSON (Y.Text -> string, Y.Map -> object with sorted keys).
pub(crate) fn out_to_json<T: ReadTxn>(txn: &T, out: &Out) -> Value {
    match out {
        Out::Any(a) => from_any(a),
        Out::YText(t) => Value::String(t.get_string(txn)),
        Out::YArray(a) => Value::Array(a.iter(txn).map(|o| out_to_json(txn, &o)).collect()),
        Out::YMap(m) => {
            let mut entries: Vec<(String, Value)> = m.iter(txn).map(|(k, o)| (k.to_owned(), out_to_json(txn, &o))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        _ => Value::Null,
    }
}

/// nbformat's `rejoin_lines` for one value: a string, or a list of strings.
fn herse_multiline(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.iter().map(|x| x.as_str()).collect::<Option<Vec<_>>>().map(|v| v.concat()),
        _ => None,
    }
}

/// Output from its JSON object, by the same rules as `herse_nb`'s parser:
/// shapes that do not fit a typed variant stay [`Output::Unknown`].
pub(crate) fn output_from_json(v: Value) -> Output {
    match v {
        Value::Object(map) => typed_output(&map).unwrap_or(Output::Unknown(map)),
        other => {
            let mut m = Map::new();
            m.insert("output_type".into(), other);
            Output::Unknown(m)
        }
    }
}

fn typed_output(map: &Map) -> Option<Output> {
    let ot = map.get("output_type")?.as_str()?;
    let allowed: &[&str] = match ot {
        "stream" => &["output_type", "name", "text"],
        "display_data" => &["output_type", "data", "metadata", "transient"],
        "execute_result" => &["output_type", "data", "metadata", "execution_count", "transient"],
        "error" => &["output_type", "ename", "evalue", "traceback"],
        _ => return None,
    };
    if !map.keys().all(|k| allowed.contains(&k.as_str())) {
        return None;
    }
    let metadata = || -> Option<Map> {
        match map.get("metadata") {
            None => Some(Map::new()),
            Some(Value::Object(m)) => Some(m.clone()),
            Some(_) => None,
        }
    };
    let data = || -> Option<MimeBundle> { Some(bundle_from_map(map.get("data")?.as_object()?)) };
    Some(match ot {
        "stream" => Output::Stream {
            name: StreamName::from_str_opt(map.get("name")?.as_str()?)?,
            text: herse_multiline(map.get("text")?)?,
        },
        "display_data" => Output::DisplayData {
            data: data()?,
            metadata: metadata()?,
            transient: map.get("transient").filter(|t| !t.is_null()).cloned(),
        },
        "execute_result" => Output::ExecuteResult {
            execution_count: match map.get("execution_count") {
                None | Some(Value::Null) => None,
                Some(Value::Number(n)) => Some(n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))?),
                Some(_) => return None,
            },
            data: data()?,
            metadata: metadata()?,
        },
        _ => Output::Error {
            ename: map.get("ename")?.as_str()?.to_owned(),
            evalue: map.get("evalue")?.as_str()?.to_owned(),
            traceback: map
                .get("traceback")?
                .as_array()?
                .iter()
                .map(|t| t.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?,
        },
    })
}

fn bundle_from_map(map: &Map) -> MimeBundle {
    MimeBundle(
        map.iter()
            .map(|(mime, v)| {
                let data = if is_json_mime(mime) {
                    MimeData::Json(v.clone())
                } else {
                    match herse_multiline(v) {
                        Some(s) => MimeData::Text(s),
                        None => MimeData::Json(v.clone()),
                    }
                };
                (mime.clone(), data)
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Editing (used by local operations)

/// Index and map of the cell with this id. `hint` is where the caller
/// expects it (checked first, so lookups are O(1) when the hint is fresh).
pub(crate) fn find_cell<T: ReadTxn>(txn: &T, roots: &Roots, id: &str, hint: Option<usize>) -> Option<(u32, MapRef)> {
    if let Some(i) = hint
        && let Some(Out::YMap(m)) = roots.cells.get(txn, i as u32)
        && cell_id(txn, &m) == id
    {
        return Some((i as u32, m));
    }
    roots.cells.iter(txn).enumerate().find_map(|(i, out)| match out {
        Out::YMap(m) if cell_id(txn, &m) == id => Some((i as u32, m)),
        _ => None,
    })
}

pub(crate) fn source_text<T: ReadTxn>(txn: &T, map: &MapRef) -> Option<TextRef> {
    match map.get(txn, "source") {
        Some(Out::YText(t)) => Some(t),
        _ => None,
    }
}

/// Make sure the cell's source is a Y.Text (foreign peers might have
/// written a plain string) and return it.
pub(crate) fn ensure_source_text(txn: &mut TransactionMut, map: &MapRef) -> TextRef {
    if let Some(t) = source_text(txn, map) {
        return t;
    }
    let current = read_source(txn, map);
    map.insert(txn, "source", TextPrelim::new(current))
}

/// Replace `range` (UTF-8 byte offsets) of `text` with `insert`.
pub(crate) fn edit_text(txn: &mut TransactionMut, text: &TextRef, range: std::ops::Range<usize>, insert: &str) -> Result<()> {
    let current = text.get_string(txn);
    if range.start > range.end || range.end > current.len() {
        return Err(invalid(format!("byte range {range:?} outside a {}-byte source", current.len())));
    }
    if !current.is_char_boundary(range.start) || !current.is_char_boundary(range.end) {
        return Err(invalid(format!("byte range {range:?} splits a UTF-8 character")));
    }
    if range.end > range.start {
        text.remove_range(txn, range.start as u32, (range.end - range.start) as u32);
    }
    if !insert.is_empty() {
        text.insert(txn, range.start as u32, insert);
    }
    Ok(())
}

/// The single replacement turning `old` into `new`: common prefix and
/// suffix kept (on character boundaries). `None` if equal.
pub(crate) fn text_diff(old: &str, new: &str) -> Option<(std::ops::Range<usize>, String)> {
    if old == new {
        return None;
    }
    let mut prefix = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = old.bytes().rev().zip(new.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    Some((prefix..old.len() - suffix, new[prefix..new.len() - suffix].to_owned()))
}

/// Set a cell's source, sending only the changed span.
pub(crate) fn set_source(txn: &mut TransactionMut, map: &MapRef, new: &str) {
    let text = ensure_source_text(txn, map);
    let old = text.get_string(txn);
    if let Some((range, insert)) = text_diff(&old, new) {
        // In range by construction.
        let _ = edit_text(txn, &text, range, &insert);
    }
}

/// Make the Y.Map under `key` of `parent` hold exactly `new`'s top-level
/// entries, writing only keys whose value changed.
fn sync_json_map(txn: &mut TransactionMut, parent: &MapRef, key: &str, new: &Map) {
    let target = match parent.get(txn, key) {
        Some(Out::YMap(m)) => m,
        _ => {
            parent.insert(txn, key, map_prelim(new));
            return;
        }
    };
    let current: HashMap<String, Out> = target.iter(txn).map(|(k, v)| (k.to_owned(), v)).collect();
    for k in current.keys() {
        if !new.contains_key(k) {
            target.remove(txn, k);
        }
    }
    for (k, v) in new {
        let any = to_any(v);
        let same = matches!(current.get(k), Some(Out::Any(a)) if *a == any);
        if !same {
            target.insert(txn, k.as_str(), any);
        }
    }
}

pub(crate) fn set_cell_metadata(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, metadata: &Map) {
    sync_json_map(txn, map, "metadata", metadata);
    put_record(txn, roots, &cell_record_key(id, "meta"), &Value::Object(metadata.clone()));
}

pub(crate) fn set_notebook_metadata(txn: &mut TransactionMut, roots: &Roots, metadata: &Map) {
    let (stored, _) = split_comments(metadata);
    sync_json_map(txn, &roots.meta, "metadata", &stored);
    put_record(txn, roots, NB_META_RECORD, &Value::Object(metadata.clone()));
}

pub(crate) fn set_nbformat(txn: &mut TransactionMut, roots: &Roots, nbformat: u32, minor: u32) {
    if read_nbformat(txn, roots) != (nbformat, minor) {
        roots.meta.insert(txn, "nbformat", Any::Number(Number::Int(nbformat as i64)));
        roots.meta.insert(txn, "nbformat_minor", Any::Number(Number::Int(minor as i64)));
    }
}

pub(crate) fn set_attachments(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, att: Option<&Value>) {
    let key = cell_record_key(id, "att");
    match att {
        Some(v) => {
            let any = to_any(v);
            if !matches!(map.get(txn, "attachments"), Some(Out::Any(a)) if a == any) {
                map.insert(txn, "attachments", any);
            }
            put_record(txn, roots, &key, v);
        }
        None => {
            if map.contains_key(txn, "attachments") {
                map.remove(txn, "attachments");
            }
            clear_record(txn, roots, &key);
        }
    }
}

pub(crate) fn set_execution_count(txn: &mut TransactionMut, map: &MapRef, count: Option<i64>) {
    let any = opt_int(count);
    if !matches!(map.get(txn, "execution_count"), Some(Out::Any(a)) if a == any) {
        map.insert(txn, "execution_count", any);
    }
}

pub(crate) fn set_execution_state(txn: &mut TransactionMut, map: &MapRef, state: &str) {
    if read_execution_state(txn, map).as_deref() != Some(state) {
        map.insert(txn, "execution_state", Any::String(state.into()));
    }
}

/// Replace the cell's unknown keys with `extra`.
pub(crate) fn set_cell_extra(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, extra: &Map) {
    let cell_type = cell_type_of(txn, map);
    let current: Vec<(String, Out)> =
        map.iter(txn).filter(|(k, _)| !is_cell_field(k, cell_type)).map(|(k, v)| (k.to_owned(), v)).collect();
    for (k, _) in &current {
        if !extra.contains_key(k) {
            map.remove(txn, k);
        }
    }
    for (k, v) in extra {
        if is_cell_field(k, cell_type) {
            continue;
        }
        let any = to_any(v);
        let same = current.iter().any(|(ck, cv)| ck == k && matches!(cv, Out::Any(a) if *a == any));
        if !same {
            map.insert(txn, k.as_str(), any);
        }
    }
    let key = cell_record_key(id, "extra");
    if extra.is_empty() {
        clear_record(txn, roots, &key);
    } else {
        put_record(txn, roots, &key, &Value::Object(extra.clone()));
    }
}

/// Change a cell's type. Code cells gain empty outputs, a null execution
/// count and an idle execution state; other cells lose them (what the app
/// does to its model, and what jupyter_ydoc's cell schema requires).
pub(crate) fn set_cell_type(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, cell_type: CellType) {
    if cell_type_of(txn, map) == cell_type {
        return;
    }
    map.insert(txn, "cell_type", Any::String(cell_type.as_str().into()));
    if cell_type == CellType::Code {
        map.insert(txn, "execution_count", Any::Null);
        map.insert(txn, "execution_state", Any::String("idle".into()));
        map.insert(txn, "outputs", ArrayPrelim::default());
    } else {
        for key in ["execution_count", "execution_state", "outputs"] {
            if map.contains_key(txn, key) {
                map.remove(txn, key);
            }
        }
    }
    clear_output_records_from(txn, roots, id, 0);
}

fn outputs_array(txn: &mut TransactionMut, map: &MapRef) -> ArrayRef {
    match map.get(txn, "outputs") {
        Some(Out::YArray(a)) => a,
        _ => map.insert(txn, "outputs", ArrayPrelim::default()),
    }
}

/// Make the cell's outputs equal `outputs`, touching as little as possible:
/// unchanged outputs stay, a stream whose text grew gets the new text
/// appended, extra outputs are pushed or removed.
pub(crate) fn set_outputs(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, outputs: &[Output]) {
    let current = read_outputs(txn, roots, map, id);
    let arr = outputs_array(txn, map);
    let common = current.len().min(outputs.len());
    // Outputs replaced wholesale when nothing in common: one delete + one insert.
    for i in 0..common {
        if current[i] == outputs[i] {
            continue;
        }
        if let (Output::Stream { name: n0, text: t0 }, Output::Stream { name: n1, text: t1 }) = (&current[i], &outputs[i])
            && n0 == n1
            && t1.starts_with(t0.as_str())
            && let Some(Out::YMap(om)) = arr.get(txn, i as u32)
            && let Some(Out::YText(text)) = om.get(txn, "text")
        {
            text.push(txn, &t1[t0.len()..]);
            continue;
        }
        replace_output_at(txn, roots, &arr, id, i, &outputs[i]);
    }
    if current.len() > outputs.len() {
        arr.remove_range(txn, common as u32, (current.len() - common) as u32);
        clear_output_records_from(txn, roots, id, common);
    }
    insert_outputs(txn, &arr, common as u32, &outputs[common..]);
    for (i, o) in outputs.iter().enumerate().skip(common) {
        put_output_record(txn, roots, id, i, o);
    }
}

fn replace_output_at(txn: &mut TransactionMut, roots: &Roots, arr: &ArrayRef, id: &str, index: usize, output: &Output) {
    arr.remove(txn, index as u32);
    arr.insert(txn, index as u32, output_prelim(output));
    put_output_record(txn, roots, id, index, output);
}

pub(crate) fn push_output(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, output: &Output) {
    let arr = outputs_array(txn, map);
    let index = arr.len(txn);
    arr.insert(txn, index, output_prelim(output));
    put_output_record(txn, roots, id, index as usize, output);
}

pub(crate) fn replace_output(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, index: usize, output: &Output) -> Result<()> {
    let arr = outputs_array(txn, map);
    if index >= arr.len(txn) as usize {
        return Err(invalid(format!("output index {index} out of range")));
    }
    replace_output_at(txn, roots, &arr, id, index, output);
    Ok(())
}

pub(crate) fn append_stream_text(txn: &mut TransactionMut, map: &MapRef, index: usize, text: &str) -> Result<()> {
    let arr = outputs_array(txn, map);
    let Some(Out::YMap(om)) = arr.get(txn, index as u32) else {
        return Err(invalid(format!("output index {index} out of range")));
    };
    match (om.get(txn, "output_type"), om.get(txn, "text")) {
        (Some(Out::Any(Any::String(t))), Some(Out::YText(yt))) if &*t == "stream" => {
            yt.push(txn, text);
            Ok(())
        }
        _ => Err(invalid(format!("output {index} is not a stream output"))),
    }
}

/// Everything about an existing cell made equal to `cell` (id unchanged).
pub(crate) fn update_cell(txn: &mut TransactionMut, roots: &Roots, map: &MapRef, id: &str, cell: &Cell) {
    set_cell_type(txn, roots, map, id, cell.cell_type);
    set_source(txn, map, &cell.source);
    if read_cell_metadata(txn, roots, map, id) != cell.metadata {
        set_cell_metadata(txn, roots, map, id, &cell.metadata);
    }
    if read_attachments(txn, roots, map, id) != cell.attachments {
        set_attachments(txn, roots, map, id, cell.attachments.as_ref());
    }
    if cell.cell_type == CellType::Code {
        set_execution_count(txn, map, cell.execution_count);
        set_outputs(txn, roots, map, id, &cell.outputs);
    }
    if read_cell_extra(txn, roots, map, id, cell.cell_type) != cell.extra {
        set_cell_extra(txn, roots, map, id, &cell.extra);
    }
}

/// Move the cell at `from` to `to` (index in the final list) the way
/// jupyter_ydoc does: delete it and insert a copy (Yjs 13 has no array move).
pub(crate) fn move_cell(txn: &mut TransactionMut, roots: &Roots, from: u32, to: u32) {
    let Some(Out::YMap(map)) = roots.cells.get(txn, from) else { return };
    let (cell, id) = read_cell(txn, roots, &map, false);
    let state = read_execution_state(txn, &map);
    roots.cells.remove(txn, from);
    let new = insert_cell(txn, roots, to, &cell, &id);
    if let Some(state) = state.filter(|s| s != "idle") {
        new.insert(txn, "execution_state", Any::String(state.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jupyter_ydoc_v1_cells() {
        // jupyter_ydoc 2.x (document version 1.0.0): outputs are plain JSON
        // in the Y.Array, stream text a list of strings, no execution_state.
        let doc = new_doc();
        let roots = Roots::new(&doc);
        {
            let mut txn = doc.transact_mut();
            roots.meta.insert(&mut txn, "nbformat", Any::Number(Number::Int(4)));
            roots.meta.insert(&mut txn, "nbformat_minor", Any::Number(Number::Int(5)));
            roots.meta.insert(&mut txn, "metadata", MapPrelim::default());
            let stream = to_any(&serde_json::json!({"output_type": "stream", "name": "stdout", "text": ["a\n", "b\n"]}));
            let result = to_any(&serde_json::json!({
                "output_type": "execute_result", "execution_count": 1.0,
                "data": {"text/plain": ["1"]}, "metadata": {}
            }));
            roots.cells.insert(
                &mut txn,
                0,
                MapPrelim::from_iter([
                    ("id", In::Any(Any::String("c1".into()))),
                    ("cell_type", In::Any(Any::String("code".into()))),
                    ("source", In::from(TextPrelim::new("1"))),
                    ("metadata", In::Map(MapPrelim::default())),
                    ("execution_count", In::Any(Any::Number(Number::Float(1.0)))),
                    ("outputs", In::Array(ArrayPrelim::from_iter([In::Any(stream), In::Any(result)]))),
                ]),
            );
        }
        let nb = notebook_from_doc(&doc);
        let cell = &nb.cells[0];
        assert_eq!(cell.execution_count, Some(1));
        assert_eq!(cell.outputs[0], Output::Stream { name: StreamName::Stdout, text: "a\nb\n".into() });
        assert_eq!(cell.outputs[1].execution_count(), Some(1));
        assert_eq!(cell.outputs[1].data().unwrap().text("text/plain"), Some("1"));
        assert!(cell.extra.is_empty());
    }

    #[test]
    fn diff_respects_char_boundaries() {
        assert_eq!(text_diff("abc", "abc"), None);
        assert_eq!(text_diff("abc", "abXc"), Some((2..2, "X".into())));
        assert_eq!(text_diff("héllo", "hello"), Some((1..3, "e".into())));
        // Shared first byte (0xE2) of two different 3-byte chars: no split.
        assert_eq!(text_diff("a€b", "a₹b"), Some((1..4, "₹".into())));
        assert_eq!(text_diff("aaaa", "aa"), Some((2..4, String::new())));
        assert_eq!(text_diff("", "x"), Some((0..0, "x".into())));
    }
}
