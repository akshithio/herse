//! A frame (or the rows a view shows) as CSV, TSV or a Markdown table.

use std::fmt::Write as _;

use crate::Frame;

/// The rows to write: `order` (a view's rows) or every row, at most `limit`.
fn rows<'a>(frame: &'a Frame, order: Option<&'a [u32]>, limit: usize) -> Box<dyn Iterator<Item = usize> + 'a> {
    match order {
        Some(order) => Box::new(order.iter().take(limit).map(|&r| r as usize)),
        None => Box::new((0..frame.rows()).take(limit)),
    }
}

/// RFC 4180 CSV with a header row. Missing values are empty.
pub fn to_csv(frame: &Frame, order: Option<&[u32]>, limit: usize) -> String {
    let mut out = String::new();
    let quote = |s: &str, out: &mut String| {
        if s.contains([',', '"', '\n', '\r']) {
            out.push('"');
            out.push_str(&s.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(s);
        }
    };
    for (i, c) in frame.columns().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        quote(&c.name, &mut out);
    }
    out.push('\n');
    for r in rows(frame, order, limit) {
        for c in 0..frame.columns().len() {
            if c > 0 {
                out.push(',');
            }
            if !frame.is_null(r, c) {
                quote(frame.text(r, c), &mut out);
            }
        }
        out.push('\n');
    }
    out
}

/// Tab-separated, for pasting into a spreadsheet: tabs and line breaks in
/// values become spaces.
pub fn to_tsv(frame: &Frame, order: Option<&[u32]>, limit: usize) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = frame.columns().iter().map(|c| clean(&c.name)).collect::<Vec<_>>().join("\t");
    out.push('\n');
    for r in rows(frame, order, limit) {
        let line: Vec<String> = (0..frame.columns().len())
            .map(|c| if frame.is_null(r, c) { String::new() } else { clean(frame.text(r, c)) })
            .collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    out
}

/// A GitHub Markdown table; number columns right-aligned.
pub fn to_markdown(frame: &Frame, order: Option<&[u32]>, limit: usize) -> String {
    let cell = |s: &str| s.replace('|', "\\|").replace('\n', "<br>");
    let n = frame.columns().len();
    let mut out = String::from("|");
    for c in frame.columns() {
        let _ = write!(out, " {} |", cell(&c.name));
    }
    out.push_str("\n|");
    for c in 0..n {
        out.push_str(if frame.kind(c).is_numeric() { " ---: |" } else { " --- |" });
    }
    out.push('\n');
    for r in rows(frame, order, limit) {
        out.push('|');
        for c in 0..n {
            let _ = write!(out, " {} |", cell(frame.text(r, c)));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkBuilder, Column, Value};

    #[test]
    fn exports() {
        let mut f = Frame::default();
        f.set_columns(vec![Column::new("name"), Column::new("n")]);
        let mut b = ChunkBuilder::new(2);
        b.push_row([Value::Text("a, \"b\""), Value::Text("1")]);
        b.push_row([Value::Text("x|y"), Value::Text("NaN")]);
        f.push_chunk(b.finish());
        assert_eq!(to_csv(&f, None, usize::MAX), "name,n\n\"a, \"\"b\"\"\",1\nx|y,\n");
        assert_eq!(to_csv(&f, Some(&[1]), usize::MAX), "name,n\nx|y,\n");
        assert_eq!(to_tsv(&f, None, 1), "name\tn\na, \"b\"\t1\n");
        assert_eq!(to_markdown(&f, None, usize::MAX), "| name | n |\n| --- | ---: |\n| a, \"b\" | 1 |\n| x\\|y | NaN |\n");
    }
}
