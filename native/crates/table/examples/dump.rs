//! Print the parsed grid of HTML files (or every table output of notebooks).
//!
//! `cargo run -p herse-table --example dump -- file.html notebook.ipynb ...`
use herse_table::{Align, Cell, CellKind, Table};

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(src) = std::fs::read_to_string(&path) else {
            eprintln!("{path}: unreadable");
            continue;
        };
        if path.ends_with(".ipynb") {
            for (label, html) in notebook_tables(&src) {
                println!("=== {path} {label}");
                show(&html);
            }
        } else {
            println!("=== {path}");
            show(&src);
        }
    }
}

fn notebook_tables(src: &str) -> Vec<(String, String)> {
    let Ok(nb) = serde_json::from_str::<serde_json::Value>(src) else { return vec![] };
    let mut out = Vec::new();
    for (i, cell) in nb["cells"].as_array().into_iter().flatten().enumerate() {
        for (j, o) in cell["outputs"].as_array().into_iter().flatten().enumerate() {
            let html = match &o["data"]["text/html"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
                _ => continue,
            };
            if html.contains("<table") {
                out.push((format!("cell {i} output {j}"), html));
            }
        }
    }
    out
}

fn show(html: &str) {
    let Some(t) = herse_table::parse(html) else {
        println!("(no table)");
        return;
    };
    print!("{}", render(&t));
}

fn render(t: &Table) -> String {
    let mut s = format!(
        "{:?} {}x{} index_columns={} standalone={} caption={:?} footer={:?} dtypes={:?}\n",
        t.flavor,
        t.header.len() + t.body.len(),
        t.columns,
        t.index_columns,
        t.standalone,
        t.caption,
        t.footer,
        t.dtypes
    );
    let w: Vec<usize> = t.column_widths_chars_capped(14).iter().map(|&w| w as usize).collect();
    let line = |row: &[Cell], s: &mut String| {
        for (c, cell) in row.iter().enumerate() {
            let text = if cell.is_covered() { "·".to_string() } else { cell.text.replace('\n', "⏎") };
            let text: String = text.chars().take(w[c].max(1)).collect();
            let mark = match cell.kind {
                CellKind::Null => "∅",
                CellKind::Ellipsis => "…",
                _ if cell.colspan > 1 || cell.rowspan > 1 => "+",
                _ => " ",
            };
            let pad = w[c].max(1);
            match cell.align {
                Align::Right => s.push_str(&format!("{mark}{text:>pad$}|")),
                Align::Center => s.push_str(&format!("{mark}{text:^pad$}|")),
                Align::Left => s.push_str(&format!("{mark}{text:<pad$}|")),
            }
        }
        s.push('\n');
    };
    for r in &t.header {
        line(r, &mut s);
    }
    s.push_str(&"-".repeat(w.iter().map(|w| w.max(&1) + 2).sum()));
    s.push('\n');
    for r in t.body.iter().take(12) {
        line(r, &mut s);
    }
    if t.body.len() > 12 {
        s.push_str(&format!("... {} more rows\n", t.body.len() - 12));
    }
    let styled: Vec<String> = t
        .header
        .iter()
        .chain(&t.body)
        .flatten()
        .filter(|c| !c.style.is_default() && !c.is_covered())
        .take(4)
        .map(|c| format!("{:?}={:?}", c.text, c.style))
        .collect();
    if !styled.is_empty() {
        s.push_str(&format!("styles: {}\n", styled.join("; ")));
    }
    s
}
