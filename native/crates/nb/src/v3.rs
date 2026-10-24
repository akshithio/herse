//! nbformat v3 -> v4 upgrade on read, following `nbformat.v4.convert.upgrade`
//! (after v3's own `rejoin_lines` / `strip_transient`).

use serde_json::Value;

use crate::convert::cell_from_map;
use crate::error::{Error, Result};
use crate::format::FormatHint;
use crate::model::{Map, Notebook, new_cell_id};
use crate::text::split_lines;

const MULTILINE_OUTPUTS: &[&str] = &["text", "html", "svg", "latex", "javascript", "json"];

const MIME_MAP: &[(&str, &str)] = &[
    ("text", "text/plain"),
    ("html", "text/html"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpeg", "image/jpeg"),
    ("latex", "text/latex"),
    ("json", "application/json"),
    ("javascript", "application/javascript"),
];

/// v3 `_join_lines`: lines written by `splitlines(True)` are concatenated,
/// lines from plain `splitlines()` are joined with `\n`.
fn join_lines_v3(v: &mut Value) {
    if let Value::Array(lines) = v
        && lines.iter().all(Value::is_string)
    {
        let strs: Vec<&str> = lines.iter().filter_map(Value::as_str).collect();
        let keep_ends = strs.first().is_some_and(|l| l.ends_with('\n') || l.ends_with('\r'));
        *v = Value::String(if keep_ends { strs.concat() } else { strs.join("\n") });
    }
}

fn to_mime_key(d: &mut Map) {
    for (alias, mime) in MIME_MAP {
        if let Some(v) = d.shift_remove(*alias) {
            d.insert((*mime).to_string(), v);
        }
    }
}

fn upgrade_output(mut o: Map) -> Map {
    for key in MULTILINE_OUTPUTS {
        if let Some(v) = o.get_mut(*key) {
            join_lines_v3(v);
        }
    }
    let ot = o.get("output_type").and_then(Value::as_str).unwrap_or("").to_string();
    match ot.as_str() {
        "pyout" | "display_data" => {
            if !o.contains_key("metadata") {
                o.insert("metadata".into(), Value::Object(Map::new()));
            }
            if ot == "pyout" {
                o.insert("output_type".into(), "execute_result".into());
                let pn = o.shift_remove("prompt_number").unwrap_or(Value::Null);
                o.insert("execution_count".into(), pn);
            }
            let mut data = Map::new();
            let keys: Vec<String> = o.keys().cloned().collect();
            for k in keys {
                if matches!(k.as_str(), "output_type" | "execution_count" | "metadata") {
                    continue;
                }
                if let Some(v) = o.shift_remove(&k) {
                    data.insert(k, v);
                }
            }
            to_mime_key(&mut data);
            if let Some(Value::String(s)) = data.get("application/json")
                && let Ok(v) = crate::parse::parse_value(s.as_bytes())
            {
                data.insert("application/json".into(), v);
            }
            o.insert("data".into(), Value::Object(data));
            if let Some(Value::Object(m)) = o.get_mut("metadata") {
                to_mime_key(m);
            }
        }
        "pyerr" => {
            o.insert("output_type".into(), "error".into());
        }
        "stream" => {
            let name = o.shift_remove("stream").unwrap_or_else(|| "stdout".into());
            o.insert("name".into(), name);
        }
        _ => {}
    }
    o
}

fn upgrade_cell(mut c: Map) -> Map {
    // v3 rejoin_lines + strip_transient
    c.shift_remove("trusted");
    if let Some(Value::Object(m)) = c.get_mut("metadata") {
        m.shift_remove("trusted");
    }
    let cell_type = c.get("cell_type").and_then(Value::as_str).unwrap_or("").to_string();
    if cell_type == "code" {
        if let Some(v) = c.get_mut("input") {
            join_lines_v3(v);
        }
    } else {
        for key in ["source", "rendered"] {
            if let Some(v) = c.get_mut(key) {
                join_lines_v3(v);
            }
        }
    }
    // v4 upgrade_cell
    if !c.contains_key("metadata") {
        c.insert("metadata".into(), Value::Object(Map::new()));
    }
    c.insert("id".into(), Value::String(new_cell_id()));
    match cell_type.as_str() {
        "code" => {
            c.shift_remove("language");
            if let Some(collapsed) = c.shift_remove("collapsed")
                && let Some(Value::Object(m)) = c.get_mut("metadata")
            {
                m.insert("collapsed".into(), collapsed);
            }
            let input = c.shift_remove("input").unwrap_or_else(|| "".into());
            c.insert("source".into(), input);
            let pn = c.shift_remove("prompt_number").unwrap_or(Value::Null);
            c.insert("execution_count".into(), pn);
            let outputs = match c.shift_remove("outputs") {
                Some(Value::Array(outs)) => outs
                    .into_iter()
                    .map(|o| match o {
                        Value::Object(m) => Value::Object(upgrade_output(m)),
                        other => other,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            c.insert("outputs".into(), Value::Array(outputs));
        }
        "heading" => {
            c.insert("cell_type".into(), "markdown".into());
            let level = c.shift_remove("level").and_then(|l| l.as_u64()).unwrap_or(1) as usize;
            let src = c.get("source").and_then(Value::as_str).unwrap_or("");
            let single: Vec<&str> = split_lines(src).map(strip_line_end).collect();
            let s = format!("{} {}", "#".repeat(level), single.join(" "));
            c.insert("source".into(), Value::String(s));
        }
        "html" => {
            c.insert("cell_type".into(), "markdown".into());
        }
        _ => {}
    }
    // `rendered` is a v3-only key; nbformat leaves it (invalid) in place.
    c
}

fn strip_line_end(line: &str) -> &str {
    let mut l = line;
    for end in ["\r\n", "\n", "\r", "\x0b", "\x0c", "\x1c", "\x1d", "\x1e", "\u{85}", "\u{2028}", "\u{2029}"] {
        if let Some(s) = l.strip_suffix(end) {
            l = s;
            break;
        }
    }
    l
}

/// Upgrade a parsed v3 notebook (`metadata` + the remaining top-level keys).
pub(crate) fn upgrade(mut metadata: Map, mut top: Map, minor: u64) -> Result<Notebook> {
    top.shift_remove("orig_nbformat");
    top.shift_remove("orig_nbformat_minor");
    let worksheets = match top.shift_remove("worksheets") {
        Some(Value::Array(ws)) => ws,
        Some(_) => return Err(Error::Invalid("v3 `worksheets` is not an array".into())),
        None => Vec::new(),
    };
    let mut cells = Vec::new();
    for ws in worksheets {
        let Value::Object(mut ws) = ws else { continue };
        let Some(Value::Array(ws_cells)) = ws.shift_remove("cells") else { continue };
        for c in ws_cells {
            let Value::Object(c) = c else { continue };
            let index = cells.len();
            let cell = cell_from_map(upgrade_cell(c)).map_err(|e| Error::Invalid(format!("v3 cell {index}: {e}")))?;
            cells.push(cell);
        }
    }
    metadata.shift_remove("name");
    metadata.shift_remove("signature");
    let format = FormatHint { converted_from: Some((3, minor as u32)), ..FormatHint::jupyter() };
    Ok(Notebook { nbformat: 4, nbformat_minor: 5, metadata, cells, extra: top, format })
}
