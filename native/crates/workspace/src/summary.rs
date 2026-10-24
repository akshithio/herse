//! Notebook summaries (cell counts, kernel, title) without parsing outputs.

use crate::json::decode_multiline;
use crate::nbscan::{CellKind, NbMeta, RawCell, scan_notebook};
use anyhow::{Context, Result, anyhow};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotebookSummary {
    pub cells: u32,
    pub code: u32,
    /// Includes nbformat-3 `heading` cells (they become markdown in v4).
    pub markdown: u32,
    pub raw: u32,
    /// `metadata.kernelspec.display_name`, else `kernelspec.name`.
    pub kernel: Option<String>,
    /// `metadata.kernelspec.name` (what to match against installed kernelspecs).
    pub kernel_name: Option<String>,
    /// `metadata.language_info.name`, else `kernelspec.language`.
    pub language: Option<String>,
    /// Any code cell has at least one stored output.
    pub has_outputs: bool,
    /// Text of the first markdown heading (ATX `#`, setext, or `<h1>`-`<h6>`).
    pub title: Option<String>,
    pub nbformat: Option<u32>,
    pub nbformat_minor: Option<u32>,
}

/// Summarizes the notebook at `path`. Fast: outputs are skipped at the byte
/// level, only markdown sources up to the first heading are decoded.
pub fn summarize(path: &Path) -> Result<NotebookSummary> {
    crate::io::with_file_bytes(path, summarize_bytes)
        .with_context(|| format!("reading {}", path.display()))?
        .with_context(|| format!("summarizing {}", path.display()))
}

/// [`summarize`] for in-memory notebook JSON.
pub fn summarize_bytes(bytes: &[u8]) -> Result<NotebookSummary> {
    match scan_summary(bytes) {
        Ok(s) => Ok(s),
        Err(scan_err) => match serde_impl::summarize_bytes_serde(bytes) {
            Ok(s) => {
                log::warn!("notebook scanner failed ({scan_err}) but serde parsed it; using serde result");
                Ok(s)
            }
            Err(e) => Err(anyhow!(e).context("invalid notebook JSON")),
        },
    }
}

/// Summarizes many notebooks in parallel on the rayon global pool.
/// `on_done(i, result)` is called once per path (with `i` its index in
/// `paths`) from rayon worker threads, in completion order, concurrently.
/// Returns when all are done.
pub fn summarize_many(paths: &[PathBuf], on_done: impl Fn(usize, Result<NotebookSummary>) + Sync) {
    paths.par_iter().enumerate().with_max_len(4).for_each(|(i, p)| on_done(i, summarize(p)));
}

pub(crate) fn scan_summary(bytes: &[u8]) -> Result<NotebookSummary, crate::json::ScanError> {
    let mut s = NotebookSummary::default();
    let mut buf = String::new();
    let meta = scan_notebook(bytes, &mut |cell: RawCell<'_>| {
        s.cells += 1;
        match cell.kind {
            CellKind::Code => {
                s.code += 1;
                s.has_outputs |= cell.outputs_nonempty;
            }
            CellKind::Markdown | CellKind::Heading => s.markdown += 1,
            CellKind::Raw => s.raw += 1,
            CellKind::Other => {}
        }
        if s.title.is_none()
            && matches!(cell.kind, CellKind::Markdown | CellKind::Heading)
            && let Some(src) = cell.source
        {
            buf.clear();
            decode_multiline(src, &mut buf);
            s.title = if cell.kind == CellKind::Heading { heading_cell_title(&buf) } else { markdown_title(&buf) };
        }
        true
    })?;
    apply_meta(&mut s, meta);
    Ok(s)
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

pub(crate) fn apply_meta(s: &mut NotebookSummary, m: NbMeta) {
    let name = non_empty(m.kernel_name);
    s.kernel = non_empty(m.kernel_display_name).or_else(|| name.clone());
    s.kernel_name = name;
    s.language = non_empty(m.language_info_name).or_else(|| non_empty(m.kernel_language));
    s.nbformat = m.nbformat;
    s.nbformat_minor = m.nbformat_minor;
}

const MAX_TITLE_CHARS: usize = 200;

fn heading_cell_title(src: &str) -> Option<String> {
    let line = src.lines().find(|l| !l.trim().is_empty())?;
    clean_inline(line.trim().trim_start_matches('#'))
}

/// First heading of a markdown document: ATX (`# Title`), setext
/// (`Title\n===`) or an HTML `<h1>`..`<h6>` line. Headings inside fenced
/// code blocks are ignored. Inline markup (emphasis, code, links, tags) is
/// stripped.
pub fn markdown_title(src: &str) -> Option<String> {
    let mut fence: Option<(u8, usize)> = None;
    let mut prev_para: Option<&str> = None;
    for line in src.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let t = line.trim_start_matches(' ');
        if let Some((ch, len)) = fence {
            let run = t.bytes().take_while(|&b| b == ch).count();
            if indent < 4 && run >= len && t[run..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if indent >= 4 {
            prev_para = None;
            continue;
        }
        let first = t.bytes().next();
        if let Some(ch @ (b'`' | b'~')) = first {
            let run = t.bytes().take_while(|&b| b == ch).count();
            if run >= 3 {
                fence = Some((ch, run));
                prev_para = None;
                continue;
            }
        }
        if first == Some(b'#') {
            let hashes = t.bytes().take_while(|&b| b == b'#').count();
            let rest = &t[hashes..];
            if hashes <= 6 && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
                let mut text = rest.trim();
                // Optional closing sequence: " ##"
                let stripped = text.trim_end_matches('#');
                if stripped.len() < text.len() && (stripped.is_empty() || stripped.ends_with([' ', '\t'])) {
                    text = stripped.trim_end();
                }
                if let Some(title) = clean_inline(text) {
                    return Some(title);
                }
                prev_para = None;
                continue;
            }
        }
        let trimmed = t.trim_end();
        if let Some(p) = prev_para
            && !trimmed.is_empty()
            && (trimmed.bytes().all(|b| b == b'=') || trimmed.bytes().all(|b| b == b'-'))
            && let Some(title) = clean_inline(p)
        {
            return Some(title);
        }
        if let Some(title) = html_heading(trimmed) {
            return Some(title);
        }
        prev_para = if trimmed.is_empty() || is_block_marker(trimmed) { None } else { Some(trimmed) };
    }
    None
}

fn is_block_marker(t: &str) -> bool {
    t.starts_with(['>', '|', '<', '-', '*', '+']) || t.bytes().take_while(u8::is_ascii_digit).count() > 0 && t.contains(". ")
}

fn html_heading(t: &str) -> Option<String> {
    let b = t.as_bytes();
    if b.len() < 4 || b[0] != b'<' || !b[1].eq_ignore_ascii_case(&b'h') || !(b'1'..=b'6').contains(&b[2]) {
        return None;
    }
    if !(b[3] == b'>' || b[3] == b' ' || b[3] == b'\t') {
        return None;
    }
    let open_end = t.find('>')? + 1;
    let lower = t.to_ascii_lowercase();
    let close = lower[open_end..].find("</h").map(|p| p + open_end).unwrap_or(t.len());
    clean_inline(&t[open_end..close])
}

/// Strips inline markdown/HTML for display.
fn clean_inline(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // Drop an HTML tag; keep a lone '<'.
                let rest: String = chars.clone().take(200).collect();
                if let Some(end) = rest.find('>')
                    && rest.chars().next().is_some_and(|n| n.is_ascii_alphabetic() || n == '/' || n == '!')
                {
                    for _ in 0..rest[..=end].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push('<');
                }
            }
            '[' => {} // link text kept, brackets dropped
            ']' if chars.peek() == Some(&'(') => {
                // skip "(url)"
                let mut depth = 0;
                for n in chars.by_ref() {
                    if n == '(' {
                        depth += 1;
                    } else if n == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            ']' => {}
            '*' | '`' => {}
            '_' if out.is_empty() || out.ends_with(' ') || chars.peek().is_none_or(|n| *n == ' ' || *n == '_') => {}
            '\\' if chars.peek().is_some_and(|n| n.is_ascii_punctuation()) => {
                out.push(chars.next().unwrap());
            }
            c if c.is_whitespace() => {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }
    let t = out.trim();
    if t.is_empty() {
        return None;
    }
    let mut t: String = t.to_string();
    if t.chars().count() > MAX_TITLE_CHARS {
        t = t.chars().take(MAX_TITLE_CHARS).collect::<String>() + "…";
    }
    for (from, to) in [("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&nbsp;", " "), ("&quot;", "\"")] {
        if t.contains(from) {
            t = t.replace(from, to);
        }
    }
    Some(t)
}

/// A `serde_json` based summarizer that skips outputs with `IgnoredAny` and
/// borrows sources as `RawValue`. Used as the fallback for the byte scanner
/// and as the baseline in `examples/bench.rs`.
#[doc(hidden)]
pub mod serde_impl {
    use super::*;
    use serde::de::{IgnoredAny, SeqAccess, Visitor};
    use serde_json::value::RawValue;
    use serde_json::{Map, Value};

    #[derive(Deserialize)]
    struct SNb<'a> {
        #[serde(default, borrow)]
        cells: Vec<SCell<'a>>,
        #[serde(default, borrow)]
        worksheets: Vec<SWorksheet<'a>>,
        #[serde(default)]
        metadata: Option<SMeta>,
        #[serde(default)]
        nbformat: Option<u32>,
        #[serde(default)]
        nbformat_minor: Option<u32>,
    }

    #[derive(Deserialize)]
    struct SWorksheet<'a> {
        #[serde(default, borrow)]
        cells: Vec<SCell<'a>>,
    }

    #[derive(Deserialize)]
    struct SCell<'a> {
        #[serde(default)]
        cell_type: String,
        #[serde(default, borrow)]
        source: Option<&'a RawValue>,
        #[serde(default, borrow)]
        input: Option<&'a RawValue>,
        #[serde(default)]
        outputs: NonEmpty,
    }

    #[derive(Deserialize, Default)]
    struct SMeta {
        #[serde(default)]
        kernelspec: Option<Value>,
        #[serde(default)]
        language_info: Option<Value>,
    }

    /// Whether a sequence has any element, without materializing elements.
    #[derive(Default)]
    struct NonEmpty(bool);

    impl<'de> Deserialize<'de> for NonEmpty {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = NonEmpty;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a sequence")
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<NonEmpty, A::Error> {
                    let mut any = false;
                    while seq.next_element::<IgnoredAny>()?.is_some() {
                        any = true;
                    }
                    Ok(NonEmpty(any))
                }
                fn visit_unit<E>(self) -> Result<NonEmpty, E> {
                    Ok(NonEmpty(false))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<NonEmpty, A::Error> {
                    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                    Ok(NonEmpty(false))
                }
            }
            d.deserialize_any(V)
        }
    }

    fn str_field(v: Option<&Value>, k: &str) -> Option<String> {
        v?.get(k)?.as_str().map(str::to_string)
    }

    pub fn summarize_bytes_serde(bytes: &[u8]) -> Result<NotebookSummary, serde_json::Error> {
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let nb: SNb = serde_json::from_slice(bytes)?;
        let mut s = NotebookSummary::default();
        let mut buf = String::new();
        let cells = nb.cells.iter().chain(nb.worksheets.iter().flat_map(|w| w.cells.iter()));
        for c in cells {
            s.cells += 1;
            let kind = match c.cell_type.as_str() {
                "code" => CellKind::Code,
                "markdown" => CellKind::Markdown,
                "raw" => CellKind::Raw,
                "heading" => CellKind::Heading,
                _ => CellKind::Other,
            };
            match kind {
                CellKind::Code => {
                    s.code += 1;
                    s.has_outputs |= c.outputs.0;
                }
                CellKind::Markdown | CellKind::Heading => s.markdown += 1,
                CellKind::Raw => s.raw += 1,
                CellKind::Other => {}
            }
            if s.title.is_none()
                && matches!(kind, CellKind::Markdown | CellKind::Heading)
                && let Some(src) = c.source.or(c.input)
            {
                buf.clear();
                decode_multiline(src.get().as_bytes(), &mut buf);
                s.title = if kind == CellKind::Heading { heading_cell_title(&buf) } else { markdown_title(&buf) };
            }
        }
        let meta = nb.metadata.unwrap_or_default();
        let ks = meta.kernelspec.as_ref();
        apply_meta(
            &mut s,
            NbMeta {
                nbformat: nb.nbformat,
                nbformat_minor: nb.nbformat_minor,
                kernel_display_name: str_field(ks, "display_name"),
                kernel_name: str_field(ks, "name"),
                kernel_language: str_field(ks, "language"),
                language_info_name: str_field(meta.language_info.as_ref(), "name"),
            },
        );
        Ok(s)
    }

    /// Reference summary from a fully materialized `serde_json::Value` (the
    /// slow, obviously-correct path; used by tests and the benchmark).
    pub fn summarize_value(v: &Value) -> NotebookSummary {
        let empty = Map::new();
        let obj = v.as_object().unwrap_or(&empty);
        let mut s = NotebookSummary::default();
        let mut cells: Vec<&Value> = obj.get("cells").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        if let Some(ws) = obj.get("worksheets").and_then(Value::as_array) {
            for w in ws {
                if let Some(a) = w.get("cells").and_then(Value::as_array) {
                    cells.extend(a.iter());
                }
            }
        }
        let join = |v: Option<&Value>| -> String {
            match v {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                _ => String::new(),
            }
        };
        for c in cells {
            s.cells += 1;
            let ty = c.get("cell_type").and_then(Value::as_str).unwrap_or("");
            match ty {
                "code" => {
                    s.code += 1;
                    if c.get("outputs").and_then(Value::as_array).is_some_and(|o| !o.is_empty()) {
                        s.has_outputs = true;
                    }
                }
                "markdown" | "heading" => s.markdown += 1,
                "raw" => s.raw += 1,
                _ => {}
            }
            if s.title.is_none() && (ty == "markdown" || ty == "heading") {
                let src = join(c.get("source").or_else(|| c.get("input")));
                s.title = if ty == "heading" { heading_cell_title(&src) } else { markdown_title(&src) };
            }
        }
        let meta = obj.get("metadata");
        let ks = meta.and_then(|m| m.get("kernelspec"));
        apply_meta(
            &mut s,
            NbMeta {
                nbformat: obj.get("nbformat").and_then(Value::as_u64).map(|n| n as u32),
                nbformat_minor: obj.get("nbformat_minor").and_then(Value::as_u64).map(|n| n as u32),
                kernel_display_name: str_field(ks, "display_name"),
                kernel_name: str_field(ks, "name"),
                kernel_language: str_field(ks, "language"),
                language_info_name: str_field(meta.and_then(|m| m.get("language_info")), "name"),
            },
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::serde_impl::*;
    use super::*;

    const NB: &str = r###"{
 "cells": [
  {"cell_type": "markdown", "id": "a", "metadata": {}, "source": ["Intro text\n", "\n", "```python\n", "# not a title\n", "```\n", "## The *Real* Title ##\n"]},
  {"cell_type": "code", "execution_count": 1, "id": "b", "metadata": {}, "outputs": [
     {"output_type": "display_data", "data": {"image/png": "iVBORw0KGgo\"]}{[", "text/plain": ["<Figure>"]}, "metadata": {}}
  ], "source": "import numpy as np\nnp.arange(3)"},
  {"cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": []},
  {"cell_type": "raw", "metadata": {}, "source": "raw"},
  {"metadata": {}, "source": "# late", "cell_type": "markdown"}
 ],
 "metadata": {
  "widgets": {"state": {"x": [1, 2, {"y": "]"}]}},
  "kernelspec": {"display_name": "Python 3 (ipykernel)", "language": "python", "name": "python3"},
  "language_info": {"codemirror_mode": {"name": "ipython", "version": 3}, "name": "python", "version": "3.11.4"}
 },
 "nbformat": 4,
 "nbformat_minor": 5
}"###;

    #[test]
    fn scanner_matches_reference() {
        let s = summarize_bytes(NB.as_bytes()).unwrap();
        assert_eq!(
            s,
            NotebookSummary {
                cells: 5,
                code: 2,
                markdown: 2,
                raw: 1,
                kernel: Some("Python 3 (ipykernel)".into()),
                kernel_name: Some("python3".into()),
                language: Some("python".into()),
                has_outputs: true,
                title: Some("The Real Title".into()),
                nbformat: Some(4),
                nbformat_minor: Some(5),
            }
        );
        assert_eq!(scan_summary(NB.as_bytes()).unwrap(), s);
        assert_eq!(summarize_bytes_serde(NB.as_bytes()).unwrap(), s);
        assert_eq!(summarize_value(&serde_json::from_str(NB).unwrap()), s);
    }

    #[test]
    fn nbformat3() {
        let v3 = r##"{"metadata": {"name": "old"}, "nbformat": 3, "nbformat_minor": 0, "worksheets": [{"cells": [
            {"cell_type": "heading", "level": 1, "metadata": {}, "source": ["Old *Title*"]},
            {"cell_type": "code", "collapsed": false, "input": ["print(1)"], "language": "python", "metadata": {}, "outputs": [{"output_type": "stream", "stream": "stdout", "text": ["1\n"]}], "prompt_number": 1},
            {"cell_type": "markdown", "metadata": {}, "source": "# md"}
        ], "metadata": {}}]}"##;
        let s = summarize_bytes(v3.as_bytes()).unwrap();
        assert_eq!((s.cells, s.code, s.markdown, s.raw, s.has_outputs), (3, 1, 2, 0, true));
        assert_eq!(s.title.as_deref(), Some("Old Title"));
        assert_eq!(s.nbformat, Some(3));
        assert_eq!(s, summarize_value(&serde_json::from_str(v3).unwrap()));
    }

    #[test]
    fn invalid_json_errors() {
        assert!(summarize_bytes(b"").is_err());
        assert!(summarize_bytes(br#"{"cells": [{"cell_type": "code""#).is_err());
        assert!(summarize_bytes(b"not json").is_err());
        // Empty object is a (degenerate) notebook.
        assert_eq!(summarize_bytes(b"{}").unwrap(), NotebookSummary::default());
    }

    #[test]
    fn titles() {
        assert_eq!(markdown_title("# Hello\n## World").as_deref(), Some("Hello"));
        assert_eq!(markdown_title("#NoSpace\n### Third ###").as_deref(), Some("Third"));
        assert_eq!(markdown_title("Setext Title\n=====\n# later").as_deref(), Some("Setext Title"));
        assert_eq!(markdown_title("Sub\n---").as_deref(), Some("Sub"));
        assert_eq!(markdown_title("- item\n---\ntext").as_deref(), None);
        assert_eq!(markdown_title("<h1 align=\"center\">Big <b>Bold</b></h1>").as_deref(), Some("Big Bold"));
        assert_eq!(markdown_title("~~~\n# no\n~~~\n# [Linked](http://x.y) `code` title").as_deref(), Some("Linked code title"));
        assert_eq!(markdown_title("    # indented code").as_deref(), None);
        assert_eq!(markdown_title("#\n# Real").as_deref(), Some("Real"));
        assert_eq!(markdown_title("# __init__ &amp; friends").as_deref(), Some("init & friends"));
        assert_eq!(markdown_title("plain text only"), None);
    }
}
