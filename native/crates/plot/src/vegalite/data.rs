//! Vega-Lite data: tables, inline/named datasets, and the transforms we can
//! evaluate exactly (filter, calculate, aggregate, bin, timeUnit, fold,
//! joinaggregate, simple windows).

use std::collections::HashMap;

use serde_json::Value;

use super::expr::{self, Env};
use crate::time;

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Null,
    Num(f64),
    Str(String),
    Bool(bool),
}

impl V {
    pub fn from_json(v: &Value) -> V {
        match v {
            Value::Null => V::Null,
            Value::Bool(b) => V::Bool(*b),
            Value::Number(n) => n.as_f64().map(V::Num).unwrap_or(V::Null),
            Value::String(s) => V::Str(s.clone()),
            other => V::Str(other.to_string()),
        }
    }
    pub fn num(&self) -> Option<f64> {
        match self {
            V::Num(n) if n.is_finite() => Some(*n),
            V::Str(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
            V::Bool(b) => Some(if *b { 1. } else { 0. }),
            _ => None,
        }
    }
    /// Temporal value in ms (strings parsed as dates; JS Date semantics:
    /// an explicit offset is honored, otherwise wall-clock = UTC here).
    pub fn date(&self) -> Option<f64> {
        match self {
            V::Num(n) if n.is_finite() => Some(*n),
            V::Str(s) => time::parse_datetime(s).map(|(t, off)| t - off.unwrap_or(0.)),
            _ => None,
        }
    }
    pub fn is_valid(&self) -> bool {
        match self {
            V::Null => false,
            V::Num(n) => !n.is_nan(),
            _ => true,
        }
    }
    /// Key for grouping / discrete domains.
    pub fn key(&self) -> String {
        match self {
            V::Null => "\u{0}null".into(),
            V::Num(n) => format!("\u{1}{}", crate::format::js_string(*n)),
            V::Str(s) => s.clone(),
            V::Bool(b) => format!("\u{2}{b}"),
        }
    }
    pub fn label(&self) -> String {
        match self {
            V::Null => "null".into(),
            V::Num(n) => crate::format::js_string(*n),
            V::Str(s) => s.clone(),
            V::Bool(b) => b.to_string(),
        }
    }
}

/// d3.ascending-like ordering (numbers numeric, strings lexicographic,
/// nulls last).
pub fn cmp_v(a: &V, b: &V) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (V::Null, V::Null) => Equal,
        (V::Null, _) => Greater,
        (_, V::Null) => Less,
        (V::Str(x), V::Str(y)) => x.cmp(y),
        (V::Num(x), V::Num(y)) => x.total_cmp(y),
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        (V::Num(_), _) => Less,
        (_, V::Num(_)) => Greater,
        _ => a.label().cmp(&b.label()),
    }
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub names: Vec<String>,
    pub index: HashMap<String, usize>,
    pub rows: Vec<Vec<V>>,
}

pub struct RowEnv<'a> {
    pub t: &'a Table,
    pub r: usize,
    pub params: &'a HashMap<String, V>,
}

impl Env for RowEnv<'_> {
    fn field(&self, name: &str) -> V {
        match self.t.index.get(name) {
            Some(&c) => self.t.rows[self.r].get(c).cloned().unwrap_or(V::Null),
            None => V::Null,
        }
    }
    fn param(&self, name: &str) -> Option<V> {
        self.params.get(name).cloned()
    }
}

impl Table {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied().or_else(|| {
            // VL escapes dots/brackets in field names with backslashes.
            let unescaped = name.replace("\\.", ".").replace("\\[", "[").replace("\\]", "]");
            self.index.get(&unescaped).copied()
        })
    }

    pub fn add_col(&mut self, name: &str) -> usize {
        if let Some(&c) = self.index.get(name) {
            return c;
        }
        let c = self.names.len();
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), c);
        for r in self.rows.iter_mut() {
            r.push(V::Null);
        }
        c
    }

    pub fn get(&self, r: usize, c: usize) -> &V {
        static NULL: V = V::Null;
        self.rows.get(r).and_then(|row| row.get(c)).unwrap_or(&NULL)
    }

    pub fn from_values(values: &[Value]) -> Table {
        let mut t = Table::default();
        for v in values {
            match v {
                Value::Object(o) => {
                    let mut row = vec![V::Null; t.names.len()];
                    for (k, x) in o {
                        let c = match t.index.get(k) {
                            Some(&c) => c,
                            None => {
                                let c = t.add_col(k);
                                row.push(V::Null);
                                c
                            }
                        };
                        if c >= row.len() {
                            row.resize(c + 1, V::Null);
                        }
                        row[c] = V::from_json(x);
                    }
                    row.resize(t.names.len(), V::Null);
                    t.rows.push(row);
                }
                // Primitive arrays become {data: v}.
                other => {
                    let c = t.add_col("data");
                    let mut row = vec![V::Null; t.names.len()];
                    row[c] = V::from_json(other);
                    t.rows.push(row);
                }
            }
        }
        for r in t.rows.iter_mut() {
            r.resize(t.names.len(), V::Null);
        }
        t
    }
}

/// Resolves a `data` spec: inline values, named datasets, sequences.
/// Returns None for URLs and other sources we can't load.
pub fn load(data: &Value, datasets: Option<&Value>) -> Option<Table> {
    let mut t = if let Some(values) = data.get("values") {
        match values {
            Value::Array(a) => Table::from_values(a),
            Value::String(_) => return None,
            Value::Object(_) => Table::from_values(std::slice::from_ref(values)),
            _ => return None,
        }
    } else if let Some(name) = data.get("name").and_then(|n| n.as_str()) {
        let ds = datasets?.get(name)?;
        Table::from_values(ds.as_array()?)
    } else {
        let seq = data.get("sequence")?;
        let start = seq.get("start").and_then(|v| v.as_f64())?;
        let stop = seq.get("stop").and_then(|v| v.as_f64())?;
        let step = seq.get("step").and_then(|v| v.as_f64()).unwrap_or(1.);
        let name = seq.get("as").and_then(|v| v.as_str()).unwrap_or("data");
        if step == 0. || ((stop - start) / step).abs() > 1e6 {
            return None;
        }
        let mut t = Table::default();
        t.add_col(name);
        let mut v = start;
        while (step > 0. && v < stop) || (step < 0. && v > stop) {
            t.rows.push(vec![V::Num(v)]);
            v += step;
        }
        t
    };
    // format.parse: {"field": "number" | "date" | "boolean"}
    if let Some(parse) = data.get("format").and_then(|f| f.get("parse")).and_then(|p| p.as_object()) {
        for (field, ty) in parse {
            let Some(c) = t.col(field) else { continue };
            let ty = ty.as_str().unwrap_or("");
            for r in t.rows.iter_mut() {
                r[c] = match ty {
                    "number" => r[c].num().map(V::Num).unwrap_or(V::Null),
                    "boolean" => V::Bool(expr::truthy(&r[c])),
                    t if t.starts_with("date") || t.starts_with("utc") => r[c].date().map(V::Num).unwrap_or(V::Null),
                    _ => r[c].clone(),
                };
            }
        }
    }
    Some(t)
}

// ---------------------------------------------------------------- bins

#[derive(Clone, Debug, PartialEq)]
pub struct Bins {
    pub start: f64,
    pub stop: f64,
    pub step: f64,
}

/// vega-statistics `bin`.
pub fn bin_params(spec: &Value, extent: (f64, f64)) -> Bins {
    let num = |k: &str| spec.get(k).and_then(|v| v.as_f64());
    let maxb = num("maxbins").filter(|m| *m > 0.).unwrap_or(10.);
    let base = num("base").filter(|b| *b > 1.).unwrap_or(10.);
    let logb = base.ln();
    let div: Vec<f64> = spec
        .get("divide")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
        .unwrap_or_else(|| vec![5., 2.]);
    let (mut min, mut max) = spec
        .get("extent")
        .and_then(|e| e.as_array())
        .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
        .unwrap_or(extent);
    if min > max {
        std::mem::swap(&mut min, &mut max);
    }
    let span = num("span").unwrap_or(if max - min != 0. { max - min } else if min.abs() != 0. { min.abs() } else { 1. });
    let minstep = num("minstep").unwrap_or(0.);
    let mut step;
    if let Some(s) = num("step").filter(|s| *s > 0.) {
        step = s;
    } else if let Some(steps) = spec.get("steps").and_then(|s| s.as_array()) {
        let v = span / maxb;
        let steps: Vec<f64> = steps.iter().filter_map(|x| x.as_f64()).collect();
        let mut i = 0;
        while i < steps.len() && steps[i] < v {
            i += 1;
        }
        step = steps.get(i.saturating_sub(1).min(steps.len().saturating_sub(1))).copied().unwrap_or(1.);
    } else {
        let level = (maxb.ln() / logb).ceil();
        step = minstep.max(base.powf((span.ln() / logb).round() - level));
        let mut guard = 0;
        while (span / step).ceil() > maxb && guard < 100 {
            step *= base;
            guard += 1;
        }
        for d in &div {
            let v = step / d;
            if v >= minstep && span / v <= maxb {
                step = v;
            }
        }
    }
    if !(step.is_finite() && step > 0.) {
        step = 1.;
    }
    let v = step.ln();
    let precision = if v >= 0. { 0. } else { (-v / logb).floor() + 1. };
    let eps = base.powf(-precision - 1.);
    let nice = spec.get("nice").and_then(|n| n.as_bool()).unwrap_or(true);
    if nice {
        let v = (min / step + eps).floor() * step;
        min = if min < v { v - step } else { v };
        max = (max / step).ceil() * step;
    }
    Bins {
        start: min,
        stop: if max == min { min + step } else { max },
        step,
    }
}

pub fn bin_of(b: &Bins, v: f64) -> Option<f64> {
    if !v.is_finite() || v < b.start || v > b.stop {
        return None;
    }
    let v = v.max(b.start).min(b.stop - b.step);
    Some(b.start + b.step * (1e-14 + (v - b.start) / b.step).floor())
}

// ---------------------------------------------------------------- time units

pub const UNITS: [&str; 11] = [
    "year", "quarter", "month", "week", "date", "day", "dayofyear", "hours", "minutes", "seconds", "milliseconds",
];

/// Splits "yearmonthdate" into canonical units.
pub fn time_units(spec: &str) -> Option<Vec<&'static str>> {
    let s = spec.strip_prefix("utc").unwrap_or(spec);
    let s = s.strip_prefix("binned").unwrap_or(s);
    let mut rest = s;
    let mut out = Vec::new();
    // Longest names first so "dayofyear" beats "day".
    let mut names: Vec<&'static str> = UNITS.to_vec();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    while !rest.is_empty() {
        let n = names.iter().find(|n| rest.starts_with(**n))?;
        out.push(*n);
        rest = &rest[n.len()..];
    }
    if out.is_empty() {
        return None;
    }
    out.sort_by_key(|u| UNITS.iter().position(|x| x == u));
    out.dedup();
    Some(out)
}

/// Truncates `t` to the units (vega `timeFloor`); missing fields take
/// Vega-Lite's defaults (year 2012, January, 1st, midnight).
pub fn time_floor(units: &[&str], t: f64) -> f64 {
    let p = time::parts(t);
    let has = |u: &str| units.contains(&u);
    let year = if has("year") { p.year } else { 2012 };
    let mut month = if has("month") {
        p.month
    } else if has("quarter") {
        ((p.month - 1) / 3) * 3 + 1
    } else {
        1
    };
    if has("quarter") && has("month") {
        month = p.month;
    }
    let mut day = if has("date") { p.day } else { 1 };
    let mut base = time::ms_from_ymd(year, month, 1);
    if has("dayofyear") {
        base = time::ms_from_ymd(year, 1, 1) + p.yday as f64 * time::DAY;
        day = 1;
    }
    if has("week") {
        // Sunday-based week of year.
        let jan1 = time::ms_from_ymd(year, 1, 1);
        let wd = time::parts(jan1).weekday;
        let week = (p.yday + wd) / 7;
        base = jan1 + ((week * 7) - wd) as f64 * time::DAY;
        day = 1;
    }
    if has("day") && !has("date") {
        // day-of-week: the first week of Jan 2012 started on a Sunday.
        let anchor = if has("week") { base } else { time::ms_from_ymd(year, month, 1) };
        let wd = time::parts(anchor).weekday;
        base = anchor + ((p.weekday - wd).rem_euclid(7)) as f64 * time::DAY;
        day = 1;
    }
    let mut out = base + (day - 1) as f64 * time::DAY;
    if has("hours") {
        out += p.hour as f64 * time::HOUR;
    }
    if has("minutes") {
        out += p.minute as f64 * time::MINUTE;
    }
    if has("seconds") {
        out += p.second as f64 * time::SECOND;
    }
    if has("milliseconds") {
        out += p.ms;
    }
    out
}

/// End of the unit interval starting at `t` (floored).
pub fn time_unit_end(units: &[&str], t: f64) -> f64 {
    let smallest = units.last().copied().unwrap_or("year");
    match smallest {
        "year" => time::add_months(t, 12),
        "quarter" => time::add_months(t, 3),
        "month" => time::add_months(t, 1),
        "week" => t + time::WEEK,
        "date" | "day" | "dayofyear" => t + time::DAY,
        "hours" => t + time::HOUR,
        "minutes" => t + time::MINUTE,
        "seconds" => t + time::SECOND,
        _ => t + 1.,
    }
}

/// vega `timeUnitSpecifier` with Vega-Lite's overrides.
pub fn time_unit_format(units: &[&str]) -> String {
    let spec = |key: &str| -> Option<&'static str> {
        Some(match key {
            "year" => "%Y ",
            "quarter" => "Q%q ",
            "month" => "%b ",
            "date" => "%d ",
            "week" => "W%U ",
            "day" => "%a ",
            "dayofyear" => "%j ",
            "hours" => "%H:00",
            "minutes" => "00:%M",
            "seconds" => ":%S",
            "milliseconds" => ".%L",
            "year-month" => "%b %Y ",
            "year-month-date" => "%b %d, %Y ",
            "hours-minutes" => "%H:%M",
            _ => return None,
        })
    };
    let mut fmt = String::new();
    let mut start = 0;
    while start < units.len() {
        let mut end = units.len();
        let mut found = false;
        while end > start {
            let key = units[start..end].join("-");
            if let Some(s) = spec(&key) {
                fmt.push_str(s);
                start = end;
                found = true;
                break;
            }
            end -= 1;
        }
        if !found {
            start += 1;
        }
    }
    fmt.trim().to_string()
}

// ---------------------------------------------------------------- aggregates

pub fn aggregate(op: &str, vals: &[V], count: usize) -> Option<V> {
    let nums = || -> Vec<f64> { vals.iter().filter_map(|v| v.num()).collect() };
    let sorted = || {
        let mut v = nums();
        v.sort_by(|a, b| a.total_cmp(b));
        v
    };
    let q = |v: &[f64], p: f64| -> V {
        if v.is_empty() {
            return V::Null;
        }
        // d3.quantile (R-7)
        let h = (v.len() - 1) as f64 * p;
        let lo = h.floor() as usize;
        let hi = (lo + 1).min(v.len() - 1);
        V::Num(v[lo] + (v[hi] - v[lo]) * (h - lo as f64))
    };
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let var = |v: &[f64], sample: bool| -> f64 {
        let m = mean(v);
        let n = v.len() as f64;
        v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / if sample { n - 1. } else { n }
    };
    Some(match op {
        "count" => V::Num(count as f64),
        "valid" => V::Num(vals.iter().filter(|v| v.is_valid()).count() as f64),
        "missing" => V::Num(vals.iter().filter(|v| !v.is_valid()).count() as f64),
        "distinct" => {
            let mut seen: Vec<String> = vals.iter().map(|v| v.key()).collect();
            seen.sort();
            seen.dedup();
            V::Num(seen.len() as f64)
        }
        "sum" => V::Num(nums().iter().sum()),
        "product" => V::Num(nums().iter().product()),
        "mean" | "average" => {
            let v = nums();
            if v.is_empty() { V::Null } else { V::Num(mean(&v)) }
        }
        "variance" | "variancep" | "stdev" | "stdevp" | "stderr" => {
            let v = nums();
            let sample = !op.ends_with('p');
            if v.len() < if sample { 2 } else { 1 } {
                V::Null
            } else {
                let x = var(&v, sample);
                V::Num(match op {
                    "stdev" | "stdevp" => x.sqrt(),
                    "stderr" => (x / v.len() as f64).sqrt(),
                    _ => x,
                })
            }
        }
        "median" => q(&sorted(), 0.5),
        "q1" => q(&sorted(), 0.25),
        "q3" => q(&sorted(), 0.75),
        "min" => nums().into_iter().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.min(b)))).map(V::Num).unwrap_or(V::Null),
        "max" => nums().into_iter().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b)))).map(V::Num).unwrap_or(V::Null),
        _ => return None,
    })
}

pub fn agg_output_name(op: &str, field: Option<&str>) -> String {
    match (op, field) {
        ("count", _) => "__count".into(),
        (op, Some(f)) => format!("{op}_{f}"),
        (op, None) => format!("{op}_"),
    }
}

/// Group-by + aggregate. `measures`: (op, field, as).
pub fn group_aggregate(t: &Table, groupby: &[String], measures: &[(String, Option<String>, String)]) -> Option<Table> {
    let gcols: Vec<Option<usize>> = groupby.iter().map(|g| t.col(g)).collect();
    let mcols: Vec<Option<usize>> = measures.iter().map(|(_, f, _)| f.as_ref().and_then(|f| t.col(f))).collect();
    let mut groups: Vec<(Vec<V>, Vec<usize>)> = Vec::new();
    let mut index: HashMap<Vec<String>, usize> = HashMap::new();
    for r in 0..t.rows.len() {
        let key_vals: Vec<V> = gcols.iter().map(|c| c.map(|c| t.get(r, c).clone()).unwrap_or(V::Null)).collect();
        let key: Vec<String> = key_vals.iter().map(|v| v.key()).collect();
        let g = *index.entry(key).or_insert_with(|| {
            groups.push((key_vals.clone(), Vec::new()));
            groups.len() - 1
        });
        groups[g].1.push(r);
    }
    let mut out = Table::default();
    for g in groupby {
        out.add_col(g);
    }
    for (_, _, name) in measures {
        out.add_col(name);
    }
    if groupby.is_empty() && groups.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }
    for (key, rows) in groups {
        let mut row = key;
        for (k, (op, _, _)) in measures.iter().enumerate() {
            let vals: Vec<V> = match mcols[k] {
                Some(c) => rows.iter().map(|&r| t.get(r, c).clone()).collect(),
                None => Vec::new(),
            };
            row.push(aggregate(op, &vals, rows.len())?);
        }
        out.rows.push(row);
    }
    Some(out)
}

// ---------------------------------------------------------------- transforms

pub struct Ctx<'a> {
    pub params: &'a HashMap<String, V>,
}

fn eval_pred(pred: &Value, t: &Table, r: usize, ctx: &Ctx) -> Option<bool> {
    match pred {
        Value::String(s) => {
            let e = expr::parse(s)?;
            Some(expr::truthy(&expr::eval(&e, &RowEnv { t, r, params: ctx.params })))
        }
        Value::Bool(b) => Some(*b),
        Value::Object(o) => {
            if let Some(a) = o.get("and").and_then(|v| v.as_array()) {
                let mut all = true;
                for p in a {
                    all &= eval_pred(p, t, r, ctx)?;
                }
                return Some(all);
            }
            if let Some(a) = o.get("or").and_then(|v| v.as_array()) {
                let mut any = false;
                for p in a {
                    any |= eval_pred(p, t, r, ctx)?;
                }
                return Some(any);
            }
            if let Some(p) = o.get("not") {
                return Some(!eval_pred(p, t, r, ctx)?);
            }
            if o.contains_key("param") {
                // Empty selections select everything (the default `empty: true`).
                return Some(o.get("empty").and_then(|e| e.as_bool()).unwrap_or(true));
            }
            let field = o.get("field").and_then(|f| f.as_str())?;
            let mut v = t.col(field).map(|c| t.get(r, c).clone()).unwrap_or(V::Null);
            if let Some(tu) = o.get("timeUnit").and_then(|u| u.as_str()) {
                let units = time_units(tu)?;
                v = v.date().map(|d| V::Num(time_floor(&units, d))).unwrap_or(V::Null);
            }
            let cmp_val = |x: &Value| -> Option<f64> {
                match x {
                    Value::Number(n) => n.as_f64(),
                    Value::String(s) => V::Str(s.clone()).num().or_else(|| V::Str(s.clone()).date()),
                    Value::Object(_) => datetime_obj(x),
                    _ => None,
                }
            };
            let vn = v.num().or_else(|| v.date());
            if let Some(eq) = o.get("equal") {
                return Some(match eq {
                    Value::String(s) => v.label() == *s,
                    other => vn.is_some() && vn == cmp_val(other),
                });
            }
            for (k, op) in [("lt", 0), ("lte", 1), ("gt", 2), ("gte", 3)] {
                if let Some(x) = o.get(k) {
                    let (Some(a), Some(b)) = (vn, cmp_val(x)) else { return Some(false) };
                    return Some(match op {
                        0 => a < b,
                        1 => a <= b,
                        2 => a > b,
                        _ => a >= b,
                    });
                }
            }
            if let Some(rg) = o.get("range").and_then(|x| x.as_array()) {
                let lo = rg.first().and_then(cmp_val);
                let hi = rg.get(1).and_then(cmp_val);
                let Some(a) = vn else { return Some(false) };
                return Some(lo.is_none_or(|l| a >= l) && hi.is_none_or(|h| a <= h));
            }
            if let Some(set) = o.get("oneOf").or_else(|| o.get("in")).and_then(|x| x.as_array()) {
                return Some(set.iter().any(|x| match x {
                    Value::String(s) => v.label() == *s,
                    other => vn.is_some() && vn == cmp_val(other),
                }));
            }
            if let Some(valid) = o.get("valid").and_then(|x| x.as_bool()) {
                return Some(v.is_valid() == valid);
            }
            None
        }
        _ => None,
    }
}

/// `{"year": 2020, "month": 1, "date": 1}` datetime objects.
pub fn datetime_obj(v: &Value) -> Option<f64> {
    let o = v.as_object()?;
    let n = |k: &str, d: f64| o.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
    let month = match o.get("month") {
        Some(Value::Number(m)) => m.as_f64()? - 1.,
        Some(Value::String(s)) => {
            let names = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
            names.iter().position(|x| s.to_ascii_lowercase().starts_with(x))? as f64
        }
        _ => 0.,
    };
    let y = n("year", 2012.);
    Some(
        time::ms_from_ymd(y as i64, month as i64 + 1, 1)
            + (n("date", 1.) - 1.) * time::DAY
            + n("hours", 0.) * time::HOUR
            + n("minutes", 0.) * time::MINUTE
            + n("seconds", 0.) * time::SECOND
            + n("milliseconds", 0.),
    )
}

/// Applies a `transform` array. None = unsupported transform.
pub fn apply(t: &mut Table, transforms: &[Value], ctx: &Ctx) -> Option<()> {
    for tr in transforms {
        let o = tr.as_object()?;
        if let Some(pred) = o.get("filter") {
            let mut keep = Vec::with_capacity(t.rows.len());
            for r in 0..t.rows.len() {
                keep.push(eval_pred(pred, t, r, ctx)?);
            }
            let mut i = 0;
            t.rows.retain(|_| {
                let k = keep[i];
                i += 1;
                k
            });
        } else if let Some(e) = o.get("calculate").and_then(|v| v.as_str()) {
            let name = o.get("as")?.as_str()?;
            let ex = expr::parse(e)?;
            if !expr::supported(&ex, &|p| ctx.params.contains_key(p)) {
                return None;
            }
            let vals: Vec<V> = (0..t.rows.len()).map(|r| expr::eval(&ex, &RowEnv { t, r, params: ctx.params })).collect();
            let c = t.add_col(name);
            for (r, v) in vals.into_iter().enumerate() {
                t.rows[r][c] = v;
            }
        } else if let Some(aggs) = o.get("aggregate").and_then(|v| v.as_array()) {
            let groupby: Vec<String> = o
                .get("groupby")
                .and_then(|g| g.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let mut measures = Vec::new();
            for a in aggs {
                let op = a.get("op")?.as_str()?.to_string();
                let field = a.get("field").and_then(|f| f.as_str()).map(str::to_string);
                let name = a.get("as").and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| agg_output_name(&op, field.as_deref()));
                measures.push((op, field, name));
            }
            *t = group_aggregate(t, &groupby, &measures)?;
        } else if let Some(aggs) = o.get("joinaggregate").and_then(|v| v.as_array()) {
            let groupby: Vec<String> = o
                .get("groupby")
                .and_then(|g| g.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let mut measures = Vec::new();
            for a in aggs {
                let op = a.get("op")?.as_str()?.to_string();
                let field = a.get("field").and_then(|f| f.as_str()).map(str::to_string);
                let name = a.get("as").and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| agg_output_name(&op, field.as_deref()));
                measures.push((op, field, name));
            }
            let agg = group_aggregate(t, &groupby, &measures)?;
            let gcols: Vec<Option<usize>> = groupby.iter().map(|g| t.col(g)).collect();
            let mut lookup: HashMap<Vec<String>, usize> = HashMap::new();
            for (i, row) in agg.rows.iter().enumerate() {
                lookup.insert(row[..groupby.len()].iter().map(|v| v.key()).collect(), i);
            }
            let cols: Vec<usize> = measures.iter().map(|(_, _, n)| t.add_col(n)).collect();
            for r in 0..t.rows.len() {
                let key: Vec<String> = gcols.iter().map(|c| c.map(|c| t.get(r, c).key()).unwrap_or_else(|| V::Null.key())).collect();
                if let Some(&ai) = lookup.get(&key) {
                    for (k, &c) in cols.iter().enumerate() {
                        t.rows[r][c] = agg.rows[ai][groupby.len() + k].clone();
                    }
                }
            }
        } else if let Some(bin) = o.get("bin") {
            let field = o.get("field")?.as_str()?;
            let c = t.col(field)?;
            let (start_name, end_name) = match o.get("as")? {
                Value::String(s) => (s.clone(), format!("{s}_end")),
                Value::Array(a) => (a.first()?.as_str()?.to_string(), a.get(1).and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| "bin_end".into())),
                _ => return None,
            };
            let ext = crate::util::extent(t.rows.iter().filter_map(|r| r[c].num()))?;
            let spec = if bin.is_object() { bin.clone() } else { Value::Object(Default::default()) };
            let b = bin_params(&spec, ext);
            let sc = t.add_col(&start_name);
            let ec = t.add_col(&end_name);
            for row in t.rows.iter_mut() {
                let s = row[c].num().and_then(|v| bin_of(&b, v));
                row[sc] = s.map(V::Num).unwrap_or(V::Null);
                row[ec] = s.map(|s| V::Num(s + b.step)).unwrap_or(V::Null);
            }
        } else if let Some(tu) = o.get("timeUnit") {
            let field = o.get("field")?.as_str()?;
            let name = o.get("as")?.as_str()?;
            let units = time_units(tu.as_str().or_else(|| tu.get("unit").and_then(|u| u.as_str()))?)?;
            let c = t.col(field)?;
            let out = t.add_col(name);
            for row in t.rows.iter_mut() {
                row[out] = row[c].date().map(|d| V::Num(time_floor(&units, d))).unwrap_or(V::Null);
            }
        } else if let Some(fold) = o.get("fold").and_then(|f| f.as_array()) {
            let fields: Vec<String> = fold.iter().filter_map(|f| f.as_str().map(str::to_string)).collect();
            let (kn, vn) = match o.get("as").and_then(|a| a.as_array()) {
                Some(a) => (
                    a.first().and_then(|x| x.as_str()).unwrap_or("key").to_string(),
                    a.get(1).and_then(|x| x.as_str()).unwrap_or("value").to_string(),
                ),
                None => ("key".into(), "value".into()),
            };
            let cols: Vec<Option<usize>> = fields.iter().map(|f| t.col(f)).collect();
            let mut out = Table { names: t.names.clone(), index: t.index.clone(), rows: Vec::new() };
            let kc = out.add_col(&kn);
            let vc = out.add_col(&vn);
            for row in &t.rows {
                for (f, c) in fields.iter().zip(&cols) {
                    let mut nr = row.clone();
                    nr.resize(out.names.len(), V::Null);
                    nr[kc] = V::Str(f.clone());
                    nr[vc] = c.map(|c| row[c].clone()).unwrap_or(V::Null);
                    out.rows.push(nr);
                }
            }
            *t = out;
        } else if let Some(win) = o.get("window").and_then(|w| w.as_array()) {
            window(t, o, win)?;
        } else if o.contains_key("sample") {
            // Altair's sample transform: deterministic first-N is a fair stand-in? No: refuse.
            return None;
        } else {
            return None;
        }
    }
    Some(())
}

/// Cumulative / ranking windows over the whole partition (the common
/// Altair cases: cumulative sums/counts and row numbers).
fn window(t: &mut Table, o: &serde_json::Map<String, Value>, ops: &[Value]) -> Option<()> {
    let frame = o.get("frame").and_then(|f| f.as_array()).map(|a| (a.first().cloned(), a.get(1).cloned()));
    let cumulative = match &frame {
        None => true,
        Some((Some(Value::Null), Some(Value::Number(n)))) if n.as_f64() == Some(0.) => true,
        Some((Some(Value::Null), Some(Value::Null))) => false,
        _ => return None,
    };
    let groupby: Vec<usize> = o
        .get("groupby")
        .and_then(|g| g.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(|f| t.col(f)).collect())
        .unwrap_or_default();
    let sort: Vec<(usize, bool)> = o
        .get("sort")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let c = t.col(s.get("field")?.as_str()?)?;
                    Some((c, s.get("order").and_then(|x| x.as_str()) == Some("descending")))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut order: Vec<usize> = (0..t.rows.len()).collect();
    order.sort_by(|&a, &b| {
        for &g in &groupby {
            let c = cmp_v(&t.rows[a][g], &t.rows[b][g]);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        for &(c, desc) in &sort {
            let o = cmp_v(&t.rows[a][c], &t.rows[b][c]);
            let o = if desc { o.reverse() } else { o };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.cmp(&b)
    });
    for op in ops {
        let name = op.get("op")?.as_str()?.to_string();
        let field = op.get("field").and_then(|f| f.as_str()).and_then(|f| t.col(f));
        let out_name = op.get("as")?.as_str()?.to_string();
        let out = t.add_col(&out_name);
        let mut i = 0;
        while i < order.len() {
            let mut j = i;
            while j < order.len() && groupby.iter().all(|&g| t.rows[order[j]][g] == t.rows[order[i]][g]) {
                j += 1;
            }
            let part = &order[i..j];
            let total_vals: Vec<V> = part.iter().map(|&r| field.map(|c| t.rows[r][c].clone()).unwrap_or(V::Null)).collect();
            let total = if cumulative { None } else { aggregate(&name, &total_vals, part.len()) };
            let mut acc = 0.;
            for (k, &r) in part.iter().enumerate() {
                let v = match name.as_str() {
                    "row_number" => V::Num((k + 1) as f64),
                    "rank" | "dense_rank" => V::Num((k + 1) as f64),
                    "count" => V::Num(if cumulative { (k + 1) as f64 } else { part.len() as f64 }),
                    "sum" => {
                        if cumulative {
                            acc += field.and_then(|c| t.rows[r][c].num()).unwrap_or(0.);
                            V::Num(acc)
                        } else {
                            total.clone()?
                        }
                    }
                    "mean" | "average" | "min" | "max" if !cumulative => total.clone()?,
                    _ => return None,
                };
                t.rows[r][out] = v;
            }
            i = j;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vega_bins() {
        // maxbins 10 over [9.3, 42.1] -> step 5, [5, 45]
        let b = bin_params(&json!({}), (9.3, 42.1));
        assert_eq!(b, Bins { start: 5., stop: 45., step: 5. });
        let b = bin_params(&json!({"maxbins": 20}), (9.3, 42.1));
        assert_eq!(b.step, 2.);
        assert_eq!(bin_of(&b, 42.1), Some(42.));
        assert_eq!(bin_of(&b, b.stop), Some(b.stop - b.step));
        let b = bin_params(&json!({"step": 0.5}), (0., 3.));
        assert_eq!(b.step, 0.5);
    }

    #[test]
    fn aggregation() {
        let t = Table::from_values(&[json!({"g": "a", "v": 1}), json!({"g": "a", "v": 3}), json!({"g": "b", "v": 10})]);
        let out = group_aggregate(&t, &["g".into()], &[("mean".into(), Some("v".into()), "m".into()), ("count".into(), None, "n".into())]).unwrap();
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0], vec![V::Str("a".into()), V::Num(2.), V::Num(2.)]);
        assert_eq!(aggregate("median", &[V::Num(1.), V::Num(2.), V::Num(10.)], 3), Some(V::Num(2.)));
        assert_eq!(aggregate("q1", &[V::Num(1.), V::Num(2.), V::Num(3.), V::Num(4.)], 4), Some(V::Num(1.75)));
        assert_eq!(aggregate("distinct", &[V::Num(1.), V::Num(1.), V::Str("x".into())], 3), Some(V::Num(2.)));
    }

    #[test]
    fn time_unit_parsing() {
        assert_eq!(time_units("yearmonth").unwrap(), vec!["year", "month"]);
        assert_eq!(time_units("utcyearmonthdate").unwrap(), vec!["year", "month", "date"]);
        assert_eq!(time_units("dayofyear").unwrap(), vec!["dayofyear"]);
        assert_eq!(time_units("hoursminutes").unwrap(), vec!["hours", "minutes"]);
        assert!(time_units("fortnight").is_none());
        let t = time::parse_datetime("2022-05-17T13:45:00").unwrap().0;
        assert_eq!(time::strftime("%Y-%m-%d", time_floor(&["year", "month"], t)), "2022-05-01");
        assert_eq!(time::strftime("%Y-%m-%d", time_floor(&["month"], t)), "2012-05-01");
        assert_eq!(time::strftime("%H:%M", time_floor(&["hours"], t)), "13:00");
        assert_eq!(time_unit_format(&["year", "month"]), "%b %Y");
        assert_eq!(time_unit_format(&["month"]), "%b");
        assert_eq!(time_unit_format(&["year", "quarter"]), "%Y Q%q");
    }

    #[test]
    fn transforms() {
        let mut t = Table::from_values(&[json!({"a": 1, "b": "x"}), json!({"a": 5, "b": "y"}), json!({"a": 9, "b": "y"})]);
        let params = HashMap::new();
        let ctx = Ctx { params: &params };
        apply(&mut t, &[json!({"filter": "datum.a > 2"}), json!({"calculate": "datum.a * 2", "as": "c"})], &ctx).unwrap();
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0][t.col("c").unwrap()], V::Num(10.));
        apply(&mut t, &[json!({"filter": {"field": "b", "oneOf": ["y"]}})], &ctx).unwrap();
        assert_eq!(t.rows.len(), 2);
        apply(&mut t, &[json!({"window": [{"op": "sum", "field": "a", "as": "cum"}]})], &ctx).unwrap();
        assert_eq!(t.rows[1][t.col("cum").unwrap()], V::Num(14.));
        assert!(apply(&mut t, &[json!({"lookup": "x"})], &ctx).is_none());
        assert!(apply(&mut t, &[json!({"calculate": "unknownFn(1)", "as": "z"})], &ctx).is_none());
    }
}
