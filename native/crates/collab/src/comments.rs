//! Comments: threads anchored to cells.
//!
//! The model works with or without a live session. Offline, load threads
//! from a notebook with [`load`], change them with the [`Thread`] methods,
//! and write them back with [`store`]. In a session, use the `Session`
//! comment methods; threads live in the shared document's `herse:comments`
//! root (one Y.Map per thread, one entry per message), so two peers adding
//! comments or replies at the same time both keep theirs, and
//! `Session::snapshot` writes them back into the notebook metadata.
//!
//! # File format
//!
//! Threads are stored in the notebook metadata under `herse.comments`
//! (namespaced like `metadata.jupyter` / `metadata.vscode`):
//!
//! ```json
//! "metadata": {
//!   "herse": {
//!     "comments": {
//!       "version": 1,
//!       "threads": [
//!         {
//!           "id": "0b6f2a7e-6c8e-4f0e-9a51-1f3c2d4e5f60",
//!           "cell_id": "a1b2c3d4",
//!           "anchor": { "start": 4, "end": 18, "quote": "df.groupby(\"k\")" },
//!           "resolved": false,
//!           "messages": [
//!             {
//!               "id": "5d1c...",
//!               "author": "Ada",
//!               "body": "Why group here?",
//!               "created_at": "2026-10-16T11:58:03.123Z"
//!             },
//!             {
//!               "id": "9e7a...",
//!               "author": "Grace",
//!               "body": "To get per-key totals.",
//!               "created_at": "2026-10-16T12:01:44.000Z",
//!               "edited_at": "2026-10-16T12:02:10.000Z"
//!             }
//!           ]
//!         }
//!       ]
//!     }
//!   }
//! }
//! ```
//!
//! * `id` — thread id (UUID). Unique within the notebook.
//! * `cell_id` — the nbformat 4.5 id of the cell the thread is about (so
//!   comments need cell ids; upgrade older notebooks with
//!   `Notebook::upgrade_to_4_5`). Threads whose cell was deleted are kept;
//!   the app decides whether to show them as orphaned.
//! * `anchor` — optional: the commented span of the cell source as UTF-8
//!   byte offsets `start..end` at the time of commenting, plus the quoted
//!   text, so the app can re-find the span after edits (search for `quote`
//!   near `start`).
//! * `resolved` — whether the thread is resolved; `resolved_by` (display
//!   name) and `resolved_at` (timestamp) are present when it is.
//! * `messages` — oldest first. `author` is a display name; timestamps are
//!   RFC 3339 UTC with milliseconds; `edited_at` only on edited messages.
//!
//! Threads and messages keep their order through [`load`] / [`store`]; in a
//! live session they are ordered oldest first (by first message time, then
//! id). Unknown
//! `version`s, or a malformed `herse.comments`, are left untouched in the
//! metadata and not shown as comments.

use std::sync::Arc;

use herse_nb::{Map, Notebook, Value};
use serde_json::json;
use yrs::{Any, Map as _, MapPrelim, MapRef, Number, Out, ReadTxn, TransactionMut};

use crate::util::{format_rfc3339, new_uuid, now_ms, parse_rfc3339};

/// Version of the `herse.comments` JSON schema.
pub const COMMENTS_VERSION: u64 = 1;

/// A comment thread about one cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    /// Id of the cell the thread is attached to.
    pub cell_id: String,
    /// The commented span of the cell's source, if the comment is about a
    /// selection rather than the whole cell.
    pub anchor: Option<Anchor>,
    pub resolved: bool,
    /// Who resolved it (display name), when resolved.
    pub resolved_by: Option<String>,
    /// When it was resolved (ms since the Unix epoch), when resolved.
    pub resolved_at: Option<i64>,
    /// Oldest first. Never empty for threads read from a document or file.
    pub messages: Vec<Comment>,
}

/// The source span a thread is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// UTF-8 byte offsets into the cell source when the thread was created.
    pub start: usize,
    pub end: usize,
    /// The text that was selected (`source[start..end]` back then).
    pub quote: String,
}

/// One message in a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    /// Display name of the author.
    pub author: String,
    pub body: String,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
    /// Milliseconds since the Unix epoch, if the body was edited.
    pub edited_at: Option<i64>,
}

impl Comment {
    /// A new message stamped now.
    pub fn new(author: &str, body: &str) -> Comment {
        Comment { id: new_uuid(), author: author.into(), body: body.into(), created_at: now_ms(), edited_at: None }
    }
}

impl Thread {
    /// A new thread with its first message.
    pub fn new(cell_id: &str, anchor: Option<Anchor>, author: &str, body: &str) -> Thread {
        Thread {
            id: new_uuid(),
            cell_id: cell_id.into(),
            anchor,
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            messages: vec![Comment::new(author, body)],
        }
    }

    /// When the thread started (its first message), ms since the epoch.
    pub fn created_at(&self) -> i64 {
        self.messages.first().map_or(0, |m| m.created_at)
    }

    /// Add a reply; returns its id.
    pub fn reply(&mut self, author: &str, body: &str) -> String {
        let c = Comment::new(author, body);
        let id = c.id.clone();
        self.messages.push(c);
        id
    }

    /// Resolve (or reopen) the thread.
    pub fn set_resolved(&mut self, resolved: bool, by: &str) {
        self.resolved = resolved;
        if resolved {
            self.resolved_by = Some(by.into());
            self.resolved_at = Some(now_ms());
        } else {
            self.resolved_by = None;
            self.resolved_at = None;
        }
    }

    /// Change a message's body. Returns false if there is no such message.
    pub fn edit(&mut self, message_id: &str, body: &str) -> bool {
        match self.messages.iter_mut().find(|m| m.id == message_id) {
            Some(m) => {
                m.body = body.into();
                m.edited_at = Some(now_ms());
                true
            }
            None => false,
        }
    }

    /// Remove a message. Returns false if there is no such message. A thread
    /// left without messages should be dropped by the caller.
    pub fn delete(&mut self, message_id: &str) -> bool {
        let before = self.messages.len();
        self.messages.retain(|m| m.id != message_id);
        self.messages.len() != before
    }
}

/// The comment threads stored in a notebook (empty if there are none or
/// `herse.comments` is not something this version understands).
pub fn load(nb: &Notebook) -> Vec<Thread> {
    threads_from_metadata(&nb.metadata).unwrap_or_default()
}

/// Store `threads` in the notebook metadata (`herse.comments`), replacing
/// what was there. No threads removes the key (and an empty `herse` object).
pub fn store(nb: &mut Notebook, threads: &[Thread]) {
    if threads.is_empty() {
        if let Some(Value::Object(herse)) = nb.metadata.get_mut("herse") {
            herse.shift_remove("comments");
            if herse.is_empty() {
                nb.metadata.shift_remove("herse");
            }
        }
    } else {
        threads_into_metadata(&mut nb.metadata, threads);
    }
}

/// Parse `metadata.herse.comments`. `None` when absent or not understood.
pub(crate) fn threads_from_metadata(metadata: &Map) -> Option<Vec<Thread>> {
    let comments = metadata.get("herse")?.get("comments")?.as_object()?;
    if comments.get("version")?.as_u64()? != COMMENTS_VERSION {
        return None;
    }
    // The file's order is authoritative (only the doc, whose maps are
    // unordered, needs sorting).
    comments.get("threads")?.as_array()?.iter().map(thread_from_json).collect()
}

/// Write threads under `metadata.herse.comments` (keeping other `herse` keys).
pub(crate) fn threads_into_metadata(metadata: &mut Map, threads: &[Thread]) {
    let value = json!({
        "version": COMMENTS_VERSION,
        "threads": threads.iter().map(thread_to_json).collect::<Vec<_>>(),
    });
    match metadata.get_mut("herse") {
        Some(Value::Object(herse)) => {
            herse.insert("comments".into(), value);
        }
        _ => {
            let mut herse = Map::new();
            herse.insert("comments".into(), value);
            metadata.insert("herse".into(), Value::Object(herse));
        }
    }
}

fn sorted(mut threads: Vec<Thread>) -> Vec<Thread> {
    for t in &mut threads {
        t.messages.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    }
    threads.sort_by(|a, b| a.created_at().cmp(&b.created_at()).then_with(|| a.id.cmp(&b.id)));
    threads
}

fn thread_to_json(t: &Thread) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), t.id.clone().into());
    m.insert("cell_id".into(), t.cell_id.clone().into());
    if let Some(a) = &t.anchor {
        m.insert("anchor".into(), json!({"start": a.start, "end": a.end, "quote": a.quote}));
    }
    m.insert("resolved".into(), t.resolved.into());
    if let Some(by) = &t.resolved_by {
        m.insert("resolved_by".into(), by.clone().into());
    }
    if let Some(at) = t.resolved_at {
        m.insert("resolved_at".into(), format_rfc3339(at).into());
    }
    m.insert("messages".into(), Value::Array(t.messages.iter().map(comment_to_json).collect()));
    Value::Object(m)
}

fn comment_to_json(c: &Comment) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), c.id.clone().into());
    m.insert("author".into(), c.author.clone().into());
    m.insert("body".into(), c.body.clone().into());
    m.insert("created_at".into(), format_rfc3339(c.created_at).into());
    if let Some(at) = c.edited_at {
        m.insert("edited_at".into(), format_rfc3339(at).into());
    }
    Value::Object(m)
}

/// A timestamp field: RFC 3339 string (what we write) or integer ms.
fn time_from_json(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => parse_rfc3339(s),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn opt_time(m: &Map, key: &str) -> Option<Option<i64>> {
    match m.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => time_from_json(v).map(Some),
    }
}

fn opt_str(m: &Map, key: &str) -> Option<Option<String>> {
    match m.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => None,
    }
}

fn anchor_from_json(v: &Value) -> Option<Option<Anchor>> {
    match v {
        Value::Null => Some(None),
        Value::Object(a) => {
            let start = a.get("start")?.as_u64()? as usize;
            let end = a.get("end")?.as_u64()? as usize;
            let quote = a.get("quote").and_then(Value::as_str).unwrap_or_default().to_owned();
            (start <= end).then_some(Some(Anchor { start, end, quote }))
        }
        _ => None,
    }
}

fn thread_from_json(v: &Value) -> Option<Thread> {
    let m = v.as_object()?;
    Some(Thread {
        id: m.get("id")?.as_str()?.to_owned(),
        cell_id: m.get("cell_id")?.as_str()?.to_owned(),
        anchor: match m.get("anchor") {
            None => None,
            Some(a) => anchor_from_json(a)?,
        },
        resolved: m.get("resolved").map_or(Some(false), Value::as_bool)?,
        resolved_by: opt_str(m, "resolved_by")?,
        resolved_at: opt_time(m, "resolved_at")?,
        messages: m.get("messages")?.as_array()?.iter().map(comment_from_json).collect::<Option<Vec<_>>>()?,
    })
}

fn comment_from_json(v: &Value) -> Option<Comment> {
    let m = v.as_object()?;
    Some(Comment {
        id: m.get("id")?.as_str()?.to_owned(),
        author: m.get("author")?.as_str()?.to_owned(),
        body: m.get("body")?.as_str()?.to_owned(),
        created_at: time_from_json(m.get("created_at")?)?,
        edited_at: opt_time(m, "edited_at")?,
    })
}

// ---------------------------------------------------------------------------
// In the shared document: root Y.Map thread id -> Y.Map {
//   cell_id, anchor?: {start, end, quote}, resolved, resolved_by?, resolved_at?,
//   messages: Y.Map message id -> {author, body, created_at, edited_at?}
// } (timestamps as integer ms).

fn s(v: &str) -> Any {
    Any::String(Arc::from(v))
}

fn int(v: i64) -> Any {
    Any::Number(Number::Int(v))
}

fn comment_any(c: &Comment) -> Any {
    let mut m = std::collections::HashMap::new();
    m.insert("author".to_owned(), s(&c.author));
    m.insert("body".to_owned(), s(&c.body));
    m.insert("created_at".to_owned(), int(c.created_at));
    if let Some(at) = c.edited_at {
        m.insert("edited_at".to_owned(), int(at));
    }
    Any::Map(Arc::new(m))
}

fn anchor_any(a: &Anchor) -> Any {
    let mut m = std::collections::HashMap::new();
    m.insert("start".to_owned(), int(a.start as i64));
    m.insert("end".to_owned(), int(a.end as i64));
    m.insert("quote".to_owned(), s(&a.quote));
    Any::Map(Arc::new(m))
}

pub(crate) fn write_thread(txn: &mut TransactionMut, root: &MapRef, t: &Thread) {
    let messages: MapPrelim = t.messages.iter().map(|c| (c.id.as_str(), comment_any(c))).collect();
    let mut entries: Vec<(&str, yrs::In)> = vec![
        ("cell_id", s(&t.cell_id).into()),
        ("resolved", Any::Bool(t.resolved).into()),
        ("messages", messages.into()),
    ];
    if let Some(a) = &t.anchor {
        entries.push(("anchor", anchor_any(a).into()));
    }
    if let Some(by) = &t.resolved_by {
        entries.push(("resolved_by", s(by).into()));
    }
    if let Some(at) = t.resolved_at {
        entries.push(("resolved_at", int(at).into()));
    }
    root.insert(txn, t.id.as_str(), MapPrelim::from_iter(entries));
}

fn thread_map<T: ReadTxn>(txn: &T, root: &MapRef, id: &str) -> Option<MapRef> {
    match root.get(txn, id)? {
        Out::YMap(m) => Some(m),
        _ => None,
    }
}

fn messages_map<T: ReadTxn>(txn: &T, thread: &MapRef) -> Option<MapRef> {
    match thread.get(txn, "messages")? {
        Out::YMap(m) => Some(m),
        _ => None,
    }
}

fn any_str(a: &Any) -> Option<String> {
    match a {
        Any::String(s) => Some(s.to_string()),
        _ => None,
    }
}

fn any_int(a: &Any) -> Option<i64> {
    match a {
        Any::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn get_any<T: ReadTxn>(txn: &T, m: &MapRef, key: &str) -> Option<Any> {
    match m.get(txn, key)? {
        Out::Any(a) => Some(a),
        _ => None,
    }
}

fn comment_from_any(id: &str, a: &Any) -> Option<Comment> {
    let Any::Map(m) = a else { return None };
    Some(Comment {
        id: id.to_owned(),
        author: m.get("author").and_then(any_str).unwrap_or_default(),
        body: m.get("body").and_then(any_str)?,
        created_at: m.get("created_at").and_then(any_int).unwrap_or(0),
        edited_at: m.get("edited_at").and_then(any_int),
    })
}

fn read_thread<T: ReadTxn>(txn: &T, id: &str, m: &MapRef) -> Option<Thread> {
    let messages = messages_map(txn, m)?
        .iter(txn)
        .filter_map(|(mid, out)| match out {
            Out::Any(a) => comment_from_any(mid, &a),
            _ => None,
        })
        .collect::<Vec<_>>();
    if messages.is_empty() {
        return None;
    }
    let anchor = match get_any(txn, m, "anchor") {
        Some(Any::Map(a)) => Some(Anchor {
            start: a.get("start").and_then(any_int).unwrap_or(0).max(0) as usize,
            end: a.get("end").and_then(any_int).unwrap_or(0).max(0) as usize,
            quote: a.get("quote").and_then(any_str).unwrap_or_default(),
        }),
        _ => None,
    };
    Some(Thread {
        id: id.to_owned(),
        cell_id: get_any(txn, m, "cell_id").as_ref().and_then(any_str)?,
        anchor,
        resolved: matches!(get_any(txn, m, "resolved"), Some(Any::Bool(true))),
        resolved_by: get_any(txn, m, "resolved_by").as_ref().and_then(any_str),
        resolved_at: get_any(txn, m, "resolved_at").as_ref().and_then(any_int),
        messages,
    })
}

/// All live threads, oldest first.
pub(crate) fn read_threads<T: ReadTxn>(txn: &T, root: &MapRef) -> Vec<Thread> {
    let threads = root
        .iter(txn)
        .filter_map(|(id, out)| match out {
            Out::YMap(m) => read_thread(txn, id, &m),
            _ => None,
        })
        .collect();
    sorted(threads)
}

pub(crate) fn add_message(txn: &mut TransactionMut, root: &MapRef, thread_id: &str, c: &Comment) -> bool {
    let Some(thread) = thread_map(txn, root, thread_id) else { return false };
    let Some(messages) = messages_map(txn, &thread) else { return false };
    messages.insert(txn, c.id.as_str(), comment_any(c));
    true
}

pub(crate) fn edit_message(txn: &mut TransactionMut, root: &MapRef, thread_id: &str, message_id: &str, body: &str) -> bool {
    let Some(messages) = thread_map(txn, root, thread_id).and_then(|t| messages_map(txn, &t)) else { return false };
    let Some(Out::Any(a)) = messages.get(txn, message_id) else { return false };
    let Some(mut c) = comment_from_any(message_id, &a) else { return false };
    c.body = body.into();
    c.edited_at = Some(now_ms());
    messages.insert(txn, message_id, comment_any(&c));
    true
}

/// Delete a message; the thread goes too when it was the last one.
pub(crate) fn delete_message(txn: &mut TransactionMut, root: &MapRef, thread_id: &str, message_id: &str) -> bool {
    let Some(thread) = thread_map(txn, root, thread_id) else { return false };
    let Some(messages) = messages_map(txn, &thread) else { return false };
    if messages.remove(txn, message_id).is_none() {
        return false;
    }
    if messages.len(txn) == 0 {
        root.remove(txn, thread_id);
    }
    true
}

pub(crate) fn set_resolved(txn: &mut TransactionMut, root: &MapRef, thread_id: &str, resolved: bool, by: &str) -> bool {
    let Some(thread) = thread_map(txn, root, thread_id) else { return false };
    thread.insert(txn, "resolved", Any::Bool(resolved));
    if resolved {
        thread.insert(txn, "resolved_by", s(by));
        thread.insert(txn, "resolved_at", int(now_ms()));
    } else {
        thread.remove(txn, "resolved_by");
        thread.remove(txn, "resolved_at");
    }
    true
}

pub(crate) fn delete_thread(txn: &mut TransactionMut, root: &MapRef, thread_id: &str) -> bool {
    root.remove(txn, thread_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb() -> Notebook {
        Notebook::new_empty("python3", "Python 3", "python")
    }

    #[test]
    fn json_roundtrip_through_metadata() {
        let mut nb = nb();
        let cell = nb.cells[0].id.clone().unwrap();
        let mut t = Thread::new(&cell, Some(Anchor { start: 0, end: 3, quote: "abc".into() }), "Ada", "Why?");
        t.reply("Grace", "Because ✓");
        let first = t.messages[0].id.clone();
        assert!(t.edit(&first, "Why though?"));
        t.set_resolved(true, "Grace");
        let t2 = Thread::new(&cell, None, "Linus", "nit");
        store(&mut nb, &[t.clone(), t2.clone()]);

        // Survives a full write + parse of the notebook file.
        let text = herse_nb::to_string(&nb);
        let parsed = herse_nb::parse(text.as_bytes()).unwrap();
        let back = load(&parsed);
        assert_eq!(back.len(), 2);
        let find = |id: &str| back.iter().find(|x| x.id == id).unwrap().clone();
        assert_eq!(find(&t.id), t);
        assert_eq!(find(&t2.id), t2);
        assert!(text.contains("\"herse\""), "{text}");

        // Removing all threads removes the key.
        let mut parsed = parsed;
        store(&mut parsed, &[]);
        assert!(!parsed.metadata.contains_key("herse"));
    }

    #[test]
    fn unknown_versions_are_left_alone() {
        let mut nb = nb();
        nb.metadata.insert("herse".into(), json!({"comments": {"version": 99, "threads": "?"}, "other": 1}));
        assert!(load(&nb).is_empty());
        assert!(threads_from_metadata(&nb.metadata).is_none());
    }
}
