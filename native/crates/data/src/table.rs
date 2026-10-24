//! A DataFrame output (the grid `herse-table` parsed from its HTML) as a
//! [`Frame`]: the rows the notebook saved, with what the output says about
//! the rows it left out.

use herse_table::{CellKind, Table};

use crate::{ChunkBuilder, Column, Frame, Value};

/// A notebook's DataFrame output as a frame.
#[derive(Clone, Debug, Default)]
pub struct FromTable {
    pub frame: Frame,
    /// Data rows the notebook saved (pandas' `...` row not counted).
    pub saved_rows: usize,
    /// The frame's real size, when the output says ("1000 rows × 50
    /// columns", polars' "shape: (1000, 3)", Julia's "3×2 DataFrame").
    pub total_rows: Option<usize>,
    pub total_columns: Option<usize>,
    /// The output left rows or columns out (pandas' `...` row or column).
    pub truncated: bool,
}

pub fn from_table(t: &Table) -> FromTable {
    let keep: Vec<usize> = (0..t.columns).filter(|&c| !t.is_ellipsis_column(c)).collect();
    let mut columns = Vec::with_capacity(keep.len());
    for &c in &keep {
        let mut parts: Vec<&str> = Vec::new();
        for (r, row) in t.header.iter().enumerate() {
            let Some(cell) = row.get(c) else { continue };
            let text = match cell.span_origin {
                Some((or, oc)) => t.header.get(or).and_then(|row| row.get(oc)).map_or("", |o| o.text.as_str()),
                None => cell.text.as_str(),
            };
            let text = text.trim();
            // A spanning label repeats down its rows; name it once.
            if !text.is_empty() && (parts.last() != Some(&text) || cell.span_origin.is_none_or(|(or, _)| or == r)) {
                parts.push(text);
            }
        }
        let name = parts.join(" / ");
        let dtype = t.dtypes.as_ref().and_then(|d| d.get(c)).filter(|d| !d.is_empty()).cloned();
        columns.push(Column { name, dtype, index: c < t.index_columns });
    }

    let mut builder = ChunkBuilder::new(keep.len());
    let mut saved = 0;
    let mut clean = String::new();
    for (r, row) in t.body.iter().enumerate() {
        if t.is_ellipsis_row(r) {
            continue;
        }
        for (k, &c) in keep.iter().enumerate() {
            let Some(cell) = row.get(c) else {
                builder.push(k, Value::Null(""));
                continue;
            };
            // A MultiIndex label spanning rows: every row carries it, so
            // sorting and filtering see it.
            let origin = cell.span_origin.and_then(|(or, oc)| t.body.get(or).and_then(|row| row.get(oc)));
            let cell = origin.unwrap_or(cell);
            let text = cell.text.as_str();
            let v = match cell.kind {
                CellKind::Null => Value::Null(text),
                CellKind::Number => {
                    clean.clear();
                    clean.extend(text.chars().filter(|ch| !matches!(ch, ',' | '%' | '$' | '€' | '£' | ' ' | '\u{a0}')));
                    match clean.parse::<f64>() {
                        Ok(x) => Value::Number(x, text),
                        Err(_) => Value::Str(text),
                    }
                }
                CellKind::Bool => Value::Bool(text.eq_ignore_ascii_case("true"), text),
                _ => Value::Str(text),
            };
            builder.push(k, v);
        }
        builder.end_row();
        saved += 1;
    }
    let mut frame = Frame::default();
    frame.set_columns(columns);
    frame.push_chunk(builder.finish());

    let shape = t.footer.as_deref().and_then(shape_of).or_else(|| t.caption.as_deref().and_then(shape_of));
    let ellipsis = (0..t.body.len()).any(|r| t.is_ellipsis_row(r)) || keep.len() < t.columns;
    let total_rows = shape.map(|s| s.0);
    FromTable {
        frame,
        saved_rows: saved,
        total_rows,
        total_columns: shape.map(|s| s.1),
        truncated: ellipsis || total_rows.is_some_and(|n| n > saved),
    }
}

/// `(rows, columns)` from a shape line.
fn shape_of(line: &str) -> Option<(usize, usize)> {
    let nums: Vec<usize> = line
        .split(|c: char| !(c.is_ascii_digit() || c == ','))
        .filter_map(|s| {
            let s = s.trim_matches(',');
            (!s.is_empty()).then(|| s.replace(',', "").parse().ok()).flatten()
        })
        .collect();
    // pandas: "1,000 rows × 50 columns"; polars: "shape: (1000, 3)";
    // Julia: "3×2 DataFrame"; R: "A data.frame: 6 × 3".
    if line.contains("rows") || line.contains("shape") || line.contains('×') {
        if let [r, c, ..] = nums[..] {
            return Some((r, c));
        }
        if line.contains("rows")
            && let [r] = nums[..]
        {
            return Some((r, 0));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(shape_of("1000 rows × 50 columns"), Some((1000, 50)));
        assert_eq!(shape_of("1,000 rows × 3 columns"), Some((1000, 3)));
        assert_eq!(shape_of("shape: (1000, 3)"), Some((1000, 3)));
        assert_eq!(shape_of("3×2 DataFrame"), Some((3, 2)));
        assert_eq!(shape_of("hello"), None);
    }

    #[test]
    fn pandas_output() {
        let html = r#"<table border="1" class="dataframe">
          <thead><tr style="text-align: right;"><th></th><th>a</th><th>b</th></tr></thead>
          <tbody><tr><th>0</th><td>1.5</td><td>x</td></tr>
                 <tr><th>...</th><td>...</td><td>...</td></tr>
                 <tr><th>999</th><td>NaN</td><td>y</td></tr></tbody>
        </table><p>1000 rows × 2 columns</p>"#;
        let t = herse_table::parse(html).unwrap();
        let f = from_table(&t);
        assert_eq!(f.saved_rows, 2);
        assert_eq!(f.total_rows, Some(1000));
        assert!(f.truncated);
        let frame = &f.frame;
        assert_eq!(frame.columns().len(), 3);
        assert!(frame.columns()[0].index);
        assert_eq!(frame.columns()[1].name, "a");
        assert_eq!(frame.text(1, 0), "999");
        assert_eq!(frame.number(0, 1), Some(1.5));
        assert!(frame.is_null(1, 1));
        assert_eq!(frame.kind(1), crate::Kind::Float);
    }
}
