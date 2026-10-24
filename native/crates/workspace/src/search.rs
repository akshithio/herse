//! Cross-notebook full-text search over cell sources (optionally outputs).
//!
//! Pipeline per notebook: read the bytes; if the query is a plain literal
//! that JSON can't escape differently, reject the whole file with one SIMD
//! literal scan of the raw bytes (most files in a search don't match); then
//! scan the structure (outputs are skipped at the byte level), reject each
//! cell's raw source with the same literal scan, and only decode + regex-match
//! the cells that can match. Notebooks are processed in parallel while the
//! folder is still being walked, so the first hit streams out immediately.

use crate::io::read_limited;
use crate::json::{Cursor, R, decode_multiline, decode_str};
use crate::nbscan::{RawCell, scan_notebook};
use crate::walk::{WalkOptions, is_notebook_path, walker};
use anyhow::{Context, Result};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};
use memchr::memchr;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    /// Treat `text` as a regular expression (Rust `regex` syntax, multi-line
    /// mode: `^`/`$` match at line boundaries).
    pub regex: bool,
    /// Match only at word boundaries (on the sides of the literal that start /
    /// end with a word character, like VS Code).
    pub whole_word: bool,
    /// Also search text outputs (stream text, `text/plain`|`text/markdown`|
    /// `text/latex` results, error tracebacks with ANSI codes stripped).
    pub include_outputs: bool,
    /// Stop after this many hits (sets [`SearchStats::truncated`]).
    pub max_hits: Option<usize>,
    /// Notebooks larger than this are skipped (counted in `files_skipped`).
    pub max_file_size: Option<u64>,
    /// How [`search`] finds notebooks under the root.
    pub walk: WalkOptions,
}

impl Default for SearchQuery {
    fn default() -> Self {
        SearchQuery {
            text: String::new(),
            case_sensitive: false,
            regex: false,
            whole_word: false,
            include_outputs: false,
            max_hits: Some(10_000),
            max_file_size: Some(256 * 1024 * 1024),
            walk: WalkOptions::default(),
        }
    }
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        SearchQuery { text: text.into(), ..Default::default() }
    }

    /// Validates and compiles the query (an invalid regex is an error; show it
    /// in the UI while the user types).
    pub fn compile(&self) -> Result<CompiledQuery> {
        CompiledQuery::new(self)
    }
}

/// Where in the cell a hit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitLocation {
    Source,
    /// In the text of `outputs[index]`.
    Output { index: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub path: PathBuf,
    pub cell_index: usize,
    /// nbformat ≥ 4.5 cell id — more robust than the index if the file changed.
    pub cell_id: Option<String>,
    /// "code" | "markdown" | "raw" (or the raw value for unknown types).
    pub cell_type: String,
    pub location: HitLocation,
    /// 0-based line within the cell source (or within the output's text).
    pub line: usize,
    /// The matching line with leading whitespace trimmed; lines longer than
    /// ~320 bytes are cut to a window around the first match.
    pub line_text: String,
    /// Byte offset of `line_text[0]` within the full line, so the column of a
    /// range in the source is `line_offset + range.start`.
    pub line_offset: usize,
    /// Byte ranges of the matches within `line_text` (non-empty, ascending).
    pub ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchStats {
    pub files_scanned: usize,
    pub files_matched: usize,
    /// Larger than `max_file_size`.
    pub files_skipped: usize,
    /// Unreadable or not valid notebook JSON.
    pub errors: usize,
    pub bytes_scanned: u64,
    pub hits: usize,
    /// Time from the call to the first `on_hit`.
    pub first_hit: Option<Duration>,
    pub elapsed: Duration,
    pub cancelled: bool,
    /// `max_hits` was reached.
    pub truncated: bool,
}

/// A compiled [`SearchQuery`]; cheap to share between threads.
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    re: Regex,
    /// Literal prefilter on raw JSON bytes (only when sound, see `new`).
    prefilter: Option<regex::bytes::Regex>,
    include_outputs: bool,
    max_hits: Option<usize>,
    max_file_size: Option<u64>,
    walk: WalkOptions,
    empty: bool,
}

const MAX_LINE: usize = 320;
const CONTEXT_BEFORE: usize = 60;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl CompiledQuery {
    fn new(q: &SearchQuery) -> Result<Self> {
        let text = q.text.as_str();
        let mut pat = if q.regex { text.to_string() } else { regex::escape(text) };
        if q.whole_word && !text.is_empty() {
            if q.regex {
                pat = format!(r"\b(?:{pat})\b");
            } else {
                if text.chars().next().is_some_and(is_word_char) {
                    pat.insert_str(0, r"\b");
                }
                if text.chars().next_back().is_some_and(is_word_char) {
                    pat.push_str(r"\b");
                }
            }
        }
        let re = RegexBuilder::new(&pat)
            .case_insensitive(!q.case_sensitive)
            .multi_line(true)
            .crlf(true)
            .build()
            .with_context(|| format!("invalid search pattern {text:?}"))?;
        // A literal can be looked for in the raw JSON only if JSON would
        // store it verbatim: printable ASCII without `"` or `\`.
        let json_safe = !q.regex && !text.is_empty() && text.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\');
        let prefilter = if json_safe {
            Some(regex::bytes::RegexBuilder::new(&regex::escape(text)).case_insensitive(!q.case_sensitive).build()?)
        } else {
            None
        };
        Ok(CompiledQuery {
            re,
            prefilter,
            include_outputs: q.include_outputs,
            max_hits: q.max_hits,
            max_file_size: q.max_file_size,
            walk: q.walk.clone(),
            empty: text.is_empty(),
        })
    }

    #[inline]
    fn may_match(&self, raw: &[u8]) -> bool {
        self.prefilter.as_ref().is_none_or(|p| p.is_match(raw))
    }

    /// Searches one notebook's bytes (e.g. an unsaved in-memory buffer).
    /// `on_hit` returns `false` to stop. Returns the number of hits delivered.
    pub fn search_bytes(&self, path: &Path, bytes: &[u8], mut on_hit: impl FnMut(SearchHit) -> bool) -> Result<usize> {
        let mut n = 0;
        let mut scratch = String::new();
        self.scan(path, bytes, &mut scratch, &|| false, &mut |h| {
            n += 1;
            on_hit(h)
        })
        .map_err(|e| anyhow::anyhow!(e).context(format!("searching {}", path.display())))?;
        Ok(n)
    }

    /// Returns Ok(()) also when stopped early.
    fn scan(
        &self,
        path: &Path,
        bytes: &[u8],
        scratch: &mut String,
        stop: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(SearchHit) -> bool,
    ) -> Result<(), crate::json::ScanError> {
        if self.empty || !self.may_match(bytes) {
            return Ok(());
        }
        let mut texts: Vec<(usize, String)> = Vec::new();
        scan_notebook(bytes, &mut |cell: RawCell<'_>| {
            if stop() {
                return false;
            }
            let mut ctx = HitCtx { path, cell: &cell, cell_id: None, location: HitLocation::Source };
            if let Some(src) = cell.source
                && self.may_match(src)
            {
                scratch.clear();
                decode_multiline(src, scratch);
                if !self.match_text(scratch, &mut ctx, emit) {
                    return false;
                }
            }
            if self.include_outputs
                && cell.outputs_nonempty
                && let Some(outs) = cell.outputs
                && self.may_match(outs)
            {
                texts.clear();
                if output_texts(outs, &mut texts).is_err() {
                    return true;
                }
                for (index, text) in &texts {
                    ctx.location = HitLocation::Output { index: *index };
                    if !self.match_text(text, &mut ctx, emit) {
                        return false;
                    }
                }
            }
            true
        })?;
        Ok(())
    }

    /// Matches `text`, emitting one hit per matching line. Returns false if
    /// the consumer asked to stop.
    fn match_text(&self, text: &str, ctx: &mut HitCtx<'_>, emit: &mut dyn FnMut(SearchHit) -> bool) -> bool {
        let bytes = text.as_bytes();
        let mut line_no = 0usize;
        let mut line_start = 0usize;
        // (line_no, line_start, line_end, ranges relative to line_start)
        let mut pending: Option<(usize, usize, usize, Vec<Range<usize>>)> = None;
        for m in self.re.find_iter(text) {
            if m.is_empty() {
                continue;
            }
            let mut moved = false;
            while let Some(off) = memchr(b'\n', &bytes[line_start..m.start()]) {
                line_start += off + 1;
                line_no += 1;
                moved = true;
            }
            if (moved || pending.is_none())
                && let Some(p) = pending.take()
                && !emit(ctx.hit(text, p))
            {
                return false;
            }
            let p = pending.get_or_insert_with(|| {
                let end = memchr(b'\n', &bytes[line_start..]).map_or(bytes.len(), |o| line_start + o);
                (line_no, line_start, end, Vec::new())
            });
            p.3.push(m.start() - p.1..m.end().min(p.2) - p.1);
        }
        match pending {
            Some(p) => emit(ctx.hit(text, p)),
            None => true,
        }
    }
}

struct HitCtx<'c> {
    path: &'c Path,
    cell: &'c RawCell<'c>,
    cell_id: Option<Option<String>>,
    location: HitLocation,
}

impl HitCtx<'_> {
    fn hit(&mut self, text: &str, (line, start, end, ranges): (usize, usize, usize, Vec<Range<usize>>)) -> SearchHit {
        let cell = self.cell;
        let cell_id = self
            .cell_id
            .get_or_insert_with(|| {
                cell.id.map(|raw| {
                    let mut s = String::new();
                    decode_str(raw, &mut s);
                    s
                })
            })
            .clone();
        let (line_text, line_offset, ranges) = window(&text[start..end], ranges);
        SearchHit {
            path: self.path.to_path_buf(),
            cell_index: cell.index,
            cell_id,
            cell_type: match cell.kind {
                crate::nbscan::CellKind::Other => String::from_utf8_lossy(&cell.cell_type).into_owned(),
                k => k.as_str().to_string(),
            },
            location: self.location,
            line,
            line_text,
            line_offset,
            ranges,
        }
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Trims leading whitespace / cuts long lines; returns (text, offset, ranges).
fn window(line: &str, mut ranges: Vec<Range<usize>>) -> (String, usize, Vec<Range<usize>>) {
    let line = line.strip_suffix('\r').unwrap_or(line);
    for r in &mut ranges {
        r.end = r.end.min(line.len());
        r.start = r.start.min(r.end);
    }
    let first = ranges.first().map_or(0, |r| r.start);
    let lead = line.len() - line.trim_start().len();
    let mut start = lead.min(first);
    let mut end = line.len();
    if end - start > MAX_LINE {
        start = floor_boundary(line, start.max(first.saturating_sub(CONTEXT_BEFORE)));
        end = floor_boundary(line, (start + MAX_LINE).max(ranges[0].end));
    }
    let ranges = ranges
        .into_iter()
        .filter(|r| r.start >= start && r.start < end)
        .map(|r| r.start - start..r.end.min(end) - start)
        .filter(|r| !r.is_empty())
        .collect();
    (line[start..end].to_string(), start, ranges)
}

/// Collects the searchable text of each output: `text` (stream / v3), else
/// the best of `data["text/plain" | "text/markdown" | "text/latex"]`, else the
/// traceback (or `ename: evalue`). ANSI escapes are stripped.
fn output_texts(raw: &[u8], out: &mut Vec<(usize, String)>) -> R<()> {
    let mut c = Cursor { b: raw, i: 0 };
    c.array(|c, index| {
        if c.peek()? != b'{' {
            return c.skip_value();
        }
        let (mut text, mut data, mut tb, mut ename, mut evalue) = (None, None, None, None, None);
        let mut data_rank = 0;
        c.object(|c, k| {
            match &*k {
                b"text" => text = Some(c.raw_value()?),
                b"traceback" => tb = Some(c.raw_value()?),
                b"ename" => ename = c.opt_string()?,
                b"evalue" => evalue = c.opt_string()?,
                b"data" if c.peek()? == b'{' => c.object(|c, mime| {
                    let rank = match &*mime {
                        b"text/plain" => 3,
                        b"text/markdown" => 2,
                        b"text/latex" => 1,
                        _ => 0,
                    };
                    if rank > data_rank {
                        data_rank = rank;
                        data = Some(c.raw_value()?);
                        Ok(())
                    } else {
                        c.skip_value()
                    }
                })?,
                _ => c.skip_value()?,
            }
            Ok(())
        })?;
        let mut s = String::new();
        if let Some(raw) = text.or(data) {
            decode_multiline(raw, &mut s);
        } else if let Some(raw) = tb {
            let mut tc = Cursor { b: raw, i: 0 };
            if tc.peek()? == b'[' {
                tc.array(|tc, i| {
                    if i > 0 {
                        s.push('\n');
                    }
                    if let Some(line) = tc.opt_string_raw()? {
                        decode_str(line, &mut s);
                    }
                    Ok(())
                })?;
            }
        }
        if s.is_empty()
            && let Some(e) = ename
        {
            s = format!("{e}: {}", evalue.unwrap_or_default());
        }
        if !s.is_empty() {
            out.push((index, strip_ansi(s)));
        }
        Ok(())
    })
}

/// Removes ANSI CSI / OSC escape sequences.
pub(crate) fn strip_ansi(s: String) -> String {
    if !s.contains('\x1b') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('[') => {
                it.next();
                for n in it.by_ref() {
                    if ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\x07' || (n == '\x1b' && it.peek() == Some(&'\\')) {
                        if n == '\x1b' {
                            it.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

thread_local! {
    static SCRATCH: std::cell::RefCell<(Vec<u8>, String)> = const { std::cell::RefCell::new((Vec::new(), String::new())) };
}

/// Shared state of one search run.
struct Run<'s> {
    q: &'s CompiledQuery,
    cancel: &'s AtomicBool,
    stop: AtomicBool,
    on_hit: &'s (dyn Fn(SearchHit) + Sync),
    started: Instant,
    first_hit_ns: AtomicU64,
    hits: AtomicUsize,
    files_scanned: AtomicUsize,
    files_matched: AtomicUsize,
    files_skipped: AtomicUsize,
    errors: AtomicUsize,
    bytes: AtomicU64,
    truncated: AtomicBool,
}

impl<'s> Run<'s> {
    fn new(q: &'s CompiledQuery, cancel: &'s AtomicBool, on_hit: &'s (dyn Fn(SearchHit) + Sync)) -> Self {
        Run {
            q,
            cancel,
            stop: AtomicBool::new(false),
            on_hit,
            started: Instant::now(),
            first_hit_ns: AtomicU64::new(u64::MAX),
            hits: AtomicUsize::new(0),
            files_scanned: AtomicUsize::new(0),
            files_matched: AtomicUsize::new(0),
            files_skipped: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            truncated: AtomicBool::new(false),
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed)
    }

    /// Returns false when the run should stop.
    fn file(&self, path: &Path, buf: &mut Vec<u8>, scratch: &mut String) -> bool {
        if self.stopped() {
            return false;
        }
        buf.clear();
        match read_limited(path, buf, self.q.max_file_size) {
            Ok(true) => {}
            Ok(false) => {
                self.files_skipped.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            Err(e) => {
                log::debug!("search: {}: {e}", path.display());
                self.errors.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        self.files_scanned.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        let mut matched = false;
        let res = self.q.scan(path, buf, scratch, &|| self.stopped(), &mut |hit| {
            let n = self.hits.fetch_add(1, Ordering::Relaxed);
            if self.q.max_hits.is_some_and(|max| n >= max) {
                self.truncated.store(true, Ordering::Relaxed);
                self.stop.store(true, Ordering::Relaxed);
                return false;
            }
            if !matched {
                matched = true;
                self.files_matched.fetch_add(1, Ordering::Relaxed);
                let ns = self.started.elapsed().as_nanos() as u64;
                self.first_hit_ns.fetch_min(ns, Ordering::Relaxed);
            }
            (self.on_hit)(hit);
            !self.stopped()
        });
        if let Err(e) = res {
            log::debug!("search: {}: {e}", path.display());
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        !self.stopped()
    }

    fn stats(self) -> SearchStats {
        let first = self.first_hit_ns.into_inner();
        let hits = self.hits.into_inner();
        let max = self.q.max_hits.unwrap_or(usize::MAX);
        SearchStats {
            files_scanned: self.files_scanned.into_inner(),
            files_matched: self.files_matched.into_inner(),
            files_skipped: self.files_skipped.into_inner(),
            errors: self.errors.into_inner(),
            bytes_scanned: self.bytes.into_inner(),
            hits: hits.min(max),
            first_hit: (first != u64::MAX).then(|| Duration::from_nanos(first)),
            elapsed: self.started.elapsed(),
            cancelled: self.cancel.load(Ordering::Relaxed),
            truncated: self.truncated.into_inner(),
        }
    }
}

struct Visitor<'s> {
    run: &'s Run<'s>,
    buf: Vec<u8>,
    scratch: String,
}

impl ParallelVisitor for Visitor<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let Ok(entry) = entry else { return WalkState::Continue };
        if self.run.stopped() {
            return WalkState::Quit;
        }
        let is_file = entry.file_type().is_some_and(|t| t.is_file() || t.is_symlink());
        if !is_file || !is_notebook_path(entry.path()) {
            return WalkState::Continue;
        }
        if self.run.file(entry.path(), &mut self.buf, &mut self.scratch) {
            if self.buf.capacity() > 16 << 20 {
                self.buf = Vec::new();
            }
            WalkState::Continue
        } else {
            WalkState::Quit
        }
    }
}

struct VisitorBuilder<'s>(&'s Run<'s>);

impl<'s> ParallelVisitorBuilder<'s> for VisitorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Visitor { run: self.0, buf: Vec::new(), scratch: String::new() })
    }
}

/// Searches every notebook under `root` (same skip rules as [`crate::walk`],
/// via `query.walk`). Blocking; runs on the walker's own threads and calls
/// `on_hit` concurrently from them as hits are found. Hits of one notebook
/// arrive in document order from a single thread; notebooks interleave in no
/// particular order. Set `cancel` to stop early (checked per notebook and per
/// cell). Errors only for an invalid query.
pub fn search(root: &Path, query: &SearchQuery, cancel: &AtomicBool, on_hit: impl Fn(SearchHit) + Sync) -> Result<SearchStats> {
    let q = query.compile()?;
    let run = Run::new(&q, cancel, &on_hit);
    if !q.empty {
        walker(root, &q.walk).build_parallel().visit(&mut VisitorBuilder(&run));
    }
    Ok(run.stats())
}

/// Like [`search`] over an explicit list of notebooks (e.g. from [`crate::walk`]),
/// on the rayon global pool.
pub fn search_paths(
    paths: &[PathBuf],
    query: &SearchQuery,
    cancel: &AtomicBool,
    on_hit: impl Fn(SearchHit) + Sync,
) -> Result<SearchStats> {
    let q = query.compile()?;
    let run = Run::new(&q, cancel, &on_hit);
    if !q.empty {
        let _ = paths.par_iter().with_max_len(8).try_for_each(|p| {
            let go = SCRATCH.with(|cell| match cell.try_borrow_mut() {
                Ok(mut guard) => {
                    let (buf, scratch) = &mut *guard;
                    let go = run.file(p, buf, scratch);
                    if buf.capacity() > 16 << 20 {
                        *buf = Vec::new();
                    }
                    go
                }
                Err(_) => run.file(p, &mut Vec::new(), &mut String::new()),
            });
            if go { Ok(()) } else { Err(()) }
        });
    }
    Ok(run.stats())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn nb(cells: &[(&str, serde_json::Value)]) -> String {
        let cells: Vec<serde_json::Value> = cells
            .iter()
            .enumerate()
            .map(|(i, (ty, src))| {
                let mut c = serde_json::json!({"cell_type": ty, "id": format!("id{i}"), "metadata": {}, "source": src});
                if *ty == "code" {
                    c["outputs"] = serde_json::json!([
                        {"output_type": "stream", "name": "stdout", "text": ["needle in output\n", "\u{1b}[31mRed\u{1b}[0m needle\n"]},
                        {"output_type": "display_data", "data": {"image/png": "bmVlZGxl", "text/plain": ["<needle figure>"]}, "metadata": {}},
                        {"output_type": "error", "ename": "ValueError", "evalue": "bad", "traceback": ["\u{1b}[0;31mValueError\u{1b}[0m: bad needle"]}
                    ]);
                    c["execution_count"] = serde_json::Value::Null;
                }
                c
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5})).unwrap()
    }

    fn run(q: &SearchQuery, bytes: &str) -> Vec<SearchHit> {
        let mut hits = vec![];
        q.compile().unwrap().search_bytes(Path::new("t.ipynb"), bytes.as_bytes(), |h| {
            hits.push(h);
            true
        })
        .unwrap();
        hits
    }

    fn sample() -> String {
        nb(&[
            ("markdown", serde_json::json!(["# Train the Model\n", "We call train_test_split here.\n"])),
            ("code", serde_json::json!("from sklearn import train_test_split\nX_train, X_test = train_test_split(X)\n    model.fit(X_train)  # Fit!")),
            ("raw", serde_json::json!("fitting \"quoted\" path\\to\\file")),
        ])
    }

    #[test]
    fn literal_case_and_lines() {
        let s = sample();
        let hits = run(&SearchQuery::new("train_test_split"), &s);
        assert_eq!(hits.len(), 3);
        assert_eq!((hits[0].cell_index, hits[0].line, hits[0].cell_type.as_str()), (0, 1, "markdown"));
        assert_eq!((hits[1].cell_index, hits[1].line), (1, 0));
        assert_eq!((hits[2].cell_index, hits[2].line), (1, 1));
        assert_eq!(hits[2].line_text, "X_train, X_test = train_test_split(X)");
        assert_eq!(hits[2].ranges, vec![18..34]);
        assert_eq!(hits[1].cell_id.as_deref(), Some("id1"));

        // case-insensitive by default, sensitive on request
        let ci = run(&SearchQuery::new("FIT"), &s);
        assert_eq!(ci.len(), 2);
        assert_eq!(ci[0].line_text, "model.fit(X_train)  # Fit!");
        assert_eq!(ci[0].line_offset, 4);
        assert_eq!(ci[0].ranges, [6..9, 22..25]);
        let cs = run(&SearchQuery { case_sensitive: true, ..SearchQuery::new("Fit") }, &s);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].ranges, vec![22..25]);
    }

    #[test]
    fn whole_word_and_regex() {
        let s = sample();
        let ww = run(&SearchQuery { whole_word: true, ..SearchQuery::new("fit") }, &s);
        assert_eq!(ww.len(), 1, "{ww:?}");
        assert_eq!(ww[0].ranges, [6..9, 22..25]);
        // `fitting` isn't a whole-word `fit`
        assert!(ww.iter().all(|h| h.cell_type != "raw"));
        // Non-word edges don't need a boundary.
        let dot = run(&SearchQuery { whole_word: true, ..SearchQuery::new(".fit(") }, &s);
        assert_eq!(dot.len(), 1);

        let re = run(&SearchQuery { regex: true, ..SearchQuery::new(r"^X_\w+") }, &s);
        assert_eq!(re.len(), 1);
        assert_eq!(re[0].ranges, vec![0..7]);
        let re_ww = run(&SearchQuery { regex: true, whole_word: true, ..SearchQuery::new(r"train|test") }, &s);
        assert_eq!(re_ww.iter().map(|h| h.ranges.len()).sum::<usize>(), 1, "{re_ww:?}"); // "Train" in the heading
        assert!(SearchQuery { regex: true, ..SearchQuery::new("(unclosed") }.compile().is_err());
        // Empty-matching regexes produce no hits.
        assert!(run(&SearchQuery { regex: true, ..SearchQuery::new("x*") }, &s).iter().all(|h| !h.ranges.is_empty()));
    }

    #[test]
    fn escaped_json_text() {
        let s = sample();
        // Needles containing characters JSON escapes bypass the raw prefilter.
        let q = run(&SearchQuery::new("\"quoted\" path\\to"), &s);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].cell_type, "raw");
        // Unicode escapes in the file.
        let esc = r#"{"cells": [{"cell_type": "code", "source": "caf\u00e9 = 1\n\u4e2d\u6587", "outputs": [], "metadata": {}}], "metadata": {}, "nbformat": 4, "nbformat_minor": 2}"#;
        assert_eq!(run(&SearchQuery::new("café"), esc).len(), 1);
        assert_eq!(run(&SearchQuery::new("中文"), esc)[0].line, 1);
    }

    #[test]
    fn single_string_and_line_array_sources_agree() {
        let a = nb(&[("code", serde_json::json!(["a = 1\n", "b = needle\n", "needle()"]))]);
        let b = nb(&[("code", serde_json::json!("a = 1\nb = needle\nneedle()"))]);
        let ha = run(&SearchQuery::new("needle"), &a);
        let hb = run(&SearchQuery::new("needle"), &b);
        assert_eq!(ha, hb);
        assert_eq!(ha.iter().map(|h| h.line).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn outputs_only_when_asked() {
        let s = nb(&[("code", serde_json::json!("x = 1"))]);
        assert!(run(&SearchQuery::new("needle"), &s).is_empty());
        let hits = run(&SearchQuery { include_outputs: true, ..SearchQuery::new("needle") }, &s);
        let locs: Vec<_> = hits.iter().map(|h| (h.location, h.line, h.line_text.as_str())).collect();
        assert_eq!(
            locs,
            [
                (HitLocation::Output { index: 0 }, 0, "needle in output"),
                (HitLocation::Output { index: 0 }, 1, "Red needle"),
                (HitLocation::Output { index: 1 }, 0, "<needle figure>"),
                (HitLocation::Output { index: 2 }, 0, "ValueError: bad needle"),
            ]
        );
    }

    #[test]
    fn long_lines_are_windowed() {
        let long = format!("{}needle{}", "a".repeat(1000), "b".repeat(1000));
        let s = nb(&[("code", serde_json::json!(long))]);
        let h = &run(&SearchQuery::new("needle"), &s)[0];
        assert!(h.line_text.len() <= MAX_LINE + 8);
        assert_eq!(&h.line_text[h.ranges[0].clone()], "needle");
        assert_eq!(h.line_offset + h.ranges[0].start, 1000);
    }

    #[test]
    fn folder_search_cancel_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let body = sample();
        for i in 0..40 {
            let p = dir.path().join(format!("d{}/nb{i}.ipynb", i % 4));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, &body).unwrap();
        }
        std::fs::create_dir_all(dir.path().join(".ipynb_checkpoints")).unwrap();
        std::fs::write(dir.path().join(".ipynb_checkpoints/nb-checkpoint.ipynb"), &body).unwrap();
        std::fs::write(dir.path().join("broken.ipynb"), "{\"cells\": [").unwrap();
        let cancel = AtomicBool::new(false);
        let hits = Mutex::new(Vec::new());
        let stats = search(dir.path(), &SearchQuery::new("train_test_split"), &cancel, |h| hits.lock().unwrap().push(h)).unwrap();
        assert_eq!(stats.hits, 120);
        assert_eq!(hits.lock().unwrap().len(), 120);
        assert_eq!((stats.files_matched, stats.files_scanned, stats.errors), (40, 41, 0)); // broken.ipynb is rejected by the prefilter
        assert!(stats.first_hit.is_some() && !stats.cancelled && !stats.truncated);

        // A broken notebook that passes the prefilter counts as an error.
        std::fs::write(dir.path().join("broken.ipynb"), "{\"cells\": [\"train_test_split").unwrap();
        let stats = search(dir.path(), &SearchQuery::new("train_test_split"), &cancel, |_| {}).unwrap();
        assert_eq!(stats.errors, 1);

        // max_hits
        let n = AtomicUsize::new(0);
        let stats = search(dir.path(), &SearchQuery { max_hits: Some(7), ..SearchQuery::new("train") }, &cancel, |_| {
            n.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(n.load(Ordering::Relaxed), 7);
        assert!(stats.truncated && stats.hits == 7);

        // Cancel from inside the callback stops promptly.
        let n = AtomicUsize::new(0);
        let stats = search(dir.path(), &SearchQuery { max_hits: None, ..SearchQuery::new("train") }, &cancel, |_| {
            if n.fetch_add(1, Ordering::Relaxed) == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
        assert!(stats.cancelled);
        assert!(n.load(Ordering::Relaxed) < 40, "{}", n.load(Ordering::Relaxed));
        // Pre-cancelled: nothing at all.
        let stats = search(dir.path(), &SearchQuery::new("train"), &cancel, |_| panic!("hit after cancel")).unwrap();
        assert_eq!(stats.hits, 0);

        // search_paths agrees with search.
        cancel.store(false, Ordering::Relaxed);
        let paths: Vec<PathBuf> = crate::walk(dir.path(), &WalkOptions::default()).into_iter().filter(|e| e.is_notebook()).map(|e| e.path).collect();
        let stats = search_paths(&paths, &SearchQuery::new("train_test_split"), &cancel, |_| {}).unwrap();
        assert_eq!(stats.hits, 120);
        // Empty query: no work.
        assert_eq!(search(dir.path(), &SearchQuery::new(""), &cancel, |_| panic!()).unwrap().files_scanned, 0);
    }

    #[test]
    fn ansi_strip() {
        assert_eq!(strip_ansi("\x1b[1;31mErr\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x1b\\".into()), "Err link");
    }
}
