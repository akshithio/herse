//! Remote changes as typed, incrementally applicable events.
//!
//! Yrs observers on the doc's roots turn every transaction that did not
//! originate locally into [`DocChange`]s: structural changes (cells
//! inserted, removed, moved) come from diffing the cell id list before and
//! after the transaction; content changes come from the observers' deltas
//! (text edits as byte ranges, outputs, metadata, ...).
//!
//! A batch of changes applies **in order**: every index refers to the cell
//! list as it is after the previous changes of the batch, and every
//! [`TextEdit`] range to the source as it is after the previous edits.
//! [`DocChange::apply_to`] is the reference implementation (and what the
//! tests use to check that replaying events reproduces the document).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use herse_nb::{Cell, Map, Notebook, Output, Value};
use yrs::types::{Delta, Event, Events, PathSegment};
use yrs::{Any, Array, BranchID, DeepObservable, Origin, Out, ReadTxn, TransactionMut};

use crate::comments::{self, Thread};
use crate::doc::{self, Roots};

/// One replacement in a cell source: bytes `range` (UTF-8 offsets into the
/// text as it is after the previous edits of the same change) become `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub text: String,
}

/// Apply edits in order. Returns false (leaving `s` partially edited) if an
/// edit does not fit `s`, which means the caller's copy had diverged and
/// should be replaced by [`crate::Session::snapshot`].
pub fn apply_text_edits(s: &mut String, edits: &[TextEdit]) -> bool {
    for e in edits {
        if e.range.start > e.range.end
            || e.range.end > s.len()
            || !s.is_char_boundary(e.range.start)
            || !s.is_char_boundary(e.range.end)
        {
            return false;
        }
        s.replace_range(e.range.clone(), &e.text);
    }
    true
}

/// A change made by another peer. Every cell-level change carries the
/// cell's doc `id` (also for nbformat < 4.5 notebooks, whose `Cell::id` is
/// `None`); `index` is the cell's position at the moment the change applies
/// (see module docs).
#[derive(Clone, Debug, PartialEq)]
pub enum DocChange {
    /// A new cell at `index`.
    CellInserted { index: usize, id: String, cell: Cell },
    /// The cell at `index` was deleted.
    CellRemoved { index: usize, id: String },
    /// The cell at `from` moved to `to` (remove at `from`, then insert at
    /// `to`). Moves are delete + re-insert in Yjs, so concurrent edits to the
    /// moved cell can be lost: replace your copy's content with `cell`.
    CellMoved { from: usize, to: usize, id: String, cell: Cell },
    /// Several things about the cell changed at once (type change, unknown
    /// keys, the cell was re-created): replace it with `cell`.
    CellReplaced { index: usize, id: String, cell: Cell },
    /// Source text edits, to apply in order.
    SourceEdited { index: usize, id: String, edits: Vec<TextEdit> },
    /// The cell's outputs list changed.
    OutputsChanged { index: usize, id: String, outputs: Vec<Output> },
    /// One output changed in place (e.g. a display updated).
    OutputChanged { index: usize, id: String, output_index: usize, output: Output },
    /// The text of stream output `output_index` was edited (usually an
    /// append), without resending the whole stream.
    OutputTextEdited { index: usize, id: String, output_index: usize, edits: Vec<TextEdit> },
    ExecutionCountChanged { index: usize, id: String, execution_count: Option<i64> },
    /// Runtime state of a code cell (`"idle"`, `"running"`, ...); not part of
    /// the notebook file.
    ExecutionStateChanged { index: usize, id: String, state: String },
    CellMetadataChanged { index: usize, id: String, metadata: Map },
    AttachmentsChanged { index: usize, id: String, attachments: Option<Value> },
    /// Notebook metadata (including `herse.comments`, so it is ready to save).
    NotebookMetadataChanged { metadata: Map },
    NbformatChanged { nbformat: u32, nbformat_minor: u32 },
    /// Unknown top-level notebook keys.
    NotebookExtraChanged { extra: Map },
    /// The comment threads changed (local or remote); the full current list.
    CommentsChanged { threads: Vec<Thread> },
    /// The change could not be described incrementally (updates arrived out
    /// of causal order, which the star topology over TCP does not produce, but
    /// other Yjs peers might): replace the model with `notebook` (keep your
    /// own format hint). `cell_ids` are the ids of its cells, in order.
    Reset { notebook: Notebook, cell_ids: Vec<String> },
}

impl DocChange {
    /// Apply to a notebook model. Returns false if the change does not fit
    /// (index out of range, text edit out of bounds): the model has diverged
    /// and should be replaced by a fresh snapshot.
    pub fn apply_to(&self, nb: &mut Notebook) -> bool {
        let len = nb.cells.len();
        match self {
            DocChange::CellInserted { index, cell, .. } => {
                if *index > len {
                    return false;
                }
                nb.cells.insert(*index, cell.clone());
            }
            DocChange::CellRemoved { index, .. } => {
                if *index >= len {
                    return false;
                }
                nb.cells.remove(*index);
            }
            DocChange::CellMoved { from, to, cell, .. } => {
                if *from >= len || *to >= len {
                    return false;
                }
                nb.cells.remove(*from);
                nb.cells.insert(*to, cell.clone());
            }
            DocChange::CellReplaced { index, cell, .. } => match nb.cells.get_mut(*index) {
                Some(c) => *c = cell.clone(),
                None => return false,
            },
            DocChange::SourceEdited { index, edits, .. } => match nb.cells.get_mut(*index) {
                Some(c) => return apply_text_edits(&mut c.source, edits),
                None => return false,
            },
            DocChange::OutputsChanged { index, outputs, .. } => match nb.cells.get_mut(*index) {
                Some(c) => c.outputs = outputs.clone(),
                None => return false,
            },
            DocChange::OutputChanged { index, output_index, output, .. } => {
                match nb.cells.get_mut(*index).and_then(|c| c.outputs.get_mut(*output_index)) {
                    Some(o) => *o = output.clone(),
                    None => return false,
                }
            }
            DocChange::OutputTextEdited { index, output_index, edits, .. } => {
                match nb.cells.get_mut(*index).and_then(|c| c.outputs.get_mut(*output_index)) {
                    Some(Output::Stream { text, .. }) => return apply_text_edits(text, edits),
                    _ => return false,
                }
            }
            DocChange::ExecutionCountChanged { index, execution_count, .. } => match nb.cells.get_mut(*index) {
                Some(c) => c.execution_count = *execution_count,
                None => return false,
            },
            DocChange::ExecutionStateChanged { index, .. } => return *index < len,
            DocChange::CellMetadataChanged { index, metadata, .. } => match nb.cells.get_mut(*index) {
                Some(c) => c.metadata = metadata.clone(),
                None => return false,
            },
            DocChange::AttachmentsChanged { index, attachments, .. } => match nb.cells.get_mut(*index) {
                Some(c) => c.attachments = attachments.clone(),
                None => return false,
            },
            DocChange::NotebookMetadataChanged { metadata } => nb.metadata = metadata.clone(),
            DocChange::NbformatChanged { nbformat, nbformat_minor } => {
                nb.nbformat = *nbformat;
                nb.nbformat_minor = *nbformat_minor;
            }
            DocChange::NotebookExtraChanged { extra } => nb.extra = extra.clone(),
            DocChange::CommentsChanged { threads } => comments::store(nb, threads),
            DocChange::Reset { notebook, .. } => {
                let format = std::mem::take(&mut nb.format);
                *nb = notebook.clone();
                nb.format = format;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Transaction origins

/// Edits made through `Session::apply_local` (the app already has them).
pub(crate) const ORIGIN_LOCAL: &str = "herse:local";
/// Building the doc / a guest's initial sync (the app gets a snapshot).
pub(crate) const ORIGIN_INIT: &str = "herse:init";
/// Updates from peers.
pub(crate) const ORIGIN_REMOTE: &str = "herse:remote";
/// The host's id-deduplication after concurrent moves.
pub(crate) const ORIGIN_REPAIR: &str = "herse:repair";

const OBSERVER_KEY: &str = "herse-collab";

fn emits(txn: &TransactionMut) -> bool {
    match txn.origin() {
        Some(o) => *o == Origin::from(ORIGIN_REMOTE) || *o == Origin::from(ORIGIN_REPAIR),
        // Transactions without an origin only come from yrs internals (e.g.
        // applying an update without our wrappers): treat as remote.
        None => true,
    }
}

/// Cell key in diffs: id plus occurrence number, unique even with
/// duplicate ids (which exist briefly after concurrent moves).
type Key = (String, u32);

fn keyed(ids: impl IntoIterator<Item = String>) -> Vec<Key> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    ids.into_iter()
        .map(|id| {
            let n = seen.entry(id.clone()).or_default();
            let k = (id, *n);
            *n += 1;
            k
        })
        .collect()
}

/// What the observers know and have produced since the last drain.
#[derive(Default)]
pub(crate) struct Tracker {
    /// Cell id and Y.Map identity per index, as of the last transaction.
    pub shadow: Vec<(String, BranchID)>,
    /// First index of each id in `shadow`.
    pub index: HashMap<String, usize>,
    /// Changes not yet handed to the app.
    pub pending: Vec<DocChange>,
    /// Comments changed (any origin) since the last drain.
    pub comments_dirty: bool,
    /// Set when the cell structure changed (the host checks for duplicate ids).
    pub structure_changed: bool,
    /// Cells whose shape may need repair by the host (type changes that raced
    /// with output updates).
    pub check_cells: Vec<String>,
}

impl Tracker {
    fn set_shadow(&mut self, shadow: Vec<(String, BranchID)>) {
        self.index.clear();
        for (i, (id, _)) in shadow.iter().enumerate() {
            self.index.entry(id.clone()).or_insert(i);
        }
        self.shadow = shadow;
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }
}

pub(crate) fn read_shadow<T: ReadTxn>(txn: &T, roots: &Roots) -> Vec<(String, BranchID)> {
    roots
        .cells
        .iter(txn)
        .filter_map(|out| match out {
            Out::YMap(m) => Some((doc::cell_id(txn, &m), m.as_ref().id())),
            _ => None,
        })
        .collect()
}

/// Rebuild the shadow from the doc (after construction / initial sync).
pub(crate) fn reset_shadow<T: ReadTxn>(tracker: &Mutex<Tracker>, txn: &T, roots: &Roots) {
    let shadow = read_shadow(txn, roots);
    lock(tracker).set_shadow(shadow);
}

fn lock(t: &Mutex<Tracker>) -> std::sync::MutexGuard<'_, Tracker> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

/// Register the observers that feed `tracker`.
pub(crate) fn observe(roots: &Roots, tracker: Arc<Mutex<Tracker>>) {
    {
        let tracker = tracker.clone();
        let r = roots.clone();
        roots.cells.observe_deep(OBSERVER_KEY, move |txn, events| on_cells(&tracker, &r, txn, events));
    }
    {
        let tracker = tracker.clone();
        let r = roots.clone();
        roots.meta.observe_deep(OBSERVER_KEY, move |txn, events| {
            if !emits(txn) {
                return;
            }
            let mut format = false;
            let mut metadata = false;
            for e in events.iter() {
                let path = e.path();
                match path.front() {
                    None => {
                        if let Event::Map(m) = e {
                            for key in m.keys(txn).keys() {
                                match &**key {
                                    "nbformat" | "nbformat_minor" => format = true,
                                    "metadata" => metadata = true,
                                    _ => {}
                                }
                            }
                        }
                    }
                    Some(PathSegment::Key(k)) if &**k == "metadata" => metadata = true,
                    _ => {}
                }
            }
            let mut t = lock(&tracker);
            if format {
                let (nbformat, nbformat_minor) = doc::read_nbformat(txn, &r);
                t.pending.push(DocChange::NbformatChanged { nbformat, nbformat_minor });
            }
            if metadata {
                t.pending.push(DocChange::NotebookMetadataChanged { metadata: doc::read_notebook_metadata(txn, &r) });
            }
        });
    }
    {
        let tracker = tracker.clone();
        let r = roots.clone();
        roots.herse.observe_deep(OBSERVER_KEY, move |txn, events| {
            if !emits(txn) {
                return;
            }
            let extra_changed = events
                .iter()
                .any(|e| matches!(e, Event::Map(m) if m.keys(txn).keys().any(|k| &**k == "extra")));
            if extra_changed {
                let extra = doc::read_notebook_extra(txn, &r);
                lock(&tracker).pending.push(DocChange::NotebookExtraChanged { extra });
            }
        });
    }
    roots.comments.observe_deep(OBSERVER_KEY, move |_txn, _events| {
        lock(&tracker).comments_dirty = true;
    });
}

/// What changed in one cell during a transaction.
#[derive(Default)]
struct CellDirty {
    replace: bool,
    edits: Option<Vec<TextEdit>>,
    metadata: bool,
    execution_count: bool,
    execution_state: bool,
    outputs: bool,
    output: BTreeSet<usize>,
    output_text: BTreeMap<usize, Vec<TextEdit>>,
    attachments: bool,
}

fn on_cells(tracker: &Mutex<Tracker>, roots: &Roots, txn: &TransactionMut, events: &Events) {
    let emit = emits(txn);
    let mut structural = false;
    let mut dirty: BTreeMap<usize, CellDirty> = BTreeMap::new();
    for e in events.iter() {
        let path = e.path();
        let mut segs = path.iter();
        let Some(first) = segs.next() else {
            structural = true;
            continue;
        };
        // A cell whose id changed (the host's duplicate repair) changes the
        // id list: handled by the structural diff.
        if path.len() == 1
            && let Event::Map(m) = e
            && m.keys(txn).contains_key("id")
        {
            structural = true;
        }
        if !emit {
            continue;
        }
        let PathSegment::Index(i) = first else { continue };
        let d = dirty.entry(*i as usize).or_default();
        match (segs.next(), segs.next()) {
            (None, _) => {
                if let Event::Map(m) = e {
                    for key in m.keys(txn).keys() {
                        match &**key {
                            "metadata" => d.metadata = true,
                            "execution_count" => d.execution_count = true,
                            "execution_state" => d.execution_state = true,
                            "outputs" => d.outputs = true,
                            "attachments" => d.attachments = true,
                            // id, cell_type, a replaced source text, unknown keys
                            _ => d.replace = true,
                        }
                    }
                } else {
                    d.replace = true;
                }
            }
            (Some(PathSegment::Key(k)), next) => match (&**k, next) {
                ("source", None) => match e {
                    Event::Text(t) => {
                        let edits = delta_to_edits(t.delta(txn));
                        d.edits.get_or_insert_with(Vec::new).extend(edits);
                    }
                    _ => d.replace = true,
                },
                ("metadata", _) => d.metadata = true,
                ("outputs", None) => d.outputs = true,
                ("outputs", Some(PathSegment::Index(k))) => {
                    let k = *k as usize;
                    match (segs.next(), segs.next(), e) {
                        (Some(PathSegment::Key(t)), None, Event::Text(te)) if &**t == "text" => {
                            d.output_text.entry(k).or_default().extend(delta_to_edits(te.delta(txn)));
                        }
                        _ => {
                            d.output.insert(k);
                        }
                    }
                }
                ("attachments", _) => d.attachments = true,
                _ => d.replace = true,
            },
            _ => d.replace = true,
        }
    }

    let mut t = lock(tracker);
    let new_shadow = if structural { Some(read_shadow(txn, roots)) } else { None };
    if !emit {
        if let Some(s) = new_shadow {
            t.set_shadow(s);
            t.structure_changed = true;
        }
        return;
    }

    let (nbformat, minor) = doc::read_nbformat(txn, roots);
    let strip = nbformat == 4 && minor <= 4;
    let cell_at = |index: usize| -> Option<(Cell, String)> {
        match roots.cells.get(txn, index as u32)? {
            Out::YMap(m) => Some(doc::read_cell(txn, roots, &m, strip)),
            _ => None,
        }
    };

    // Indices (in the new list) whose content is already sent in full.
    let mut full: HashSet<usize> = HashSet::new();
    let mut out: Vec<DocChange> = Vec::new();
    if let Some(new_shadow) = new_shadow {
        let old_keys = keyed(t.shadow.iter().map(|(id, _)| id.clone()));
        let new_keys = keyed(new_shadow.iter().map(|(id, _)| id.clone()));
        let new_pos: HashMap<&Key, usize> = new_keys.iter().enumerate().map(|(i, k)| (k, i)).collect();
        for op in list_diff(&old_keys, &new_keys) {
            match op {
                ListOp::Remove { index, key } => out.push(DocChange::CellRemoved { index, id: key.0 }),
                ListOp::Move { from, to, key } => {
                    let at = new_pos[&key];
                    full.insert(at);
                    if let Some((cell, id)) = cell_at(at) {
                        out.push(DocChange::CellMoved { from, to, id, cell });
                    }
                }
                ListOp::Insert { index, key } => {
                    let at = new_pos[&key];
                    full.insert(at);
                    if let Some((cell, id)) = cell_at(at) {
                        out.push(DocChange::CellInserted { index, id, cell });
                    }
                }
            }
        }
        // Same id, same place, new Y.Map: the cell was re-created.
        let old_branch: HashMap<&Key, &BranchID> =
            old_keys.iter().zip(t.shadow.iter()).map(|(k, (_, b))| (k, b)).collect();
        for (i, (k, (_, branch))) in new_keys.iter().zip(new_shadow.iter()).enumerate() {
            if !full.contains(&i)
                && old_branch.get(k).is_some_and(|b| *b != branch)
                && let Some((cell, id)) = cell_at(i)
            {
                full.insert(i);
                out.push(DocChange::CellReplaced { index: i, id, cell });
            }
        }
        t.set_shadow(new_shadow);
        t.structure_changed = true;
    }

    for (index, d) in dirty {
        if full.contains(&index) {
            continue;
        }
        let Some(Out::YMap(map)) = roots.cells.get(txn, index as u32) else { continue };
        let id = doc::cell_id(txn, &map);
        let code_keys = d.execution_count || d.outputs || !d.output.is_empty() || !d.output_text.is_empty();
        if code_keys && doc::cell_type_of(txn, &map) != herse_nb::CellType::Code {
            // Code-cell keys on a non-code cell (a type change raced with
            // output updates): they read as unknown keys. Send the whole cell,
            // and let the host clean it up.
            t.check_cells.push(id.clone());
            let (cell, id) = doc::read_cell(txn, roots, &map, strip);
            out.push(DocChange::CellReplaced { index, id, cell });
            continue;
        }
        if d.replace {
            t.check_cells.push(id.clone());
            let (cell, id) = doc::read_cell(txn, roots, &map, strip);
            out.push(DocChange::CellReplaced { index, id, cell });
            continue;
        }
        if let Some(edits) = d.edits.filter(|e| !e.is_empty()) {
            out.push(DocChange::SourceEdited { index, id: id.clone(), edits });
        }
        if d.metadata {
            let metadata = doc::read_cell_metadata(txn, roots, &map, &id);
            out.push(DocChange::CellMetadataChanged { index, id: id.clone(), metadata });
        }
        if d.attachments {
            let attachments = doc::read_attachments(txn, roots, &map, &id);
            out.push(DocChange::AttachmentsChanged { index, id: id.clone(), attachments });
        }
        if d.execution_count {
            let execution_count = doc::read_execution_count(txn, &map);
            out.push(DocChange::ExecutionCountChanged { index, id: id.clone(), execution_count });
        }
        if d.outputs {
            let outputs = doc::read_outputs(txn, roots, &map, &id);
            out.push(DocChange::OutputsChanged { index, id: id.clone(), outputs });
        } else {
            for (output_index, edits) in d.output_text {
                if !d.output.contains(&output_index) && !edits.is_empty() {
                    out.push(DocChange::OutputTextEdited { index, id: id.clone(), output_index, edits });
                }
            }
            for output_index in d.output {
                if let Some(output) = doc::read_output(txn, roots, &map, &id, output_index) {
                    out.push(DocChange::OutputChanged { index, id: id.clone(), output_index, output });
                }
            }
        }
        if d.execution_state
            && let Some(state) = doc::read_execution_state(txn, &map)
        {
            out.push(DocChange::ExecutionStateChanged { index, id: id.clone(), state });
        }
    }
    t.pending.extend(out);
}

/// Text delta (byte units) -> sequential edits.
fn delta_to_edits(delta: &[Delta]) -> Vec<TextEdit> {
    let mut pos = 0usize;
    let mut edits: Vec<TextEdit> = Vec::new();
    for d in delta {
        match d {
            Delta::Retain(n, _) => pos += *n as usize,
            Delta::Deleted(n) => {
                let n = *n as usize;
                edits.push(TextEdit { range: pos..pos + n, text: String::new() });
            }
            Delta::Inserted(value, _) => {
                let s = match value {
                    Out::Any(Any::String(s)) => s.to_string(),
                    // Embeds are not used in cell sources.
                    _ => continue,
                };
                // A delete right before at the same position becomes a replace.
                match edits.last_mut() {
                    Some(last) if last.range.start == pos && last.text.is_empty() => last.text = s.clone(),
                    _ => edits.push(TextEdit { range: pos..pos, text: s.clone() }),
                }
                pos += s.len();
            }
        }
    }
    edits
}

/// One step of turning one list of unique keys into another.
#[derive(Debug, PartialEq)]
pub(crate) enum ListOp<K> {
    Remove { index: usize, key: K },
    /// Remove at `from`, then insert at `to` (index after the removal).
    Move { from: usize, to: usize, key: K },
    Insert { index: usize, key: K },
}

/// Sequential edit script from `old` to `new` (keys unique in each list):
/// removals, then the fewest moves (everything outside a longest increasing
/// subsequence moves), then insertions.
pub(crate) fn list_diff<K: Clone + Eq + std::hash::Hash>(old: &[K], new: &[K]) -> Vec<ListOp<K>> {
    let new_set: HashSet<&K> = new.iter().collect();
    let old_set: HashSet<&K> = old.iter().collect();
    let mut ops = Vec::new();
    let mut cur: Vec<K> = old.to_vec();
    // Removals, from the back so indices stay simple.
    for i in (0..cur.len()).rev() {
        if !new_set.contains(&cur[i]) {
            let key = cur.remove(i);
            ops.push(ListOp::Remove { index: i, key });
        }
    }
    // Moves among the kept keys.
    let target: Vec<K> = new.iter().filter(|k| old_set.contains(k)).cloned().collect();
    let target_pos: HashMap<&K, usize> = target.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let seq: Vec<usize> = cur.iter().map(|k| target_pos[k]).collect();
    let stay: HashSet<usize> = longest_increasing(&seq).into_iter().collect();
    for (t, key) in target.iter().enumerate() {
        if stay.contains(&t) {
            continue;
        }
        let from = cur.iter().position(|k| k == key).unwrap_or(0);
        let k = cur.remove(from);
        let to = if t == 0 { 0 } else { cur.iter().position(|k| *k == target[t - 1]).map_or(0, |p| p + 1) };
        cur.insert(to, k);
        ops.push(ListOp::Move { from, to, key: key.clone() });
    }
    // Insertions in target order.
    for (i, key) in new.iter().enumerate() {
        if !old_set.contains(key) {
            cur.insert(i, key.clone());
            ops.push(ListOp::Insert { index: i, key: key.clone() });
        }
    }
    debug_assert!(cur == new);
    ops
}

/// Values of one longest strictly increasing subsequence of `seq`.
fn longest_increasing(seq: &[usize]) -> Vec<usize> {
    // Patience sorting with back-pointers.
    let mut tails: Vec<usize> = Vec::new(); // indices into seq
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let pos = tails.partition_point(|&j| seq[j] < v);
        prev[i] = pos.checked_sub(1).map(|p| tails[p]);
        if pos == tails.len() {
            tails.push(i);
        } else {
            tails[pos] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        out.push(seq[i]);
        cur = prev[i];
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &[char], ops: &[ListOp<char>]) -> Vec<char> {
        let mut v = old.to_vec();
        for op in ops {
            match op {
                ListOp::Remove { index, key } => assert_eq!(v.remove(*index), *key),
                ListOp::Move { from, to, key } => {
                    assert_eq!(v.remove(*from), *key);
                    v.insert(*to, *key);
                }
                ListOp::Insert { index, key } => v.insert(*index, *key),
            }
        }
        v
    }

    #[test]
    fn list_diff_cases() {
        let cases: &[(&str, &str, usize)] = &[
            ("abcd", "abcd", 0),
            ("abcd", "bcda", 1),
            ("abcd", "dabc", 1),
            ("abcd", "acbd", 1),
            ("xyab", "abyx", 2),
            ("abcd", "xbdy", 4),
            ("", "abc", 3),
            ("abc", "", 3),
            ("abcdef", "fedcba", 5),
        ];
        for (old, new, n) in cases {
            let old: Vec<char> = old.chars().collect();
            let new: Vec<char> = new.chars().collect();
            let ops = list_diff(&old, &new);
            assert_eq!(apply(&old, &ops), new, "{old:?} -> {new:?}: {ops:?}");
            assert_eq!(ops.len(), *n, "{old:?} -> {new:?}: {ops:?}");
        }
    }

    #[test]
    fn list_diff_random() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..500 {
            let old: Vec<u32> = (0..rnd(12) as u32).collect();
            let mut new: Vec<u32> = old.iter().copied().filter(|_| rnd(4) != 0).collect();
            for i in 0..new.len() {
                let j = rnd(new.len());
                new.swap(i, j);
            }
            for k in 0..rnd(4) {
                let at = rnd(new.len() + 1);
                new.insert(at, 100 + k as u32);
            }
            let ops = list_diff(&old, &new);
            let mut v = old.clone();
            for op in &ops {
                match op {
                    ListOp::Remove { index, .. } => {
                        v.remove(*index);
                    }
                    ListOp::Move { from, to, .. } => {
                        let x = v.remove(*from);
                        v.insert(*to, x);
                    }
                    ListOp::Insert { index, key } => v.insert(*index, *key),
                }
            }
            assert_eq!(v, new);
        }
    }

    #[test]
    fn deltas_become_sequential_edits() {
        let d = vec![
            Delta::Retain(2, None),
            Delta::Deleted(3),
            Delta::Inserted(Out::Any(Any::String("é".into())), None),
            Delta::Retain(1, None),
            Delta::Inserted(Out::Any(Any::String("!".into())), None),
        ];
        let edits = delta_to_edits(&d);
        let mut s = String::from("abcdefg");
        assert!(apply_text_edits(&mut s, &edits));
        assert_eq!(s, "abéf!g");
    }
}
