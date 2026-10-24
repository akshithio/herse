//! The writer: a JSON emitter that reproduces Python's `json.dumps` output
//! (and other writers' formatting, via [`FormatHint`]).

use serde_json::{Number, Value};

use crate::format::{FormatHint, Indent, KeyOrder, LineSplit, NumberSpellings, ObjKind, RawStrings, Shape, Shapes, WriteOptions};
use crate::model::{Cell, CellType, Map, MimeBundle, MimeData, Notebook, Output, is_json_mime, is_split_mime};
use crate::text::{content_hash, split_lines, write_i64, write_json_str, write_py_float, write_u64};

/// Serialize with the notebook's own [`FormatHint`] (byte-identical to the
/// source file for untouched notebooks written by nbformat, and for most
/// written by other tools).
pub fn to_string(nb: &Notebook) -> String {
    to_string_with(nb, &WriteOptions::preserve())
}

/// Serialize with explicit options; [`WriteOptions::jupyter`] gives exactly
/// what `nbformat.write` would produce.
pub fn to_string_with(nb: &Notebook, opts: &WriteOptions) -> String {
    let fmt = opts.format.as_ref().unwrap_or(&nb.format);
    let nl = if fmt.crlf { "\r\n" } else { "\n" };
    let unit_len = match fmt.indent {
        Indent::Compact => 0,
        Indent::Spaces(n) | Indent::Tabs(n) => n as usize,
    };
    let mut w = Writer {
        out: String::with_capacity(estimate_size(nb)),
        fmt,
        indent_buf: nl.to_string(),
        unit_len,
        nl_len: nl.len(),
        compact: fmt.indent == Indent::Compact,
        nl,
        comma_space: fmt.comma_space,
        colon_space: fmt.colon_space,
        depth: 0,
        // Key order templates only make sense with the hint they came from.
        key_order: if fmt.sort_keys { None } else { nb.format.key_order.as_deref().or(fmt.key_order.as_deref()) },
        numbers: nb.format.numbers.as_deref(),
        keep_spellings: opts.keep_number_spellings,
        strip: opts.strip_transient,
        split: fmt.split_lines,
        newline_split: fmt.line_split == LineSplit::Newline,
        shapes: fmt.shapes.as_deref(),
        raw_strings: fmt.strings.as_deref(),
    };
    if fmt.bom {
        w.out.push('\u{feff}');
    }
    w.notebook(nb);
    if fmt.trailing_newline {
        w.out.push_str(w.nl);
    }
    w.out
}

/// Output size estimate, so the buffer rarely regrows: line arrays cost
/// indentation + quotes + comma per line (~1/4 extra for typical text), base64
/// and other single strings almost nothing.
fn estimate_size(nb: &Notebook) -> usize {
    let lines = |t: &str| t.len() + t.len() / 4 + 16;
    let mut n = 1024;
    for c in &nb.cells {
        n += lines(&c.source) + 160;
        for o in &c.outputs {
            n += 96;
            match o {
                Output::Stream { text, .. } => n += lines(text),
                Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
                    for (mime, d) in &data.0 {
                        n += match d {
                            MimeData::Text(t) if is_split_mime(mime) => lines(t),
                            MimeData::Text(t) => t.len() + 32,
                            MimeData::Json(_) => 256,
                        };
                    }
                }
                Output::Error { traceback, .. } => n += traceback.iter().map(|t| t.len() + t.len() / 8 + 16).sum::<usize>(),
                Output::Unknown(_) => n += 256,
            }
        }
    }
    n
}

struct Writer<'a> {
    out: String,
    fmt: &'a FormatHint,
    /// `nl` followed by enough indentation for any depth seen so far.
    indent_buf: String,
    unit_len: usize,
    nl_len: usize,
    compact: bool,
    nl: &'static str,
    comma_space: bool,
    colon_space: bool,
    depth: usize,
    key_order: Option<&'a KeyOrder>,
    numbers: Option<&'a NumberSpellings>,
    keep_spellings: bool,
    strip: bool,
    split: bool,
    newline_split: bool,
    shapes: Option<&'a Shapes>,
    raw_strings: Option<&'a RawStrings>,
}

/// Values the writer knows how to emit as object members.
#[derive(Clone, Copy)]
enum Item<'a> {
    Value(&'a Value),
    Str(&'a str),
    /// Multiline text: a line array (or one string when the hint says so).
    Lines(&'a str),
    /// Text nbformat writes as one string (base64 images), unless the file had it split.
    TextStr(&'a str),
    OptInt(Option<i64>),
    U32(u32),
    Meta(&'a Map, &'static [&'static str]),
    Cells(&'a [Cell]),
    Outputs(&'a [Output]),
    Bundle(&'a MimeBundle),
    StrList(&'a [String]),
    Attachments(&'a Value),
    /// An attachment's mime bundle (text mimes split like output bundles).
    AttachmentBundle(&'a Map),
    /// The `data` of an unparsed output, normalized like nbformat would.
    RawBundle(&'a Map),
}

impl Item<'_> {
    /// Whether a typed cell field still holds its empty default.
    fn is_empty(&self) -> bool {
        match self {
            Item::OptInt(n) => n.is_none(),
            Item::Outputs(o) => o.is_empty(),
            Item::Meta(m, _) => m.is_empty(),
            Item::Lines(s) | Item::Str(s) => s.is_empty(),
            _ => false,
        }
    }
}

/// Object members collected for reordering; inline up to 8, then spills.
struct Members<'b> {
    inline: [(&'b str, Item<'b>); 8],
    len: usize,
    spill: Vec<(&'b str, Item<'b>)>,
}

impl<'b> Members<'b> {
    #[inline]
    fn new() -> Self {
        Members { inline: [("", Item::U32(0)); 8], len: 0, spill: Vec::new() }
    }

    #[inline]
    fn push(&mut self, key: &'b str, item: Item<'b>) {
        if self.spill.is_empty() && self.len < 8 {
            self.inline[self.len] = (key, item);
            self.len += 1;
        } else {
            if self.spill.is_empty() {
                self.spill.extend_from_slice(&self.inline[..self.len]);
            }
            self.spill.push((key, item));
        }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [(&'b str, Item<'b>)] {
        if self.spill.is_empty() { &mut self.inline[..self.len] } else { &mut self.spill }
    }
}

const NB_TRANSIENT: &[&str] = &["orig_nbformat", "orig_nbformat_minor", "signature"];
const CELL_TRANSIENT: &[&str] = &["trusted"];

impl<'a> Writer<'a> {
    #[inline]
    fn newline(&mut self) {
        if self.compact {
            return;
        }
        let need = self.nl_len + self.depth * self.unit_len;
        // Keep 32 bytes of slack so short indents are one fixed-size copy.
        if need + 32 > self.indent_buf.len() {
            self.grow_indent(need + 32);
        }
        if need <= 32 {
            let v = unsafe { self.out.as_mut_vec() };
            v.reserve(32);
            // SAFETY: `indent_buf` has >= need + 32 bytes; `v` has 32 bytes of
            // spare capacity; only the first `need` (ASCII) bytes are kept.
            unsafe {
                std::ptr::copy_nonoverlapping(self.indent_buf.as_ptr(), v.as_mut_ptr().add(v.len()), 32);
                v.set_len(v.len() + need);
            }
        } else {
            self.out.push_str(&self.indent_buf[..need]);
        }
    }

    #[inline]
    fn item_sep(&mut self) {
        self.out.push(',');
        if self.comma_space {
            self.out.push(' ');
        }
    }

    #[inline]
    fn key_sep(&mut self) {
        self.out.push(':');
        if self.colon_space {
            self.out.push(' ');
        }
    }

    #[cold]
    fn grow_indent(&mut self, need: usize) {
        let unit = match self.fmt.indent {
            Indent::Tabs(_) => "\t",
            _ => " ",
        };
        while self.indent_buf.len() < need {
            self.indent_buf.push_str(unit);
        }
    }

    #[inline]
    fn str(&mut self, s: &str) {
        if let Some(raw) = self.raw_strings
            && let Some(text) = raw.map.get(&(content_hash(s), s.len()))
        {
            self.out.push_str(text);
            return;
        }
        write_json_str(&mut self.out, s, self.fmt.ensure_ascii);
    }

    /// Emit an object from `(key, item)` members, sorted or in template order.
    fn object(&mut self, members: &mut [(&str, Item<'_>)], kind: Option<ObjKind>) {
        let order = match (self.key_order, kind) {
            (Some(ko), Some(kind)) => ko.template(kind),
            _ => None,
        };
        self.object_ordered(members, order);
    }

    fn object_ordered(&mut self, members: &mut [(&str, Item<'_>)], order: Option<&[Box<str>]>) {
        if self.fmt.sort_keys {
            if !members.is_sorted_by(|a, b| a.0 <= b.0) {
                members.sort_by(|a, b| a.0.cmp(b.0));
            }
        } else if let Some(order) = order {
            // Stable: keys missing from the template keep their relative order at the end.
            members.sort_by_key(|m| order.iter().position(|k| **k == *m.0).unwrap_or(usize::MAX));
        }
        self.emit(members.iter().copied());
    }

    /// Write `{ members }` in the given order.
    #[inline]
    fn emit<'b>(&mut self, members: impl Iterator<Item = (&'b str, Item<'b>)>) {
        let mut first = true;
        for (k, item) in members {
            if first {
                self.out.push('{');
                self.depth += 1;
                first = false;
            } else {
                self.item_sep();
            }
            self.newline();
            self.str(k);
            self.key_sep();
            self.item(&item);
        }
        if first {
            self.out.push_str("{}");
        } else {
            self.depth -= 1;
            self.newline();
            self.out.push('}');
        }
    }

    fn item(&mut self, item: &Item<'_>) {
        match *item {
            Item::Value(v) => self.value(v),
            Item::Str(s) => self.str(s),
            Item::Lines(s) => self.lines(s),
            Item::TextStr(s) => self.text_str(s),
            Item::OptInt(Some(n)) => write_i64(&mut self.out, n),
            Item::OptInt(None) => self.out.push_str("null"),
            Item::U32(n) => write_u64(&mut self.out, n as u64),
            Item::Meta(m, skip) => self.map(m, if self.strip { skip } else { &[] }),
            Item::Cells(cells) => self.array(cells.len(), |w, i| w.cell(&cells[i], i)),
            Item::Outputs(outs) => self.array(outs.len(), |w, i| w.output(&outs[i])),
            Item::Bundle(b) => self.bundle(b),
            Item::StrList(list) => self.array(list.len(), |w, i| w.str(&list[i])),
            Item::Attachments(v) => self.attachments(v),
            Item::AttachmentBundle(m) => self.attachment_bundle(m),
            Item::RawBundle(m) => self.raw_bundle(m),
        }
    }

    fn array(&mut self, len: usize, mut each: impl FnMut(&mut Self, usize)) {
        if len == 0 {
            self.out.push_str("[]");
            return;
        }
        self.out.push('[');
        self.depth += 1;
        for i in 0..len {
            if i > 0 {
                self.item_sep();
            }
            self.newline();
            each(self, i);
        }
        self.depth -= 1;
        self.newline();
        self.out.push(']');
    }

    #[inline]
    fn shape(&self, s: &str) -> Option<&'a Shape> {
        let shapes = self.shapes?;
        shapes.map.get(&(content_hash(s), s.len()))
    }

    fn lines(&mut self, s: &str) {
        let split = match self.shape(s) {
            None => self.split,
            Some(Shape::String) => false,
            Some(Shape::Lines) => true,
            Some(Shape::Chunks(c)) => {
                if self.chunks(s, c) {
                    return;
                }
                self.split
            }
        };
        if !split {
            self.str(s);
        } else if self.newline_split {
            self.line_array(s.split_inclusive('\n'));
        } else {
            self.line_array(split_lines(s));
        }
    }

    fn text_str(&mut self, s: &str) {
        if let Some(Shape::Chunks(c)) = self.shape(s)
            && self.chunks(s, c)
        {
            return;
        }
        self.str(s);
    }

    /// Write `s` cut at the recorded chunk lengths; false if they don't fit.
    fn chunks(&mut self, s: &str, lens: &[u32]) -> bool {
        let mut at = 0usize;
        for &l in lens {
            at += l as usize;
            if at > s.len() || !s.is_char_boundary(at) {
                return false;
            }
        }
        if at != s.len() {
            return false;
        }
        let mut at = 0usize;
        self.line_array(lens.iter().map(|&l| {
            let piece = &s[at..at + l as usize];
            at += l as usize;
            piece
        }));
        true
    }

    fn line_array<'s>(&mut self, mut lines: impl Iterator<Item = &'s str>) {
        let Some(first) = lines.next() else {
            self.out.push_str("[]");
            return;
        };
        self.out.push('[');
        self.depth += 1;
        self.newline();
        self.str(first);
        for line in lines {
            self.item_sep();
            self.newline();
            self.str(line);
        }
        self.depth -= 1;
        self.newline();
        self.out.push(']');
    }

    fn number(&mut self, n: &Number) {
        if let Some(u) = n.as_u64() {
            write_u64(&mut self.out, u);
        } else if let Some(i) = n.as_i64() {
            write_i64(&mut self.out, i);
        } else {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if let Some(nums) = self.numbers
                && let Some((text, big_int)) = nums.map.get(&f.to_bits())
                && (*big_int || self.keep_spellings)
            {
                self.out.push_str(text);
                return;
            }
            write_py_float(&mut self.out, f);
        }
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.out.push_str("null"),
            Value::Bool(true) => self.out.push_str("true"),
            Value::Bool(false) => self.out.push_str("false"),
            Value::Number(n) => self.number(n),
            Value::String(s) => self.str(s),
            Value::Array(a) => self.array(a.len(), |w, i| w.value(&a[i])),
            Value::Object(m) => self.map(m, &[]),
        }
    }

    fn map(&mut self, m: &Map, skip: &[&str]) {
        let keep = |k: &String| skip.is_empty() || !skip.contains(&k.as_str());
        if !self.fmt.sort_keys || m.keys().is_sorted() {
            self.emit(m.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.as_str(), Item::Value(v))));
            return;
        }
        let mut members: Vec<(&str, Item)> =
            m.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.as_str(), Item::Value(v))).collect();
        members.sort_by(|a, b| a.0.cmp(b.0));
        self.emit(members.into_iter());
    }

    fn notebook(&mut self, nb: &Notebook) {
        let mut m = Members::new();
        m.push("cells", Item::Cells(&nb.cells));
        m.push("metadata", Item::Meta(&nb.metadata, NB_TRANSIENT));
        m.push("nbformat", Item::U32(nb.nbformat));
        m.push("nbformat_minor", Item::U32(nb.nbformat_minor));
        for (k, v) in &nb.extra {
            if !matches!(k.as_str(), "cells" | "metadata" | "nbformat" | "nbformat_minor") {
                m.push(k, Item::Value(v));
            }
        }
        self.object(m.as_mut_slice(), Some(ObjKind::Notebook));
    }

    fn cell(&mut self, c: &Cell, index: usize) {
        let mut m = Members::new();
        if let Some(a) = &c.attachments {
            m.push("attachments", Item::Attachments(a));
        }
        m.push("cell_type", Item::Str(c.cell_type.as_str()));
        if c.cell_type == CellType::Code {
            m.push("execution_count", Item::OptInt(c.execution_count));
        }
        if let Some(id) = &c.id {
            m.push("id", Item::Str(id));
        }
        m.push("metadata", Item::Meta(&c.metadata, CELL_TRANSIENT));
        if c.cell_type == CellType::Code {
            m.push("outputs", Item::Outputs(&c.outputs));
        }
        m.push("source", Item::Lines(&c.source));
        let typed = m.len;
        for (k, v) in &c.extra {
            // A key the typed fields also write: the raw value wins while the
            // typed field is still empty (an ill-typed value kept verbatim, e.g.
            // `"metadata": null`), the typed one once it has content (e.g.
            // outputs of a markdown cell that became a code cell and ran).
            match m.inline[..typed].iter_mut().find(|(t, _)| *t == k) {
                Some(slot) => {
                    if slot.1.is_empty() {
                        slot.1 = Item::Value(v);
                    }
                }
                None => m.push(k, Item::Value(v)),
            }
        }
        let kind = match c.cell_type {
            CellType::Code => ObjKind::CodeCell,
            CellType::Markdown => ObjKind::MarkdownCell,
            CellType::Raw => ObjKind::RawCell,
        };
        match self.key_order.filter(|_| !self.fmt.sort_keys) {
            Some(ko) => {
                let order = ko.cell(c.id.as_deref(), index).or_else(|| ko.template(kind));
                self.object_ordered(m.as_mut_slice(), order);
            }
            None => self.object(m.as_mut_slice(), Some(kind)),
        }
    }

    fn output(&mut self, o: &Output) {
        match o {
            Output::Stream { name, text } => {
                let mut m = [
                    ("name", Item::Str(name.as_str())),
                    ("output_type", Item::Str("stream")),
                    ("text", Item::Lines(text)),
                ];
                self.object(&mut m, Some(ObjKind::Stream));
            }
            Output::DisplayData { data, metadata, transient: _ } => {
                let mut m = [
                    ("data", Item::Bundle(data)),
                    ("metadata", Item::Meta(metadata, &[])),
                    ("output_type", Item::Str("display_data")),
                ];
                self.object(&mut m, Some(ObjKind::DisplayData));
            }
            Output::ExecuteResult { execution_count, data, metadata } => {
                let mut m = [
                    ("data", Item::Bundle(data)),
                    ("execution_count", Item::OptInt(*execution_count)),
                    ("metadata", Item::Meta(metadata, &[])),
                    ("output_type", Item::Str("execute_result")),
                ];
                self.object(&mut m, Some(ObjKind::ExecuteResult));
            }
            Output::Error { ename, evalue, traceback } => {
                let mut m = [
                    ("ename", Item::Str(ename)),
                    ("evalue", Item::Str(evalue)),
                    ("output_type", Item::Str("error")),
                    ("traceback", Item::StrList(traceback)),
                ];
                self.object(&mut m, Some(ObjKind::Error));
            }
            Output::Unknown(map) => self.raw_output(map),
        }
    }

    fn raw_output(&mut self, map: &Map) {
        if !self.strip {
            self.map(map, &[]);
            return;
        }
        // Canonical mode: what nbformat's read (`rejoin_lines`) + write
        // (`split_lines`) does to this object: stream text and display mime
        // text are joined then split; other outputs' list-valued `text` is joined.
        let ot = map.get("output_type").and_then(Value::as_str).unwrap_or("");
        let joined_text: Option<String> = match map.get("text") {
            Some(Value::Array(a)) if a.iter().all(Value::is_string) => Some(a.iter().filter_map(Value::as_str).collect()),
            _ => None,
        };
        let mut members: Vec<(&str, Item)> = Vec::with_capacity(map.len());
        for (k, v) in map {
            let item = match (k.as_str(), v) {
                ("text", Value::String(t)) if ot == "stream" => Item::Lines(t),
                ("text", Value::Array(_)) if joined_text.is_some() => match (ot, &joined_text) {
                    ("stream", Some(t)) => Item::Lines(t),
                    (_, Some(t)) => Item::Str(t),
                    _ => unreachable!(),
                },
                ("data", Value::Object(d)) if ot == "display_data" || ot == "execute_result" => Item::RawBundle(d),
                _ => Item::Value(v),
            };
            members.push((k.as_str(), item));
        }
        self.object(&mut members, None);
    }

    /// A raw (unparsed) mime bundle, normalized like nbformat's rejoin + split.
    fn raw_bundle(&mut self, d: &Map) {
        let mut joined: Vec<Option<String>> = Vec::with_capacity(d.len());
        for (mime, v) in d {
            joined.push(match v {
                Value::Array(a) if !is_json_mime(mime) && a.iter().all(Value::is_string) => {
                    Some(a.iter().filter_map(Value::as_str).collect())
                }
                _ => None,
            });
        }
        let mut members: Vec<(&str, Item)> = Vec::with_capacity(d.len());
        for ((mime, v), j) in d.iter().zip(&joined) {
            let text = match (j, v) {
                (Some(t), _) => Some(t.as_str()),
                (None, Value::String(t)) => Some(t.as_str()),
                _ => None,
            };
            let item = match text {
                Some(t) if is_split_mime(mime) => Item::Lines(t),
                Some(t) => Item::Str(t),
                None => Item::Value(v),
            };
            members.push((mime.as_str(), item));
        }
        self.object(&mut members, None);
    }

    #[inline]
    fn mime_item<'b>(mime: &'b str, d: &'b MimeData) -> (&'b str, Item<'b>) {
        let item = match d {
            MimeData::Text(t) if is_split_mime(mime) => Item::Lines(t),
            MimeData::Text(t) => Item::TextStr(t),
            MimeData::Json(v) => Item::Value(v),
        };
        (mime, item)
    }

    fn bundle(&mut self, b: &MimeBundle) {
        if !self.fmt.sort_keys || b.0.is_sorted_by(|x, y| x.0 <= y.0) {
            self.emit(b.0.iter().map(|(mime, d)| Self::mime_item(mime, d)));
            return;
        }
        let mut members: Vec<(&str, Item)> = b.0.iter().map(|(mime, d)| Self::mime_item(mime, d)).collect();
        members.sort_by(|x, y| x.0.cmp(y.0));
        self.emit(members.into_iter());
    }

    fn attachments(&mut self, v: &Value) {
        let Value::Object(atts) = v else {
            self.value(v);
            return;
        };
        fn item(v: &Value) -> Item<'_> {
            match v {
                Value::Object(bundle) => Item::AttachmentBundle(bundle),
                other => Item::Value(other),
            }
        }
        if !self.fmt.sort_keys || atts.keys().is_sorted() {
            self.emit(atts.iter().map(|(k, v)| (k.as_str(), item(v))));
            return;
        }
        let mut members: Vec<(&str, Item)> = atts.iter().map(|(k, v)| (k.as_str(), item(v))).collect();
        members.sort_by(|a, b| a.0.cmp(b.0));
        self.emit(members.into_iter());
    }

    fn attachment_bundle(&mut self, bundle: &Map) {
        fn item<'b>(mime: &str, d: &'b Value) -> Item<'b> {
            match d {
                Value::String(s) if is_split_mime(mime) && !is_json_mime(mime) => Item::Lines(s),
                other => Item::Value(other),
            }
        }
        if !self.fmt.sort_keys || bundle.keys().is_sorted() {
            self.emit(bundle.iter().map(|(k, v)| (k.as_str(), item(k, v))));
            return;
        }
        let mut members: Vec<(&str, Item)> = bundle.iter().map(|(k, v)| (k.as_str(), item(k, v))).collect();
        members.sort_by(|a, b| a.0.cmp(b.0));
        self.emit(members.into_iter());
    }
}
