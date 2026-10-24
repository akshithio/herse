//! `serde_json::Value` -> model conversion. Used by `output_from_iopub`, by
//! the parser for shapes its fast path does not handle, and for v3 upgrades.

use serde_json::Value;

use crate::model::{Cell, CellType, Map, MimeBundle, MimeData, Output, StreamName, is_json_mime};

/// nbformat `rejoin_lines` for one multiline value: a string, or a list of
/// strings joined. Anything else is `None`.
pub(crate) fn join_multiline(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let mut len = 0;
            for x in a {
                len += x.as_str()?.len();
            }
            let mut s = String::with_capacity(len);
            for x in a {
                s.push_str(x.as_str()?);
            }
            Some(s)
        }
        _ => None,
    }
}

fn take_multiline(v: Value) -> Result<String, Value> {
    match v {
        Value::String(s) => Ok(s),
        Value::Array(_) => join_multiline(&v).ok_or(v),
        other => Err(other),
    }
}

/// Mime bundle from a JSON object, joining non-JSON mimes' line arrays.
pub(crate) fn bundle_from_map(map: Map) -> MimeBundle {
    MimeBundle(
        map.into_iter()
            .map(|(mime, v)| {
                let data = if is_json_mime(&mime) {
                    MimeData::Json(v)
                } else {
                    match take_multiline(v) {
                        Ok(s) => MimeData::Text(s),
                        Err(v) => MimeData::Json(v),
                    }
                };
                (mime, data)
            })
            .collect(),
    )
}

/// Join line arrays inside an attachments object (`{name: bundle}`), like nbformat.
pub(crate) fn rejoin_attachments(v: &mut Value) {
    if let Value::Object(atts) = v {
        for (_, bundle) in atts.iter_mut() {
            if let Value::Object(b) = bundle {
                for (mime, val) in b.iter_mut() {
                    if !is_json_mime(mime)
                        && matches!(val, Value::Array(_))
                        && let Some(s) = join_multiline(val)
                    {
                        *val = Value::String(s);
                    }
                }
            }
        }
    }
}

fn opt_i64(v: Option<&Value>) -> Option<Option<i64>> {
    match v {
        None | Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => n.as_i64().map(Some),
        _ => None,
    }
}

/// Output from an on-disk output object. Shapes that don't fit the typed
/// variants are kept verbatim as [`Output::Unknown`].
///
/// Same rules as the parser's fast path: `metadata` may be missing on
/// display_data / execute_result (repaired to `{}`), `transient` is accepted
/// there (kept on display_data, dropped on execute_result), any other
/// unexpected key makes the output `Unknown`.
pub(crate) fn output_from_map(map: Map) -> Output {
    match typed_output(&map) {
        Some(o) => o,
        None => Output::Unknown(map),
    }
}

fn typed_output(map: &Map) -> Option<Output> {
    let ot = map.get("output_type")?.as_str()?;
    let allowed: &[&str] = match ot {
        "stream" => &["output_type", "name", "text"],
        "display_data" => &["output_type", "data", "metadata", "transient"],
        "execute_result" => &["output_type", "data", "metadata", "execution_count", "transient"],
        "error" => &["output_type", "ename", "evalue", "traceback"],
        _ => return None,
    };
    if !map.keys().all(|k| allowed.contains(&k.as_str())) {
        return None;
    }
    let metadata = || -> Option<Map> {
        match map.get("metadata") {
            None => Some(Map::new()),
            Some(Value::Object(m)) => Some(m.clone()),
            Some(_) => None,
        }
    };
    let data = || -> Option<MimeBundle> { Some(bundle_from_map(map.get("data")?.as_object()?.clone())) };
    Some(match ot {
        "stream" => Output::Stream {
            name: StreamName::from_str_opt(map.get("name")?.as_str()?)?,
            text: join_multiline(map.get("text")?)?,
        },
        "display_data" => Output::DisplayData {
            data: data()?,
            metadata: metadata()?,
            transient: map.get("transient").filter(|t| !t.is_null()).cloned(),
        },
        "execute_result" => Output::ExecuteResult {
            execution_count: opt_i64(map.get("execution_count"))?,
            data: data()?,
            metadata: metadata()?,
        },
        _ => Output::Error {
            ename: map.get("ename")?.as_str()?.to_owned(),
            evalue: map.get("evalue")?.as_str()?.to_owned(),
            traceback: map
                .get("traceback")?
                .as_array()?
                .iter()
                .map(|t| t.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?,
        },
    })
}

/// Cell from an on-disk cell object.
pub(crate) fn cell_from_map(mut map: Map) -> Result<Cell, String> {
    let cell_type = match map.get("cell_type").and_then(Value::as_str) {
        Some(t) => CellType::from_str_opt(t).ok_or_else(|| format!("unknown cell_type {t:?}"))?,
        None => return Err("cell without cell_type".into()),
    };
    map.shift_remove("cell_type");
    let mut cell = Cell::new(cell_type);
    if let Some(Value::String(_)) = map.get("id") {
        let Some(Value::String(id)) = map.shift_remove("id") else { unreachable!() };
        cell.id = Some(id);
    }
    if let Some(Value::Object(_)) = map.get("metadata") {
        let Some(Value::Object(m)) = map.shift_remove("metadata") else { unreachable!() };
        cell.metadata = m;
    }
    if let Some(src) = map.get("source")
        && let Some(s) = join_multiline(src)
    {
        cell.source = s;
        map.shift_remove("source");
    }
    if let Some(a) = map.get("attachments")
        && !a.is_null()
    {
        let mut a = map.shift_remove("attachments").unwrap();
        rejoin_attachments(&mut a);
        cell.attachments = Some(a);
    }
    if cell_type == CellType::Code {
        if let Some(ec) = opt_i64(map.get("execution_count"))
            && map.contains_key("execution_count")
        {
            cell.execution_count = ec;
            map.shift_remove("execution_count");
        }
        if let Some(Value::Array(_)) = map.get("outputs") {
            let Some(Value::Array(outs)) = map.shift_remove("outputs") else { unreachable!() };
            cell.outputs = outs
                .into_iter()
                .map(|o| match o {
                    Value::Object(m) => output_from_map(m),
                    other => {
                        let mut m = Map::new();
                        m.insert("output_type".into(), other);
                        Output::Unknown(m)
                    }
                })
                .collect();
        }
    }
    cell.extra = map;
    Ok(cell)
}

/// Output from a Jupyter iopub message (messaging spec 5.x).
///
/// Handles `stream`, `display_data`, `update_display_data` (returned as a
/// `DisplayData` carrying `transient.display_id`: the caller replaces the
/// outputs with that display id instead of appending), `execute_result` and
/// `error`. Everything else (`status`, `execute_input`, `clear_output`, ...)
/// returns `None`.
pub fn output_from_iopub(msg_type: &str, content: &Value) -> Option<Output> {
    let obj = content.as_object()?;
    let bundle = |key: &str| -> MimeBundle {
        match obj.get(key) {
            Some(Value::Object(m)) => bundle_from_map(m.clone()),
            _ => MimeBundle::default(),
        }
    };
    let meta = |key: &str| -> Map {
        match obj.get(key) {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        }
    };
    let transient = || match obj.get("transient") {
        Some(Value::Object(t)) if !t.is_empty() => Some(Value::Object(t.clone())),
        _ => None,
    };
    match msg_type {
        "stream" => {
            let name = match obj.get("name").and_then(Value::as_str) {
                Some("stderr") => StreamName::Stderr,
                _ => StreamName::Stdout,
            };
            let text = obj.get("text").and_then(join_multiline).unwrap_or_default();
            Some(Output::Stream { name, text })
        }
        "display_data" | "update_display_data" => Some(Output::DisplayData {
            data: bundle("data"),
            metadata: meta("metadata"),
            transient: transient(),
        }),
        "execute_result" => Some(Output::ExecuteResult {
            execution_count: obj.get("execution_count").and_then(Value::as_i64),
            data: bundle("data"),
            metadata: meta("metadata"),
        }),
        "error" => Some(Output::Error {
            ename: obj.get("ename").and_then(Value::as_str).unwrap_or("Error").to_owned(),
            evalue: obj.get("evalue").and_then(Value::as_str).unwrap_or("").to_owned(),
            traceback: obj
                .get("traceback")
                .and_then(Value::as_array)
                .map(|tb| tb.iter().map(|t| t.as_str().map_or_else(|| t.to_string(), str::to_owned)).collect())
                .unwrap_or_default(),
        }),
        _ => None,
    }
}
