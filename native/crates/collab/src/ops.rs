//! Local edits.

use std::ops::Range;
use std::sync::Mutex;

use herse_nb::{Cell, CellType, Map, Notebook, Output, Value};
use yrs::{Array, Out, TransactionMut};

use crate::changes::{self, ListOp, Tracker};
use crate::doc::{self, AssignedId, Roots};
use crate::error::{Result, invalid};

/// An edit made by this peer, applied with [`crate::Session::apply_local`].
///
/// Cells are addressed by id (stable while others insert and delete cells);
/// byte ranges are UTF-8 offsets into the cell source as the app has it,
/// which is also the doc's current text as long as the app applied every
/// event returned so far (see [`crate::Session::poll`]).
#[derive(Clone, Debug, PartialEq)]
pub enum LocalOp {
    /// Insert a cell at `index` (0..=len). A cell without an id, or with one
    /// already in use, gets a fresh UUID (returned by `apply_local`).
    InsertCell { index: usize, cell: Cell },
    DeleteCell { id: String },
    /// Move a cell to `to`, its index after the move.
    MoveCell { id: String, to: usize },
    /// Make the cell with `cell.id` equal to `cell`, sending only what
    /// differs (source as one minimal replacement, changed metadata keys,
    /// appended stream text, ...).
    UpdateCell { cell: Cell },
    /// Replace bytes `range` of the source with `text` (one keystroke,
    /// paste, deletion...). The cheapest edit; use it while typing.
    EditSource { id: String, range: Range<usize>, text: String },
    /// Set the whole source (sent as the minimal single replacement).
    SetSource { id: String, source: String },
    /// Change the cell type: code cells get empty outputs and no execution
    /// count (replacing unknown keys of those names), other cells lose them.
    SetCellType { id: String, cell_type: CellType },
    /// Replace the cell metadata (only changed top-level keys are sent).
    SetCellMetadata { id: String, metadata: Map },
    SetAttachments { id: String, attachments: Option<Value> },
    /// Replace the outputs (diffed: unchanged outputs are not resent, grown
    /// stream text is appended). Decodes the current outputs to compare; for
    /// streaming kernel output prefer `PushOutput` / `AppendStreamText`.
    SetOutputs { id: String, outputs: Vec<Output> },
    PushOutput { id: String, output: Output },
    /// Replace output `index` (e.g. `update_display_data`).
    ReplaceOutput { id: String, index: usize, output: Output },
    /// Append to the text of stream output `index`.
    AppendStreamText { id: String, index: usize, text: String },
    SetExecutionCount { id: String, execution_count: Option<i64> },
    /// Runtime state shown to others (`"running"`, `"idle"`); not saved.
    SetExecutionState { id: String, state: String },
    /// Replace the notebook metadata (`herse.comments` is ignored: comments
    /// change through the comment methods).
    SetNotebookMetadata { metadata: Map },
}

fn hint(tracker: &Mutex<Tracker>, id: &str) -> Option<usize> {
    tracker.lock().unwrap_or_else(|e| e.into_inner()).index_of(id)
}

fn find(txn: &TransactionMut, roots: &Roots, tracker: &Mutex<Tracker>, id: &str) -> Result<(u32, yrs::MapRef)> {
    doc::find_cell(txn, roots, id, hint(tracker, id)).ok_or_else(|| invalid(format!("no cell with id {id:?}")))
}

/// Apply one op inside `txn`. Returns the id of an inserted cell.
pub(crate) fn apply(txn: &mut TransactionMut, roots: &Roots, tracker: &Mutex<Tracker>, op: LocalOp) -> Result<Option<String>> {
    match op {
        LocalOp::InsertCell { index, cell } => {
            let len = roots.cells.len(txn) as usize;
            if index > len {
                return Err(invalid(format!("insert index {index} > {len} cells")));
            }
            let id = match cell.id.as_deref() {
                Some(id) if !id.is_empty() && doc::find_cell(txn, roots, id, hint(tracker, id)).is_none() => id.to_owned(),
                _ => crate::util::new_uuid(),
            };
            doc::insert_cell(txn, roots, index as u32, &cell, &id);
            return Ok(Some(id));
        }
        LocalOp::DeleteCell { id } => {
            let (index, _) = find(txn, roots, tracker, &id)?;
            roots.cells.remove(txn, index);
            if doc::find_cell(txn, roots, &id, None).is_none() {
                doc::clear_cell_records(txn, roots, &id);
            }
        }
        LocalOp::MoveCell { id, to } => {
            let (from, _) = find(txn, roots, tracker, &id)?;
            let len = roots.cells.len(txn);
            if to as u32 >= len {
                return Err(invalid(format!("move target {to} >= {len} cells")));
            }
            if from != to as u32 {
                doc::move_cell(txn, roots, from, to as u32);
            }
        }
        LocalOp::UpdateCell { cell } => {
            let id = cell.id.clone().ok_or_else(|| invalid("UpdateCell needs a cell with an id"))?;
            let (_, map) = find(txn, roots, tracker, &id)?;
            doc::update_cell(txn, roots, &map, &id, &cell);
        }
        LocalOp::EditSource { id, range, text } => {
            let (_, map) = find(txn, roots, tracker, &id)?;
            let ytext = doc::ensure_source_text(txn, &map);
            doc::edit_text(txn, &ytext, range, &text)?;
        }
        LocalOp::SetSource { id, source } => {
            let (_, map) = find(txn, roots, tracker, &id)?;
            doc::set_source(txn, &map, &source);
        }
        LocalOp::SetCellType { id, cell_type } => {
            let (_, map) = find(txn, roots, tracker, &id)?;
            doc::set_cell_type(txn, roots, &map, &id, cell_type);
        }
        LocalOp::SetCellMetadata { id, metadata } => {
            let (_, map) = find(txn, roots, tracker, &id)?;
            doc::set_cell_metadata(txn, roots, &map, &id, &metadata);
        }
        LocalOp::SetAttachments { id, attachments } => {
            let (_, map) = find(txn, roots, tracker, &id)?;
            doc::set_attachments(txn, roots, &map, &id, attachments.as_ref());
        }
        LocalOp::SetOutputs { id, outputs } => {
            let (_, map) = code_cell(txn, roots, tracker, &id)?;
            doc::set_outputs(txn, roots, &map, &id, &outputs);
        }
        LocalOp::PushOutput { id, output } => {
            let (_, map) = code_cell(txn, roots, tracker, &id)?;
            doc::push_output(txn, roots, &map, &id, &output);
        }
        LocalOp::ReplaceOutput { id, index, output } => {
            let (_, map) = code_cell(txn, roots, tracker, &id)?;
            doc::replace_output(txn, roots, &map, &id, index, &output)?;
        }
        LocalOp::AppendStreamText { id, index, text } => {
            let (_, map) = code_cell(txn, roots, tracker, &id)?;
            doc::append_stream_text(txn, &map, index, &text)?;
        }
        LocalOp::SetExecutionCount { id, execution_count } => {
            let (_, map) = code_cell(txn, roots, tracker, &id)?;
            doc::set_execution_count(txn, &map, execution_count);
        }
        LocalOp::SetExecutionState { id, state } => {
            let (_, map) = code_cell(txn, roots, tracker, &id)?;
            doc::set_execution_state(txn, &map, &state);
        }
        LocalOp::SetNotebookMetadata { metadata } => doc::set_notebook_metadata(txn, roots, &metadata),
    }
    Ok(None)
}

fn code_cell(txn: &TransactionMut, roots: &Roots, tracker: &Mutex<Tracker>, id: &str) -> Result<(u32, yrs::MapRef)> {
    let (i, map) = find(txn, roots, tracker, id)?;
    if doc::cell_type_of(txn, &map) != CellType::Code {
        return Err(invalid(format!("cell {id:?} is not a code cell")));
    }
    Ok((i, map))
}

/// Make the doc equal `nb` with the fewest changes: cells matched by id,
/// removed / moved / inserted as needed, matched cells updated field by
/// field, then notebook metadata, nbformat and unknown keys.
pub(crate) fn sync_notebook(txn: &mut TransactionMut, roots: &Roots, nb: &Notebook) -> Vec<AssignedId> {
    let (ids, assigned) = doc::assign_cell_ids(nb);
    let current: Vec<String> = changes::read_shadow(txn, roots).into_iter().map(|(id, _)| id).collect();
    // Duplicate ids in the doc (briefly possible after concurrent moves):
    // treat later copies as foreign so they get removed.
    let mut seen = std::collections::HashSet::new();
    let old_keys: Vec<(String, bool)> = current.into_iter().map(|id| { let first = seen.insert(id.clone()); (id, first) }).collect();
    let new_keys: Vec<(String, bool)> = ids.iter().map(|id| (id.clone(), true)).collect();
    let fresh = list_inserts(&old_keys, &new_keys);
    for op in changes::list_diff(&old_keys, &new_keys) {
        match op {
            ListOp::Remove { index, key } => {
                roots.cells.remove(txn, index as u32);
                if key.1 && !ids.contains(&key.0) {
                    doc::clear_cell_records(txn, roots, &key.0);
                }
            }
            ListOp::Move { from, to, .. } => doc::move_cell(txn, roots, from as u32, to as u32),
            ListOp::Insert { index, key } => {
                doc::insert_cell(txn, roots, index as u32, &nb.cells[index], &key.0);
            }
        }
    }
    for (i, (cell, id)) in nb.cells.iter().zip(&ids).enumerate() {
        if fresh.contains(&i) {
            continue;
        }
        if let Some(Out::YMap(map)) = roots.cells.get(txn, i as u32) {
            doc::update_cell(txn, roots, &map, id, cell);
        }
    }
    doc::set_nbformat(txn, roots, nb.nbformat, nb.nbformat_minor);
    if doc::read_notebook_metadata(txn, roots) != nb.metadata {
        doc::set_notebook_metadata(txn, roots, &nb.metadata);
    }
    if doc::read_notebook_extra(txn, roots) != nb.extra {
        doc::set_notebook_extra(txn, roots, &nb.extra);
    }
    assigned
}

/// Indices in `new` that are not in `old` (cells inserted fresh).
fn list_inserts<K: Eq + std::hash::Hash>(old: &[K], new: &[K]) -> std::collections::HashSet<usize> {
    let old: std::collections::HashSet<&K> = old.iter().collect();
    new.iter().enumerate().filter(|(_, k)| !old.contains(k)).map(|(i, _)| i).collect()
}
