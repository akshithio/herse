//! Reading notebooks: a single-pass, notebook-specialized JSON parser.
//!
//! The parser walks the bytes once, building the model directly (no
//! intermediate `Value` tree for cells and outputs), joining nbformat line
//! arrays straight into one `String`, and finding string ends with SIMD
//! `memchr2`. Strings without escapes (base64 images, most text) are copied
//! exactly once. While parsing it also records the formatting facts the
//! writer needs to reproduce the file ([`FormatHint`]): key order, number
//! spellings Python would not produce, line-array vs string style.
//!
//! Shapes the typed fast path does not expect (wrong types, unknown output
//! keys) are re-parsed generically from the object's start and converted by
//! the same rules `output_from_iopub` uses, so nothing is lost.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Number, Value};

use crate::convert::{self, rejoin_attachments};
use crate::error::{Error, Result};
use crate::format::{CellRef, FormatHint, Indent, KeyOrder, LineSplit, NumberSpellings, OBJ_KINDS, ObjKind, RawStrings, Shape, Shapes};
use crate::model::{Cell, CellType, Map, MimeBundle, MimeData, Notebook, Output, StreamName, is_json_mime, is_split_mime};
use crate::text::write_py_float;

const BOM: &[u8] = b"\xef\xbb\xbf";
const MAX_DEPTH: u32 = 512;

/// Parse `.ipynb` bytes (nbformat 4.x; 3.x is upgraded to 4 on read).
pub fn parse(bytes: &[u8]) -> Result<Notebook> {
    let (body, bom) = match bytes.strip_prefix(BOM) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    let text = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(e) => return Err(Error::json_at(body, e.valid_up_to(), "invalid UTF-8")),
    };
    let mut p = Parser::new(text);
    let mut nb = p.notebook()?;
    if p.upgraded {
        return Ok(nb);
    }
    let mut fmt = detect_whitespace(body);
    fmt.bom = bom;
    fmt.sort_keys = p.sorted;
    p.finish_shapes(&mut fmt);
    fmt.ensure_ascii = p.non_ascii_escape && p.file_is_ascii();
    if !p.odd_strings.is_empty() {
        fmt.strings = Some(Arc::new(RawStrings { map: std::mem::take(&mut p.odd_strings) }));
    }
    if !p.sorted {
        fmt.key_order = Some(Arc::new(KeyOrder {
            kinds: std::mem::take(&mut p.templates),
            cells: std::mem::take(&mut p.cell_orders),
        }));
    }
    if !p.numbers.is_empty() {
        fmt.numbers = Some(Arc::new(NumberSpellings { map: std::mem::take(&mut p.numbers) }));
    }
    nb.format = fmt;
    Ok(nb)
}

/// Parse arbitrary JSON into a `serde_json::Value` with this crate's parser
/// (same number handling and leniency as notebook parsing: `NaN`/`Infinity`
/// become `null`).
pub fn parse_value(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).map_err(|e| Error::json_at(bytes, e.valid_up_to(), "invalid UTF-8"))?;
    let mut p = Parser::new(text);
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.pos != p.b.len() {
        return Err(p.err("trailing characters after JSON value"));
    }
    Ok(v)
}

pub(crate) struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    depth: u32,
    /// Every object's keys were strictly increasing.
    pub(crate) sorted: bool,
    pub(crate) templates: [Option<Vec<Box<str>>>; OBJ_KINDS],
    template_sigs: [u64; OBJ_KINDS],
    pub(crate) cell_orders: HashMap<CellRef, Vec<Box<str>>>,
    pub(crate) numbers: HashMap<u64, (Box<str>, bool)>,
    /// Multiline fields seen as plain strings / as line arrays.
    pub(crate) ml_strings: usize,
    pub(crate) ml_arrays: usize,
    /// Line arrays whose chunking only Python's splitlines / only a `\n` split explains.
    py_only: usize,
    nl_only: usize,
    /// Shape candidates: plain strings in line-array fields; well-cut arrays
    /// seen after a plain string; arrays not cut by both rules.
    cand_plain: Vec<(u64, usize)>,
    cand_lines: Vec<(u64, usize)>,
    cand_arrays: Vec<ArrayCandidate>,
    chunks: Vec<u32>,
    pub(crate) non_ascii_escape: bool,
    /// The file was nbformat v3 and has been upgraded to v4.
    pub(crate) upgraded: bool,
    numbuf: String,
    scratch: String,
    /// Strings with escapes the writer would spell differently: raw source text by content.
    odd_strings: HashMap<(u64, usize), Box<str>>,
    ascii: Option<bool>,
    /// Key spans (byte ranges in the input) of the cell / output being parsed;
    /// `None` once a key needed unescaping (then keys are re-read if needed).
    cell_keys: Vec<(usize, usize)>,
    cell_keys_ok: bool,
    out_keys: Vec<(usize, usize)>,
    out_keys_ok: bool,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(s: &'a str) -> Parser<'a> {
        Parser {
            s,
            b: s.as_bytes(),
            pos: 0,
            depth: 0,
            sorted: true,
            templates: Default::default(),
            template_sigs: [0; OBJ_KINDS],
            cell_orders: HashMap::new(),
            numbers: HashMap::new(),
            ml_strings: 0,
            ml_arrays: 0,
            py_only: 0,
            nl_only: 0,
            cand_plain: Vec::new(),
            cand_lines: Vec::new(),
            cand_arrays: Vec::new(),
            chunks: Vec::new(),
            non_ascii_escape: false,
            upgraded: false,
            numbuf: String::new(),
            scratch: String::new(),
            odd_strings: HashMap::new(),
            ascii: None,
            cell_keys: Vec::new(),
            cell_keys_ok: true,
            out_keys: Vec::new(),
            out_keys_ok: true,
        }
    }

    #[cold]
    fn err(&self, msg: &str) -> Error {
        let found = match self.b.get(self.pos) {
            Some(&c) if c.is_ascii_graphic() => format!("{msg}, found `{}`", c as char),
            Some(_) => msg.to_string(),
            None => format!("{msg}, found end of input"),
        };
        Error::json_at(self.b, self.pos, found)
    }

    #[inline(always)]
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    #[inline(always)]
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.pos) {
            if c == b' ' || c == b'\n' || c == b'\r' || c == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    #[inline]
    fn expect(&mut self, c: u8, what: &str) -> Result<()> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(what))
        }
    }

    /// Object member iteration. Call with `pos` just after `{` and `first =
    /// true`; returns the next key with `pos` at its value, or `None` after
    /// consuming `}`. Tracks key sortedness.
    #[inline]
    fn next_key(&mut self, first: &mut bool, prev: &mut Option<Cow<'a, str>>) -> Result<Option<Cow<'a, str>>> {
        self.ws();
        if *first {
            *first = false;
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Ok(None);
            }
        } else {
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.ws();
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(None);
                }
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
        if self.peek() != Some(b'"') {
            return Err(self.err("expected a string key"));
        }
        let key = self.string()?;
        if let Some(p) = prev.as_deref() {
            // Most neighbouring keys differ in the first byte; skip memcmp then.
            let (a, b) = (p.as_bytes(), key.as_bytes());
            let out_of_order = match (a.first(), b.first()) {
                (Some(x), Some(y)) if x != y => x > y,
                _ => a >= b,
            };
            if out_of_order {
                self.sorted = false;
            }
        }
        *prev = Some(key.clone());
        self.ws();
        self.expect(b':', "expected `:`")?;
        self.ws();
        Ok(Some(key))
    }

    /// Array iteration. Call with `pos` just after `[`; returns `true` with
    /// `pos` at the next element, `false` after consuming `]`.
    #[inline]
    fn next_elem(&mut self, first: &mut bool) -> Result<bool> {
        self.ws();
        if *first {
            *first = false;
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(false);
            }
            return Ok(true);
        }
        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                self.ws();
                Ok(true)
            }
            Some(b']') => {
                self.pos += 1;
                Ok(false)
            }
            _ => Err(self.err("expected `,` or `]`")),
        }
    }

    // ---------------------------------------------------------------- strings

    /// Index of the first `"` or `\\` at or after `from`. Short strings
    /// (keys, source lines) are scanned inline 16 bytes at a time (NEON) or 8
    /// (SWAR elsewhere); long ones (base64 images) go to memchr's SIMD search.
    #[inline(always)]
    fn find_special(&self, from: usize) -> Option<usize> {
        let b = self.b;
        let inline_end = (from + 64).min(b.len());
        let mut i = from;
        #[cfg(target_arch = "aarch64")]
        {
            use std::arch::aarch64::*;
            // SAFETY: NEON is baseline on aarch64; each load reads b[i..i + 16] with i + 16 <= b.len().
            unsafe {
                let quote = vdupq_n_u8(b'"');
                let bslash = vdupq_n_u8(b'\\');
                while i + 16 <= inline_end {
                    let v = vld1q_u8(b.as_ptr().add(i));
                    let m = vorrq_u8(vceqq_u8(v, quote), vceqq_u8(v, bslash));
                    // 4 bits per byte: the first set nibble is the first match.
                    let nib = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m))));
                    if nib != 0 {
                        return Some(i + (nib.trailing_zeros() / 4) as usize);
                    }
                    i += 16;
                }
            }
        }
        const LO: u64 = 0x0101_0101_0101_0101;
        const HI: u64 = 0x8080_8080_8080_8080;
        while i + 8 <= inline_end {
            let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
            let q = w ^ (LO * b'"' as u64);
            let bs = w ^ (LO * b'\\' as u64);
            let m = (q.wrapping_sub(LO) & !q & HI) | (bs.wrapping_sub(LO) & !bs & HI);
            if m != 0 {
                return Some(i + (m.trailing_zeros() / 8) as usize);
            }
            i += 8;
        }
        if inline_end == b.len() {
            return b[i..].iter().position(|&c| c == b'"' || c == b'\\').map(|k| i + k);
        }
        while i < inline_end {
            if b[i] == b'"' || b[i] == b'\\' {
                return Some(i);
            }
            i += 1;
        }
        memchr::memchr2(b'"', b'\\', &b[i..]).map(|k| i + k)
    }

    /// String at `pos` (on the opening quote); borrowed when it has no escapes.
    #[inline]
    fn string(&mut self) -> Result<Cow<'a, str>> {
        let start = self.pos + 1;
        match self.find_special(start) {
            Some(i) if self.b[i] == b'"' => {
                self.pos = i + 1;
                Ok(Cow::Borrowed(&self.s[start..i]))
            }
            Some(_) => {
                let mut out = String::new();
                self.string_into(&mut out)?;
                Ok(Cow::Owned(out))
            }
            None => {
                self.pos = self.b.len();
                Err(self.err("unterminated string"))
            }
        }
    }

    #[inline]
    fn string_owned(&mut self) -> Result<String> {
        Ok(self.string()?.into_owned())
    }

    /// Append the decoded string at `pos` (on the opening quote) to `out`.
    #[inline]
    fn string_into(&mut self, out: &mut String) -> Result<()> {
        let quote = self.pos;
        let before = out.len();
        let mut odd = false;
        self.pos += 1;
        // SAFETY: only valid UTF-8 is appended: slices of the (validated)
        // input cut at ASCII bytes, ASCII escape results, and whole chars.
        let v = unsafe { out.as_mut_vec() };
        loop {
            let Some(at) = self.find_special(self.pos) else {
                self.pos = self.b.len();
                return Err(self.err("unterminated string"));
            };
            copy_segment(v, self.b, self.pos, at);
            if self.b[at] == b'"' {
                self.pos = at + 1;
                break;
            }
            self.pos = at + 2;
            match self.b.get(at + 1).copied() {
                Some(b'n') => v.push(b'\n'),
                Some(b'"') => v.push(b'"'),
                Some(b'\\') => v.push(b'\\'),
                Some(b't') => v.push(b'\t'),
                Some(b'r') => v.push(b'\r'),
                Some(b'/') => {
                    odd = true;
                    v.push(b'/');
                }
                Some(b'b') => v.push(0x08),
                Some(b'f') => v.push(0x0c),
                Some(b'u') => {
                    let (ch, canonical) = self.unicode_escape()?;
                    odd |= !canonical;
                    v.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => {
                    self.pos = at + 1;
                    return Err(self.err("invalid escape"));
                }
            }
            // Common case: the escape ends the string (`"...\n"`).
            if self.b.get(self.pos) == Some(&b'"') {
                self.pos += 1;
                break;
            }
        }
        if odd {
            self.note_odd_string(&out[before..], quote);
        }
        Ok(())
    }

    /// Remember the raw text of a string whose escapes our writer would spell
    /// differently (`\\/`, uppercase hex, `\\u0041`, `\\u00e9` in a UTF-8 file).
    #[cold]
    fn note_odd_string(&mut self, decoded: &str, quote: usize) {
        let raw = &self.s[quote..self.pos];
        self.odd_strings.insert((crate::text::content_hash(decoded), decoded.len()), raw.into());
    }

    /// Whether the whole input is ASCII (computed once, on first need).
    fn file_is_ascii(&mut self) -> bool {
        *self.ascii.get_or_insert_with(|| self.b.is_ascii())
    }

    /// The code point of a `\\uXXXX` escape (pos after the `u`), combining
    /// surrogate pairs, and whether the writer would spell it the same way.
    #[cold]
    fn unicode_escape(&mut self) -> Result<(char, bool)> {
        let lower = |h: &[u8]| !h.iter().any(|c| c.is_ascii_uppercase());
        let mut canonical = lower(&self.b[self.pos..(self.pos + 4).min(self.b.len())]);
        let cp = self.hex4()?;
        if cp >= 0x80 {
            self.non_ascii_escape = true;
        }
        if cp >= 0x7f {
            // Escaped non-ASCII is canonical only in an all-ASCII (ensure_ascii) file.
            canonical &= self.file_is_ascii();
        } else if cp >= 0x20 || matches!(cp, 0x08 | 0x09 | 0x0a | 0x0c | 0x0d) {
            // Printable ASCII and \\b \\t \\n \\f \\r are never written as \\u escapes.
            canonical = false;
        }
        let ch = if (0xd800..0xdc00).contains(&cp) {
            let save = self.pos;
            if self.b[self.pos..].starts_with(b"\\u") {
                self.pos += 2;
                canonical &= lower(&self.b[self.pos..(self.pos + 4).min(self.b.len())]);
                let lo = self.hex4()?;
                if (0xdc00..0xe000).contains(&lo) {
                    char::from_u32(0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00))
                } else {
                    self.pos = save;
                    None
                }
            } else {
                None
            }
        } else {
            char::from_u32(cp)
        };
        // Lone surrogates can't live in a Rust string (nor be saved as UTF-8).
        if ch.is_none() {
            canonical = false;
        }
        Ok((ch.unwrap_or('\u{fffd}'), canonical))
    }

    fn hex4(&mut self) -> Result<u32> {
        let Some(h) = self.b.get(self.pos..self.pos + 4) else { return Err(self.err("truncated \\u escape")) };
        let mut v = 0u32;
        for &c in h {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(self.err("invalid \\u escape")),
            };
            v = v * 16 + d as u32;
        }
        self.pos += 4;
        Ok(v)
    }

    /// A multiline field: a string, or an array of strings joined. Returns
    /// `None` (with `pos` unspecified) when the value is neither. For arrays,
    /// the chunk lengths are left in `self.chunks` and the result says
    /// whether the chunking is what Python `splitlines` / a `\n` split gives.
    fn multiline(&mut self) -> Result<Option<(String, Stored)>> {
        match self.peek() {
            Some(b'"') => Ok(Some((self.string_owned()?, Stored::Plain))),
            Some(b'[') => {
                self.pos += 1;
                // Join into a reused scratch buffer, then copy once into an
                // exactly sized String (no growth reallocations).
                let mut buf = std::mem::take(&mut self.scratch);
                buf.clear();
                self.chunks.clear();
                let res = self.join_array(&mut buf);
                let out = match res {
                    Ok(Some(stored)) => Ok(Some((buf.as_str().to_owned(), stored))),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
                self.scratch = buf;
                out
            }
            _ => Ok(None),
        }
    }

    fn join_array(&mut self, buf: &mut String) -> Result<Option<Stored>> {
        let mut first = true;
        let mut check = ChunkCheck::new();
        while self.next_elem(&mut first)? {
            if self.peek() != Some(b'"') {
                return Ok(None);
            }
            let before = buf.len();
            self.string_into(buf)?;
            let chunk = &buf.as_bytes()[before..];
            self.chunks.push(chunk.len() as u32);
            check.push(chunk);
        }
        Ok(Some(check.finish()))
    }

    /// Note the multiline fields of a cell that took the generic path
    /// (source, stream text, mime text), as the fast path would have.
    fn note_generic_cell(&mut self, m: &Map) {
        if let Some(v) = m.get("source") {
            self.note_value_multiline(v, true);
        }
        let Some(Value::Array(outs)) = m.get("outputs") else { return };
        for o in outs {
            let Some(o) = o.as_object() else { continue };
            if let Some(t) = o.get("text")
                && o.get("output_type").and_then(Value::as_str) == Some("stream")
            {
                self.note_value_multiline(t, true);
            }
            if let Some(Value::Object(data)) = o.get("data") {
                for (mime, v) in data {
                    if !is_json_mime(mime) {
                        self.note_value_multiline(v, is_split_mime(mime));
                    }
                }
            }
        }
    }

    fn note_value_multiline(&mut self, v: &Value, lines: bool) {
        match v {
            Value::String(s) => self.note_multiline(s, Stored::Plain, lines),
            Value::Array(a) if a.iter().all(Value::is_string) => {
                let mut check = ChunkCheck::new();
                let mut joined = String::new();
                self.chunks.clear();
                for chunk in a.iter().filter_map(Value::as_str) {
                    check.push(chunk.as_bytes());
                    self.chunks.push(chunk.len() as u32);
                    joined.push_str(chunk);
                }
                self.note_multiline(&joined, check.finish(), lines);
            }
            _ => {}
        }
    }

    /// Record how a multiline field was stored, for the line-style vote and
    /// for per-field shape exceptions. `lines` is false for non-split mime
    /// text (base64 images), which nbformat writes as one string.
    fn note_multiline(&mut self, s: &str, stored: Stored, lines: bool) {
        match (stored, lines) {
            (Stored::Plain, true) => {
                self.ml_strings += 1;
                self.cand_plain.push((crate::text::content_hash(s), s.len()));
            }
            (Stored::Plain, false) => {}
            (Stored::Array { py, nl }, true) => {
                self.ml_arrays += 1;
                if py && nl {
                    if self.ml_strings > 0 {
                        self.cand_lines.push((crate::text::content_hash(s), s.len()));
                    }
                } else {
                    if py {
                        self.py_only += 1;
                    } else if nl {
                        self.nl_only += 1;
                    }
                    let chunks = self.chunks.as_slice().into();
                    self.cand_arrays.push((crate::text::content_hash(s), s.len(), chunks, py, nl));
                }
            }
            (Stored::Array { .. }, false) => {
                let chunks = self.chunks.as_slice().into();
                self.cand_arrays.push((crate::text::content_hash(s), s.len(), chunks, false, false));
            }
        }
    }

    /// Multiline field that counts toward the line-array style vote.
    fn multiline_counted(&mut self) -> Result<Option<String>> {
        match self.multiline()? {
            Some((s, stored)) => {
                self.note_multiline(&s, stored, true);
                Ok(Some(s))
            }
            None => Ok(None),
        }
    }

    /// Decide the file's line style and the per-field exceptions to it.
    fn finish_shapes(&mut self, fmt: &mut FormatHint) {
        fmt.split_lines = self.ml_arrays >= self.ml_strings;
        fmt.line_split = if self.nl_only > 0 && self.py_only == 0 { LineSplit::Newline } else { LineSplit::Python };
        let mut map = HashMap::new();
        if fmt.split_lines {
            for &k in &self.cand_plain {
                map.insert(k, Shape::String);
            }
        } else {
            for &k in &self.cand_lines {
                map.insert(k, Shape::Lines);
            }
        }
        for (h, len, chunks, py, nl) in self.cand_arrays.drain(..) {
            let ok = fmt.split_lines
                && match fmt.line_split {
                    LineSplit::Python => py,
                    LineSplit::Newline => nl,
                };
            if !ok {
                map.insert((h, len), Shape::Chunks(chunks));
            } else if !fmt.split_lines {
                map.insert((h, len), Shape::Lines);
            }
        }
        if !map.is_empty() {
            fmt.shapes = Some(Arc::new(Shapes { map }));
        }
    }

    // ---------------------------------------------------------------- generic values

    pub(crate) fn value(&mut self) -> Result<Value> {
        match self.peek() {
            Some(b'{') => Ok(Value::Object(self.map()?)),
            Some(b'[') => {
                self.enter()?;
                self.pos += 1;
                let mut a = Vec::new();
                let mut first = true;
                while self.next_elem(&mut first)? {
                    a.push(self.value()?);
                }
                self.depth -= 1;
                Ok(Value::Array(a))
            }
            Some(b'"') => Ok(Value::String(self.string_owned()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            // Python's json accepts these; serde_json::Value cannot hold them.
            Some(b'N') => self.literal("NaN", Value::Null),
            Some(b'I') => self.literal("Infinity", Value::Null),
            Some(b'-') if self.b.get(self.pos + 1) == Some(&b'I') => self.literal("-Infinity", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    #[inline]
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        Ok(())
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value> {
        if self.b[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err("expected a JSON value"))
        }
    }

    pub(crate) fn map(&mut self) -> Result<Map> {
        self.enter()?;
        self.expect(b'{', "expected an object")?;
        let mut m = Map::new();
        let mut first = true;
        let mut prev = None;
        while let Some(k) = self.next_key(&mut first, &mut prev)? {
            let v = self.value()?;
            m.insert(k.into_owned(), v);
        }
        self.depth -= 1;
        Ok(m)
    }

    fn number(&mut self) -> Result<Value> {
        let b = self.b;
        let start = self.pos;
        let mut i = start;
        if b[i] == b'-' {
            i += 1;
        }
        let int_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == int_start {
            self.pos = i;
            return Err(self.err("invalid number"));
        }
        let int_end = i;
        let mut float = false;
        if i < b.len() && b[i] == b'.' {
            float = true;
            i += 1;
            let d = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == d {
                self.pos = i;
                return Err(self.err("invalid number"));
            }
        }
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            float = true;
            i += 1;
            if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                i += 1;
            }
            let d = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == d {
                self.pos = i;
                return Err(self.err("invalid number"));
            }
        }
        self.pos = i;
        let text = &self.s[start..i];
        if !float {
            let neg = int_start > start;
            let digits = &b[int_start..int_end];
            if digits.len() <= 19 {
                let mut v: u64 = 0;
                for &d in digits {
                    v = v * 10 + (d - b'0') as u64;
                }
                if !neg {
                    return Ok(Value::Number(v.into()));
                }
                if v <= i64::MAX as u64 + 1 {
                    return Ok(Value::Number((v as i64).wrapping_neg().into()));
                }
            } else if !neg && digits.len() == 20 && let Ok(v) = text.parse::<u64>() {
                return Ok(Value::Number(v.into()));
            }
            // Too large for 64 bits: keep the value as f64 and remember the digits.
            let f: f64 = text.parse().unwrap_or(f64::NAN);
            return Ok(match Number::from_f64(f) {
                Some(n) => {
                    self.numbers.insert(f.to_bits(), (text.into(), true));
                    Value::Number(n)
                }
                None => Value::Null,
            });
        }
        let f: f64 = text.parse().map_err(|_| self.err("invalid number"))?;
        let Some(n) = Number::from_f64(f) else { return Ok(Value::Null) };
        if !self.is_python_repr(text, f) {
            self.numbers.insert(f.to_bits(), (text.into(), false));
        }
        Ok(Value::Number(n))
    }

    /// Whether Python's `repr(f)` spells `f` exactly as `text`.
    #[inline]
    fn is_python_repr(&mut self, text: &str, f: f64) -> bool {
        self.numbuf.clear();
        write_py_float(&mut self.numbuf, f);
        self.numbuf == text
    }

    /// `null` or an integer.
    fn opt_int(&mut self) -> Result<Option<Option<i64>>> {
        match self.peek() {
            Some(b'n') => {
                self.literal("null", Value::Null)?;
                Ok(Some(None))
            }
            Some(b'-' | b'0'..=b'9') => match self.number()? {
                Value::Number(n) if n.is_i64() => Ok(Some(n.as_i64())),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Keys from recorded spans (`ok` = no key needed unescaping), else by
    /// re-reading the object at `start`.
    fn keys_from_spans(&mut self, spans: &[(usize, usize)], ok: bool, start: usize) -> Result<Vec<Box<str>>> {
        if ok {
            Ok(spans.iter().map(|&(a, b)| Box::from(&self.s[a..b])).collect())
        } else {
            self.object_keys_at(start)
        }
    }

    /// Key order of the object starting at `start` (rare path: first object
    /// of a kind, or one whose order differs from its kind's template).
    #[cold]
    fn object_keys_at(&mut self, start: usize) -> Result<Vec<Box<str>>> {
        let save = self.pos;
        self.pos = start + 1;
        let mut keys = Vec::new();
        let mut first = true;
        let mut prev = None;
        while let Some(k) = self.next_key(&mut first, &mut prev)? {
            keys.push(Box::from(&*k));
            self.skip_value()?;
        }
        self.pos = save;
        Ok(keys)
    }

    /// Step over a value without building it (used when re-reading an
    /// object's keys; its content was already parsed once).
    fn skip_value(&mut self) -> Result<()> {
        match self.peek() {
            Some(b'{') => {
                self.enter()?;
                self.pos += 1;
                let mut first = true;
                let mut prev = None;
                while self.next_key(&mut first, &mut prev)?.is_some() {
                    self.skip_value()?;
                }
                self.depth -= 1;
                Ok(())
            }
            Some(b'[') => {
                self.enter()?;
                self.pos += 1;
                let mut first = true;
                while self.next_elem(&mut first)? {
                    self.skip_value()?;
                }
                self.depth -= 1;
                Ok(())
            }
            Some(b'"') => {
                let mut i = self.pos + 1;
                loop {
                    match self.find_special(i) {
                        Some(at) if self.b[at] == b'"' => {
                            self.pos = at + 1;
                            return Ok(());
                        }
                        Some(at) => i = at + 2,
                        None => {
                            self.pos = self.b.len();
                            return Err(self.err("unterminated string"));
                        }
                    }
                }
            }
            _ => self.value().map(drop),
        }
    }

    fn note_template(&mut self, kind: ObjKind, keys: &[Cow<'a, str>]) {
        let slot = &mut self.templates[kind as usize];
        if slot.is_none() {
            *slot = Some(keys.iter().map(|k| Box::from(&**k)).collect());
        }
    }

    /// Record a cell's key order: the template for its kind if it is the
    /// first, an override if it differs from the template. `sig` is
    /// [`key_sig`] of its keys; `keys` produces them (only called on the rare
    /// paths).
    fn note_cell_order(&mut self, kind: ObjKind, cell: &Cell, index: usize, sig: u64, keys: impl FnOnce(&mut Self) -> Result<Vec<Box<str>>>) -> Result<()> {
        let k = kind as usize;
        if self.templates[k].is_none() {
            self.templates[k] = Some(keys(self)?);
            self.template_sigs[k] = sig;
        } else if self.template_sigs[k] != sig {
            let r = match &cell.id {
                Some(id) => CellRef::Id(id.as_str().into()),
                None => CellRef::Index(index),
            };
            let keys = keys(self)?;
            self.cell_orders.insert(r, keys);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- notebook

    fn notebook(&mut self) -> Result<Notebook> {
        self.ws();
        if self.peek() != Some(b'{') {
            // Distinguish "valid JSON, not a notebook" from garbage.
            let v = self.value()?;
            let _ = v;
            return Err(Error::Invalid("top level is not a JSON object".into()));
        }
        self.pos += 1;
        let mut major = None;
        let mut minor = None;
        let mut metadata = None;
        let mut cells = None;
        let mut extra = Map::new();
        let mut keys = Vec::with_capacity(4);
        let mut first = true;
        let mut prev = None;
        while let Some(key) = self.next_key(&mut first, &mut prev)? {
            match &*key {
                "cells" if self.peek() == Some(b'[') => cells = Some(self.cells()?),
                "metadata" if self.peek() == Some(b'{') => metadata = Some(self.map()?),
                "nbformat" | "nbformat_minor" => {
                    let v = self.value()?;
                    let n = v.as_u64().ok_or_else(|| Error::Invalid(format!("`{key}` is not an integer")))?;
                    if &*key == "nbformat" {
                        major = Some(n)
                    } else {
                        minor = Some(n)
                    }
                }
                _ => {
                    let v = self.value()?;
                    extra.insert(key.to_string(), v);
                }
            }
            keys.push(key);
        }
        self.ws();
        if self.pos != self.b.len() {
            return Err(self.err("trailing characters after the notebook"));
        }
        self.note_template(ObjKind::Notebook, &keys);
        let major = major.unwrap_or(1);
        let minor = minor.unwrap_or(0);
        if major == 3 {
            self.upgraded = true;
            return crate::v3::upgrade(metadata.unwrap_or_default(), extra, minor);
        }
        if major != 4 {
            return Err(Error::UnsupportedVersion { nbformat: major, nbformat_minor: minor });
        }
        if let Some(v) = extra.get("metadata") {
            return Err(Error::Invalid(format!("`metadata` is not an object: {v}")));
        }
        if let Some(v) = extra.get("cells") {
            return Err(Error::Invalid(format!("`cells` is not an array: {v}")));
        }
        Ok(Notebook {
            nbformat: major as u32,
            nbformat_minor: minor as u32,
            metadata: metadata.unwrap_or_default(),
            cells: cells.unwrap_or_default(),
            extra,
            format: FormatHint::jupyter(),
        })
    }

    fn cells(&mut self) -> Result<Vec<Cell>> {
        self.pos += 1;
        let mut cells = Vec::new();
        let mut first = true;
        while self.next_elem(&mut first)? {
            let start = self.pos;
            let index = cells.len();
            let cell = match self.cell_fast()? {
                Some((cell, sig)) => {
                    self.note_cell_order(cell_kind(cell.cell_type), &cell, index, sig, |p| {
                        let spans = std::mem::take(&mut p.cell_keys);
                        let keys = p.keys_from_spans(&spans, p.cell_keys_ok, start);
                        p.cell_keys = spans;
                        keys
                    })?;
                    cell
                }
                None => {
                    self.pos = start;
                    match self.value()? {
                        Value::Object(m) => {
                            self.note_generic_cell(&m);
                            let keys: Vec<Box<str>> = m.keys().map(|k| Box::from(k.as_str())).collect();
                            let sig = keys.iter().fold(KEY_SIG_SEED, |h, k| key_sig(h, k));
                            let cell = convert::cell_from_map(m).map_err(|e| Error::Invalid(format!("cell {index}: {e}")))?;
                            self.note_cell_order(cell_kind(cell.cell_type), &cell, index, sig, |_| Ok(keys))?;
                            cell
                        }
                        _ => return Err(Error::Invalid(format!("cell {index} is not an object"))),
                    }
                }
            };
            cells.push(cell);
        }
        Ok(cells)
    }

    /// Typed cell parse (with its key-order signature); `None` means
    /// "unusual shape, re-parse generically".
    fn cell_fast(&mut self) -> Result<Option<(Cell, u64)>> {
        if self.peek() != Some(b'{') {
            return Ok(None);
        }
        self.pos += 1;
        let mut cell = Cell::new(CellType::Code);
        let mut cell_type = None;
        let mut code_keys = false;
        let mut sig = KEY_SIG_SEED;
        let mut first = true;
        let mut prev = None;
        self.cell_keys.clear();
        self.cell_keys_ok = true;
        while let Some(key) = self.next_key(&mut first, &mut prev)? {
            if let Cow::Borrowed(k) = &key {
                let at = k.as_ptr() as usize - self.s.as_ptr() as usize;
                self.cell_keys.push((at, at + k.len()));
            } else {
                self.cell_keys_ok = false;
            }
            match &*key {
                "cell_type" => {
                    if self.peek() != Some(b'"') {
                        return Ok(None);
                    }
                    let t = self.string()?;
                    match CellType::from_str_opt(&t) {
                        Some(t) => cell_type = Some(t),
                        None => return Ok(None),
                    }
                }
                "id" => {
                    if self.peek() != Some(b'"') {
                        return Ok(None);
                    }
                    cell.id = Some(self.string_owned()?);
                }
                "metadata" => {
                    if self.peek() != Some(b'{') {
                        return Ok(None);
                    }
                    cell.metadata = self.map()?;
                }
                "source" => match self.multiline_counted()? {
                    Some(s) => cell.source = s,
                    None => return Ok(None),
                },
                "execution_count" => match self.opt_int()? {
                    Some(n) => {
                        cell.execution_count = n;
                        code_keys = true;
                    }
                    None => return Ok(None),
                },
                "outputs" => {
                    if self.peek() != Some(b'[') {
                        return Ok(None);
                    }
                    cell.outputs = self.outputs()?;
                    code_keys = true;
                }
                "attachments" => {
                    if self.peek() == Some(b'n') {
                        return Ok(None);
                    }
                    let mut a = self.value()?;
                    rejoin_attachments(&mut a);
                    cell.attachments = Some(a);
                }
                _ => {
                    let v = self.value()?;
                    cell.extra.insert(key.to_string(), v);
                }
            }
            sig = key_sig(sig, &key);
        }
        let Some(cell_type) = cell_type else { return Ok(None) };
        if cell_type != CellType::Code && code_keys {
            return Ok(None);
        }
        cell.cell_type = cell_type;
        Ok(Some((cell, sig)))
    }

    fn outputs(&mut self) -> Result<Vec<Output>> {
        self.pos += 1;
        let mut outs = Vec::new();
        let mut first = true;
        while self.next_elem(&mut first)? {
            let start = self.pos;
            let out = match self.output_fast()? {
                Some(o) => o,
                None => {
                    self.pos = start;
                    match self.value()? {
                        Value::Object(m) => convert::output_from_map(m),
                        other => {
                            let mut m = Map::new();
                            m.insert("output_type".into(), other);
                            Output::Unknown(m)
                        }
                    }
                }
            };
            outs.push(out);
        }
        Ok(outs)
    }

    /// Typed output parse; `None` means "unusual shape, re-parse generically".
    fn output_fast(&mut self) -> Result<Option<Output>> {
        if self.peek() != Some(b'{') {
            return Ok(None);
        }
        self.pos += 1;
        let mut output_type: Option<Cow<'a, str>> = None;
        let mut name = None;
        let mut text = None;
        let mut data = None;
        let mut metadata = None;
        let mut execution_count = None;
        let mut ename = None;
        let mut evalue = None;
        let mut traceback = None;
        let mut transient = None;
        let start = self.pos - 1;
        let mut first = true;
        let mut prev = None;
        self.out_keys.clear();
        self.out_keys_ok = true;
        while let Some(key) = self.next_key(&mut first, &mut prev)? {
            if let Cow::Borrowed(k) = &key {
                let at = k.as_ptr() as usize - self.s.as_ptr() as usize;
                self.out_keys.push((at, at + k.len()));
            } else {
                self.out_keys_ok = false;
            }
            match &*key {
                "output_type" | "name" | "ename" | "evalue" => {
                    if self.peek() != Some(b'"') {
                        return Ok(None);
                    }
                    let s = self.string()?;
                    match &*key {
                        "output_type" => output_type = Some(s),
                        "name" => name = Some(s),
                        "ename" => ename = Some(s.into_owned()),
                        _ => evalue = Some(s.into_owned()),
                    }
                }
                "text" => match self.multiline()? {
                    Some(t) => text = Some(t),
                    None => return Ok(None),
                },
                "data" => {
                    if self.peek() != Some(b'{') {
                        return Ok(None);
                    }
                    data = Some(self.bundle()?);
                }
                "metadata" => {
                    if self.peek() != Some(b'{') {
                        return Ok(None);
                    }
                    metadata = Some(self.map()?);
                }
                "execution_count" => match self.opt_int()? {
                    Some(n) => execution_count = Some(n),
                    None => return Ok(None),
                },
                "traceback" => {
                    if self.peek() != Some(b'[') {
                        return Ok(None);
                    }
                    self.pos += 1;
                    let mut tb = Vec::new();
                    let mut f = true;
                    while self.next_elem(&mut f)? {
                        if self.peek() != Some(b'"') {
                            return Ok(None);
                        }
                        tb.push(self.string_owned()?);
                    }
                    traceback = Some(tb);
                }
                "transient" => transient = Some(self.value()?),
                _ => return Ok(None),
            }
        }
        let Some(ot) = output_type else { return Ok(None) };
        let (out, kind) = match &*ot {
            "stream" => {
                let (Some(name), Some((text, stored))) = (name, text) else { return Ok(None) };
                let Some(name) = StreamName::from_str_opt(&name) else { return Ok(None) };
                if data.is_some() || metadata.is_some() || execution_count.is_some() || ename.is_some() || evalue.is_some() || traceback.is_some() || transient.is_some() {
                    return Ok(None);
                }
                self.note_multiline(&text, stored, true);
                (Output::Stream { name, text }, ObjKind::Stream)
            }
            "display_data" => {
                let Some(data) = data else { return Ok(None) };
                if name.is_some() || text.is_some() || execution_count.is_some() || ename.is_some() || evalue.is_some() || traceback.is_some() {
                    return Ok(None);
                }
                let transient = transient.filter(|t| !t.is_null());
                (Output::DisplayData { data, metadata: metadata.unwrap_or_default(), transient }, ObjKind::DisplayData)
            }
            "execute_result" => {
                let Some(data) = data else { return Ok(None) };
                if name.is_some() || text.is_some() || ename.is_some() || evalue.is_some() || traceback.is_some() {
                    return Ok(None);
                }
                (
                    Output::ExecuteResult { execution_count: execution_count.flatten(), data, metadata: metadata.unwrap_or_default() },
                    ObjKind::ExecuteResult,
                )
            }
            "error" => {
                let (Some(ename), Some(evalue), Some(traceback)) = (ename, evalue, traceback) else { return Ok(None) };
                if name.is_some() || text.is_some() || data.is_some() || metadata.is_some() || execution_count.is_some() || transient.is_some() {
                    return Ok(None);
                }
                (Output::Error { ename, evalue, traceback }, ObjKind::Error)
            }
            _ => return Ok(None),
        };
        if self.templates[kind as usize].is_none() {
            let spans = std::mem::take(&mut self.out_keys);
            let keys = self.keys_from_spans(&spans, self.out_keys_ok, start);
            self.out_keys = spans;
            self.templates[kind as usize] = Some(keys?);
        }
        Ok(Some(out))
    }

    fn bundle(&mut self) -> Result<MimeBundle> {
        self.pos += 1;
        let mut v = Vec::with_capacity(4);
        let mut first = true;
        let mut prev = None;
        while let Some(key) = self.next_key(&mut first, &mut prev)? {
            let data = if is_json_mime(&key) {
                MimeData::Json(self.value()?)
            } else {
                match self.peek() {
                    Some(b'"') => {
                        let s = self.string_owned()?;
                        if is_split_mime(&key) {
                            self.note_multiline(&s, Stored::Plain, true);
                        }
                        MimeData::Text(s)
                    }
                    Some(b'[') => {
                        let save = self.pos;
                        match self.multiline()? {
                            Some((s, stored)) => {
                                self.note_multiline(&s, stored, is_split_mime(&key));
                                MimeData::Text(s)
                            }
                            None => {
                                self.pos = save;
                                MimeData::Json(self.value()?)
                            }
                        }
                    }
                    _ => MimeData::Json(self.value()?),
                }
            };
            v.push((key.into_owned(), data));
        }
        Ok(MimeBundle(v))
    }
}

/// A line array not cut by both rules: (content hash, length, chunk lengths,
/// splitlines-consistent, `\n`-split-consistent).
type ArrayCandidate = (u64, usize, Box<[u32]>, bool, bool);

/// How a multiline field was stored.
#[derive(Clone, Copy, Debug)]
enum Stored {
    Plain,
    Array { py: bool, nl: bool },
}

/// Whether a line array's chunks are exactly what Python's `splitlines(True)`
/// (`py`) and a split after each `\n` (`nl`) would produce for their join.
struct ChunkCheck {
    py: bool,
    nl: bool,
    py_open: bool,
    nl_open: bool,
    prev_cr: bool,
}

impl ChunkCheck {
    fn new() -> Self {
        ChunkCheck { py: true, nl: true, py_open: false, nl_open: false, prev_cr: false }
    }

    #[inline]
    fn push(&mut self, chunk: &[u8]) {
        // A previous chunk without a terminator is only fine as the last one.
        if self.py_open {
            self.py = false;
        }
        if self.nl_open {
            self.nl = false;
        }
        if chunk.is_empty() {
            self.py = false;
            self.nl = false;
            return;
        }
        if self.py {
            let (end, terminated) = crate::text::first_line(chunk);
            // "\r" + "\n" across a chunk boundary is one Python line break.
            if end < chunk.len() || (self.prev_cr && chunk[0] == b'\n') {
                self.py = false;
            }
            self.py_open = !terminated;
            // The usual line: its only break is the final "\n", so a "\n" split agrees.
            if end == chunk.len() && terminated && chunk[end - 1] == b'\n' {
                self.nl_open = false;
                self.prev_cr = false;
                return;
            }
        }
        if self.nl {
            match memchr::memchr(b'\n', chunk) {
                Some(i) if i + 1 == chunk.len() => self.nl_open = false,
                Some(_) => self.nl = false,
                None => self.nl_open = true,
            }
        }
        self.prev_cr = chunk.last() == Some(&b'\r');
    }

    fn finish(self) -> Stored {
        Stored::Array { py: self.py, nl: self.nl }
    }
}

/// Append `src[from..to]` to `v`. Short runs are copied as one fixed 64-byte
/// block (into spare capacity, then truncated), avoiding a `memcpy` call per
/// source line; the source must have 64 readable bytes from `from`.
#[inline(always)]
fn copy_segment(v: &mut Vec<u8>, src: &[u8], from: usize, to: usize) {
    let len = to - from;
    if len <= 64 && from + 64 <= src.len() {
        v.reserve(64);
        // SAFETY: `from + 64 <= src.len()`; `v` has >= 64 bytes of spare
        // capacity; only `len` of the copied bytes become part of `v`.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr().add(from), v.as_mut_ptr().add(v.len()), 64);
            v.set_len(v.len() + len);
        }
    } else {
        v.extend_from_slice(&src[from..to]);
    }
}

const KEY_SIG_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a over an object's key sequence (order-sensitive), so key order can
/// be compared against a template without collecting the keys.
#[inline]
fn key_sig(mut h: u64, key: &str) -> u64 {
    for &b in key.as_bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    (h ^ 0xff).wrapping_mul(0x0100_0000_01b3)
}

fn cell_kind(t: CellType) -> ObjKind {
    match t {
        CellType::Code => ObjKind::CodeCell,
        CellType::Markdown => ObjKind::MarkdownCell,
        CellType::Raw => ObjKind::RawCell,
    }
}

/// Detect indentation, newline style, separators and the trailing newline
/// from the raw text.
pub(crate) fn detect_whitespace(b: &[u8]) -> FormatHint {
    let mut fmt = FormatHint::jupyter();
    fmt.trailing_newline = b.ends_with(b"\n");
    let content_end = if fmt.trailing_newline { b.len() - 1 } else { b.len() };
    // Raw newlines never occur inside JSON strings, so the first one is structural.
    match memchr::memchr(b'\n', &b[..content_end]) {
        Some(nl) => {
            fmt.crlf = nl > 0 && b[nl - 1] == b'\r';
            let rest = &b[nl + 1..];
            let spaces = rest.iter().take_while(|&&c| c == b' ').count();
            let tabs = rest.iter().take_while(|&&c| c == b'\t').count();
            fmt.indent = if tabs > 0 { Indent::Tabs(tabs.min(255) as u8) } else { Indent::Spaces(spaces.min(255) as u8) };
            // Item separator: look at the first "...,\n" / "..., \n".
            for i in memchr::memchr_iter(b'\n', b) {
                let mut j = i;
                if j > 0 && b[j - 1] == b'\r' {
                    j -= 1;
                }
                if j > 0 && b[j - 1] == b',' {
                    fmt.comma_space = false;
                    break;
                }
                if j > 1 && b[j - 1] == b' ' && b[j - 2] == b',' {
                    fmt.comma_space = true;
                    break;
                }
            }
        }
        None => {
            fmt.indent = Indent::Compact;
            fmt.crlf = b.ends_with(b"\r\n");
            fmt.comma_space = compact_comma_space(b);
        }
    }
    // Key separator: after the first key of the top-level object.
    if let Some(q) = memchr::memchr(b'"', b)
        && let Some(end) = memchr::memchr2(b'"', b'\\', &b[q + 1..])
    {
        let after = &b[q + 1 + end + 1..];
        fmt.colon_space = after.starts_with(b": ");
    }
    fmt
}

/// In a compact file, whether the first structural comma is followed by a space.
fn compact_comma_space(b: &[u8]) -> bool {
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b',' => return b.get(i + 1) == Some(&b' '),
                _ => {}
            }
        }
        i += 1;
    }
    false
}
