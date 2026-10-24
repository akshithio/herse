//! What an agent can do with a table open in Herse (a data file, or a
//! DataFrame opened full size; see [`crate::frame_view`]): read it as Herse
//! read it. Nothing here changes the table. The work runs off the main
//! thread on the frame as read so far, so a big file stays smooth.
//!
//! Rows and columns are numbered from 1, as the viewer numbers rows.

use std::collections::HashMap;

use gpui_kit::{AsyncWindowContext, WeakEntity};
use herse_data::{Filter, Frame, Kind};
use serde_json::{Value, json};

use crate::frame_view::FrameView;

/// The longest reply a tool gives: an agent reads every character of it.
const BUDGET: usize = 24_000;
/// Rows a reply shows at most.
const MAX_ROWS: usize = 200;

/// Told to the agent when it connects.
pub const INSTRUCTIONS: &str = "Herse is showing the user a table (a data file, or a DataFrame from a \
notebook) in its table viewer. These tools read that table as Herse read it, without changing it: \
describe_table first (its columns, their types, missing values, ranges and common values), then \
read_rows, find_rows and group_rows to answer questions. Rows are numbered from 1, as the viewer \
shows them.";

/// The tools, as MCP lists them.
pub fn definitions() -> Value {
    let columns = json!({
        "type": "array", "items": { "type": "string" },
        "description": "Column names to show (default: all)."
    });
    let conditions = json!({
        "type": "array",
        "description": "Conditions every kept row meets, one per column, as typed in the viewer's filter: \
`text` (contains), `!text`, `=value`, `!=value`, `> 10`, `>= 10`, `< 10`, `<= 10`, `10..20`, `null`, `!null`.",
        "items": {
            "type": "object",
            "properties": {
                "column": { "type": "string" },
                "condition": { "type": "string" }
            },
            "required": ["column", "condition"]
        }
    });
    json!([
        {
            "name": "describe_table",
            "description": "The open table: where it's from, its size, and per column its type, missing values, min/max/mean for numbers, and the most common values for text.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "read_rows",
            "description": "Rows `from` through `to` (from 1, inclusive; at most 200), tab-separated with a header.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "integer", "minimum": 1 },
                    "to": { "type": "integer", "minimum": 1 },
                    "columns": columns
                },
                "required": ["from"]
            }
        },
        {
            "name": "find_rows",
            "description": "How many rows meet the conditions (and contain `search` in any column, if given), and the first `limit` of them (default 50, at most 200) with their row numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "where": conditions,
                    "search": { "type": "string", "description": "Text any column contains, ignoring case." },
                    "limit": { "type": "integer", "minimum": 0 },
                    "columns": columns
                }
            }
        },
        {
            "name": "group_rows",
            "description": "Rows grouped by the values of column `by`, each group with its row count and, given `value`, the sum, mean, min and max of that numeric column; largest first (by `value`'s sum, else by count), at most `top` groups (default 30). `where` keeps only rows meeting its conditions first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "by": { "type": "string" },
                    "value": { "type": "string" },
                    "where": conditions,
                    "top": { "type": "integer", "minimum": 1 }
                },
                "required": ["by"]
            }
        }
    ])
}

/// Answer one tool call against the table in `view`.
pub async fn call(view: WeakEntity<FrameView>, tool: String, args: Value, cx: &mut AsyncWindowContext) -> Result<String, String> {
    let (frame, title, path) = view
        .read_with(cx, |v, _| (v.frame(), v.title(), v.path().map(|p| p.display().to_string())))
        .map_err(|_| "The table was closed.".to_string())?;
    cx.background_executor()
        .spawn(async move {
            match tool.as_str() {
                "describe_table" => Ok(describe(&frame, &title, path.as_deref())),
                "read_rows" => read_rows(&frame, &args),
                "find_rows" => find_rows(&frame, &args),
                "group_rows" => group_rows(&frame, &args),
                other => Err(format!("Herse has no tool called {other} for tables.")),
            }
            .map(clip)
        })
        .await
}

fn describe(frame: &Frame, title: &str, path: Option<&str>) -> String {
    let columns = frame.columns();
    let mut out = format!("{title}\n");
    if let Some(path) = path {
        out.push_str(&format!("File: {path}\n"));
    }
    out.push_str(&format!("{} rows × {} columns\n\n", frame.rows(), columns.len()));
    for (c, column) in columns.iter().enumerate() {
        let stats = frame.stats(c);
        let kind = frame.kind(c);
        let dtype = column.dtype.clone().unwrap_or_else(|| kind.label().to_string());
        let mut line = format!("{}. {} ({dtype})", c + 1, column.name);
        if stats.nulls > 0 {
            line.push_str(&format!(", {} missing", stats.nulls));
        }
        match kind {
            Kind::Int | Kind::Float => {
                line.push_str(&format!(", min {} max {}", stats.min, stats.max));
                if let Some(mean) = stats.mean() {
                    line.push_str(&format!(" mean {}", round(mean)));
                }
            }
            Kind::Bool | Kind::Text => {
                let (counts, distinct, capped) = value_counts(frame, c, 5);
                let more = if capped { "+" } else { "" };
                line.push_str(&format!(", {distinct}{more} distinct; most common: "));
                let common: Vec<String> = counts.iter().map(|(v, n)| format!("{} ({n})", quote(v))).collect();
                line.push_str(&common.join(", "));
            }
            Kind::Empty => {}
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn read_rows(frame: &Frame, args: &Value) -> Result<String, String> {
    let rows = frame.rows();
    if rows == 0 {
        return Ok("The table has no rows.".into());
    }
    let from = number(args, "from").ok_or("`from` is required.")?.max(1);
    let to = number(args, "to").unwrap_or(from).max(from).min(rows).min(from + MAX_ROWS - 1);
    if from > rows {
        return Err(format!("The table has {rows} rows."));
    }
    let columns = pick_columns(frame, args)?;
    Ok(table(frame, &columns, (from - 1..to).collect::<Vec<_>>().as_slice()))
}

fn find_rows(frame: &Frame, args: &Value) -> Result<String, String> {
    let filters = conditions(frame, args)?;
    let search = args.get("search").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let limit = number(args, "limit").unwrap_or(50).min(MAX_ROWS);
    let columns = pick_columns(frame, args)?;
    let kinds: Vec<Kind> = (0..frame.columns().len()).map(|c| frame.kind(c)).collect();
    let mut kept = Vec::new();
    let mut count = 0;
    for row in 0..frame.rows() {
        if !filters.iter().all(|f| f.keeps(frame, kinds[f.column], row)) {
            continue;
        }
        if !search.is_empty() && !(0..kinds.len()).any(|c| frame.text(row, c).to_lowercase().contains(&search)) {
            continue;
        }
        count += 1;
        if kept.len() < limit {
            kept.push(row);
        }
    }
    let mut out = format!("{count} of {} rows match.", frame.rows());
    if !kept.is_empty() {
        let shown = if kept.len() < count { format!(" The first {}:", kept.len()) } else { String::new() };
        out.push_str(&format!("{shown}\n\n{}", table(frame, &columns, &kept)));
    }
    Ok(out)
}

fn group_rows(frame: &Frame, args: &Value) -> Result<String, String> {
    let by = column(frame, args.get("by").and_then(Value::as_str).ok_or("`by` is required.")?)?;
    let value = args.get("value").and_then(Value::as_str).map(|name| column(frame, name)).transpose()?;
    if let Some(v) = value
        && !frame.kind(v).is_numeric()
    {
        return Err(format!("{} isn't a column of numbers.", frame.columns()[v].name));
    }
    let filters = conditions(frame, args)?;
    let top = number(args, "top").unwrap_or(30).max(1);
    let kinds: Vec<Kind> = (0..frame.columns().len()).map(|c| frame.kind(c)).collect();

    #[derive(Default)]
    struct Group {
        rows: usize,
        values: usize,
        sum: f64,
        min: f64,
        max: f64,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    for row in 0..frame.rows() {
        if !filters.iter().all(|f| f.keeps(frame, kinds[f.column], row)) {
            continue;
        }
        let key = if frame.is_null(row, by) { "(missing)".to_string() } else { frame.text(row, by).to_string() };
        let group = groups.entry(key).or_default();
        group.rows += 1;
        if let Some(x) = value.and_then(|v| frame.number(row, v)) {
            if group.values == 0 {
                group.min = x;
                group.max = x;
            }
            group.values += 1;
            group.sum += x;
            group.min = group.min.min(x);
            group.max = group.max.max(x);
        }
    }
    let total = groups.len();
    let mut groups: Vec<(String, Group)> = groups.into_iter().collect();
    groups.sort_by(|a, b| {
        let key = |g: &Group| if value.is_some() { g.sum } else { g.rows as f64 };
        key(&b.1).total_cmp(&key(&a.1)).then_with(|| a.0.cmp(&b.0))
    });
    let names = frame.columns();
    let mut out = format!("{total} groups by {}", names[by].name);
    if groups.len() > top {
        out.push_str(&format!(", the largest {top}"));
    }
    out.push_str(":\n\n");
    out.push_str(&names[by].name);
    out.push_str("\trows");
    if let Some(v) = value {
        let n = &names[v].name;
        out.push_str(&format!("\tsum of {n}\tmean of {n}\tmin of {n}\tmax of {n}"));
    }
    out.push('\n');
    for (key, g) in groups.iter().take(top) {
        out.push_str(&format!("{key}\t{}", g.rows));
        if value.is_some() {
            if g.values == 0 {
                out.push_str("\t\t\t\t");
            } else {
                let mean = g.sum / g.values as f64;
                out.push_str(&format!("\t{}\t{}\t{}\t{}", round(g.sum), round(mean), g.min, g.max));
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// The most common values of column `c` (at most `n`), how many distinct
/// values there are, and whether counting stopped early.
fn value_counts(frame: &Frame, c: usize, n: usize) -> (Vec<(String, usize)>, usize, bool) {
    const CAP: usize = 50_000;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut capped = false;
    for row in 0..frame.rows() {
        if frame.is_null(row, c) {
            continue;
        }
        let text = frame.text(row, c);
        if counts.len() >= CAP && !counts.contains_key(text) {
            capped = true;
            continue;
        }
        *counts.entry(text).or_default() += 1;
    }
    let distinct = counts.len();
    let mut counts: Vec<(&str, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    (counts.into_iter().take(n).map(|(v, k)| (v.to_string(), k)).collect(), distinct, capped)
}

/// Rows as tab-separated text, each led by its number.
fn table(frame: &Frame, columns: &[usize], rows: &[usize]) -> String {
    let names = frame.columns();
    let mut out = String::from("row");
    for &c in columns {
        out.push('\t');
        out.push_str(&names[c].name);
    }
    out.push('\n');
    for &row in rows {
        out.push_str(&(row + 1).to_string());
        for &c in columns {
            out.push('\t');
            if !frame.is_null(row, c) {
                out.push_str(&frame.text(row, c).replace(['\t', '\n'], " "));
            }
        }
        out.push('\n');
    }
    out
}

fn conditions(frame: &Frame, args: &Value) -> Result<Vec<Filter>, String> {
    let Some(list) = args.get("where").and_then(Value::as_array) else { return Ok(Vec::new()) };
    list.iter()
        .map(|item| {
            let name = item.get("column").and_then(Value::as_str).ok_or("Each condition needs a `column`.")?;
            let condition = item.get("condition").and_then(Value::as_str).ok_or("Each condition needs a `condition`.")?;
            Filter::parse(column(frame, name)?, condition)
        })
        .collect()
}

fn pick_columns(frame: &Frame, args: &Value) -> Result<Vec<usize>, String> {
    match args.get("columns").and_then(Value::as_array) {
        Some(names) if !names.is_empty() => names
            .iter()
            .map(|n| column(frame, n.as_str().ok_or("Column names are strings.")?))
            .collect(),
        _ => Ok((0..frame.columns().len()).collect()),
    }
}

/// A column by name: exactly, else ignoring case.
fn column(frame: &Frame, name: &str) -> Result<usize, String> {
    let columns = frame.columns();
    columns
        .iter()
        .position(|c| c.name == name)
        .or_else(|| columns.iter().position(|c| c.name.eq_ignore_ascii_case(name.trim())))
        .ok_or_else(|| {
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            format!("There's no column {name:?}. The columns are: {}.", names.join(", "))
        })
}

fn number(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn round(x: f64) -> String {
    let s = format!("{x:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn quote(v: &str) -> String {
    let short: String = v.chars().take(40).collect();
    if short.len() < v.len() { format!("{short:?}…") } else { format!("{short:?}") }
}

fn clip(text: String) -> String {
    if text.len() <= BUDGET {
        return text;
    }
    let mut end = BUDGET;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more characters; ask for fewer rows or columns)", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use herse_data::{ChunkBuilder, Column, Value as Cell};

    fn sales() -> Frame {
        let mut frame = Frame::default();
        frame.set_columns(vec![Column::new("region"), Column::new("units")]);
        let mut chunk = ChunkBuilder::new(2);
        for (region, units) in [("West", "5"), ("East", "2"), ("West", "7"), ("North", "")] {
            let units = if units.is_empty() { Cell::Null("") } else { Cell::Text(units) };
            chunk.push_row([Cell::Text(region), units]);
        }
        frame.push_chunk(chunk.finish());
        frame
    }

    #[test]
    fn groups_sum_a_column_largest_first() {
        let out = group_rows(&sales(), &json!({ "by": "region", "value": "units" })).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "3 groups by region:");
        assert!(lines[3].starts_with("West\t2\t12\t6\t5\t7"), "{out}");
        assert!(lines[4].starts_with("East\t1\t2"), "{out}");
    }

    #[test]
    fn finds_rows_with_the_viewer_s_filters() {
        let out = find_rows(&sales(), &json!({ "where": [{ "column": "units", "condition": "> 3" }] })).unwrap();
        assert!(out.starts_with("2 of 4 rows match."), "{out}");
        assert!(out.contains("\n1\tWest\t5\n") && out.contains("\n3\tWest\t7\n"), "{out}");
    }

    #[test]
    fn names_the_columns_when_one_is_wrong() {
        let err = read_rows(&sales(), &json!({ "from": 1, "columns": ["price"] })).unwrap_err();
        assert!(err.contains("region, units"), "{err}");
    }

    #[test]
    fn describes_types_and_common_values() {
        let out = describe(&sales(), "sales.csv", None);
        assert!(out.contains("4 rows × 2 columns"), "{out}");
        assert!(out.contains("1. region (str), 3 distinct; most common: \"West\" (2)"), "{out}");
        assert!(out.contains("2. units (int), 1 missing, min 2 max 7 mean 4.6667"), "{out}");
    }
}
