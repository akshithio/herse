//! Shared helpers: fixture extraction and a stable text rendering of a
//! [`Table`] for golden files.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use herse_table::{Align, Cell, CellKind, Table};

pub fn native_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Every `text/html` output containing `<table` in a notebook, labelled
/// `cell{i}_{j}` (cell index, output index).
pub fn notebook_tables(path: &Path) -> Vec<(String, String)> {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let nb: serde_json::Value = serde_json::from_str(&src).expect("notebook JSON");
    let mut out = Vec::new();
    for (i, cell) in nb["cells"].as_array().into_iter().flatten().enumerate() {
        for (j, o) in cell["outputs"].as_array().into_iter().flatten().enumerate() {
            let html: String = match &o["data"]["text/html"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
                _ => continue,
            };
            if html.contains("<table") {
                out.push((format!("cell{i:02}_{j}"), html));
            }
        }
    }
    out
}

/// The table output of a given notebook cell (first output with a table).
pub fn fixture(notebook: &str, cell: usize) -> String {
    let path = native_root().join("fixtures").join(notebook);
    notebook_tables(&path)
        .into_iter()
        .find(|(l, _)| l.starts_with(&format!("cell{cell:02}_")))
        .unwrap_or_else(|| panic!("{notebook}: no table in cell {cell}"))
        .1
}

pub fn sample(name: &str) -> String {
    std::fs::read_to_string(data_dir().join(format!("{name}.html"))).expect("sample")
}

fn hex(c: herse_table::Rgba) -> String {
    if c.a == 255 { format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b) } else { format!("#{:08x}", c.to_u32()) }
}

fn cell(c: &Cell) -> String {
    let mut s = String::new();
    if let Some((r, k)) = c.span_origin {
        let _ = write!(s, "^{r},{k}");
    } else {
        s.push(match c.align {
            Align::Left => '<',
            Align::Right => '>',
            Align::Center => '=',
        });
        s.push(match c.kind {
            CellKind::Empty => '_',
            CellKind::Text => 't',
            CellKind::Number => 'n',
            CellKind::Bool => 'b',
            CellKind::Null => 'z',
            CellKind::Ellipsis => 'e',
        });
        let _ = write!(s, "{:?}", c.text);
        if c.colspan != 1 {
            let _ = write!(s, " cs{}", c.colspan);
        }
        if c.rowspan != 1 {
            let _ = write!(s, " rs{}", c.rowspan);
        }
    }
    if c.is_header {
        s.push_str(" H");
    }
    if c.is_index {
        s.push_str(" I");
    }
    let st = &c.style;
    if let Some(bg) = st.bg {
        let _ = write!(s, " bg{}", hex(bg));
    }
    if let Some(fg) = st.fg {
        let _ = write!(s, " fg{}", hex(fg));
    }
    if st.bold {
        s.push_str(" bold");
    }
    if st.italic {
        s.push_str(" italic");
    }
    if let Some(b) = st.bar {
        let _ = write!(s, " bar{:.3}-{:.3}{}", b.start, b.end, hex(b.color));
    }
    s
}

/// Stable, reviewable rendering of everything in a [`Table`].
pub fn render(t: &Table) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "flavor={:?} columns={} index_columns={} standalone={} header_rows={} body_rows={} dropped={}",
        t.flavor,
        t.columns,
        t.index_columns,
        t.standalone,
        t.header.len(),
        t.body.len(),
        t.dropped_rows
    );
    let _ = writeln!(s, "caption={:?}", t.caption);
    let _ = writeln!(s, "footer={:?}", t.footer);
    let _ = writeln!(s, "dtypes={:?}", t.dtypes);
    let _ = writeln!(s, "widths={:?}", t.column_widths_chars());
    let ell_rows: Vec<usize> = (0..t.body.len()).filter(|&r| t.is_ellipsis_row(r)).collect();
    let ell_cols: Vec<usize> = (0..t.columns).filter(|&c| t.is_ellipsis_column(c)).collect();
    let _ = writeln!(s, "ellipsis_rows={ell_rows:?} ellipsis_cols={ell_cols:?}");
    for (name, rows) in [("H", &t.header), ("B", &t.body)] {
        for (r, row) in rows.iter().enumerate() {
            let cells: Vec<String> = row.iter().map(cell).collect();
            let _ = writeln!(s, "{name}{r}: {}", cells.join(" | "));
        }
    }
    s
}

/// Compare against `tests/golden/{name}.txt`; `UPDATE_GOLDEN=1` rewrites it.
pub fn check_golden(name: &str, actual: &str) -> Result<(), String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return Ok(());
    }
    let expected = std::fs::read_to_string(&path)
        .map_err(|e| format!("{name}: missing golden {} ({e}); run with UPDATE_GOLDEN=1", path.display()))?;
    if expected == actual {
        return Ok(());
    }
    let (n, (e, a)) = expected
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (e, a))| e != a)
        .unwrap_or((expected.lines().count().min(actual.lines().count()), ("<eof>", "<eof>")));
    Err(format!("{name}: golden mismatch at line {}\n  expected: {e}\n  actual:   {a}", n + 1))
}
