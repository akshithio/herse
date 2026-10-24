//! A Vega-Lite (layered) view: encodings -> derived data -> scales -> axes
//! and marks.

use std::collections::HashMap;

use serde_json::Value;

use super::data::{self, Bins, Table, V, cmp_v};
use super::{Config, Guides, LegendReq, Theme};
use crate::color::{Colorscale, Rgba};
use crate::format;
use crate::svg::{Anchor, PathData, Style, Svg, TextStyle};
use crate::text;
use crate::ticks;
use crate::time;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Q,
    N,
    O,
    T,
}

impl Ty {
    pub fn discrete(self) -> bool {
        matches!(self, Ty::N | Ty::O)
    }
}

/// One encoding channel after derivation (aggregate/bin/timeUnit applied).
#[derive(Clone, Debug)]
pub struct Chan {
    pub def: Value,
    pub field: Option<String>,
    pub field2: Option<String>,
    pub raw_field: Option<String>,
    pub ty: Ty,
    pub agg: Option<String>,
    pub bin: Option<Value>,
    pub bins: Option<Bins>,
    pub binned: bool,
    pub units: Option<Vec<&'static str>>,
    pub title: Option<String>,
    pub value: Option<Value>,
}

impl Chan {
    pub fn scale(&self, k: &str) -> Option<&Value> {
        self.def.get("scale").and_then(|s| s.get(k)).filter(|v| !v.is_null())
    }
    pub fn has_field(&self) -> bool {
        self.field.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct MarkDef {
    pub ty: String,
    pub props: serde_json::Map<String, Value>,
}

impl MarkDef {
    pub fn get<'a>(&'a self, k: &str, cfg: &'a Config) -> Option<&'a Value> {
        self.props.get(k).filter(|v| !v.is_null()).or_else(|| cfg.mark_prop(&self.ty, k))
    }
    pub fn num(&self, k: &str, cfg: &Config) -> Option<f64> {
        self.get(k, cfg).and_then(|v| v.as_f64())
    }
    pub fn color(&self, k: &str, cfg: &Config) -> Option<Rgba> {
        self.get(k, cfg).and_then(|v| v.as_str()).and_then(Rgba::parse)
    }
    pub fn flag(&self, k: &str, cfg: &Config) -> Option<bool> {
        self.get(k, cfg).and_then(|v| v.as_bool())
    }
}

pub struct Layer {
    pub mark: MarkDef,
    pub enc: HashMap<String, Chan>,
    pub table: Table,
    /// (measure channel, start col, end col)
    pub stack: Option<(String, String, String)>,
    pub orient_h: bool,
}

impl Layer {
    pub fn ch(&self, c: &str) -> Option<&Chan> {
        self.enc.get(c)
    }
}

// ---------------------------------------------------------------- building layers

fn ty_of(def: &Value) -> Option<Ty> {
    Some(match def.get("type").and_then(|t| t.as_str())? {
        "quantitative" => Ty::Q,
        "nominal" => Ty::N,
        "ordinal" => Ty::O,
        "temporal" => Ty::T,
        _ => return None,
    })
}

fn title_case(op: &str) -> String {
    let mut c = op.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

const CHANNELS: &[&str] = &[
    "x", "y", "x2", "y2", "xOffset", "yOffset", "color", "fill", "stroke", "opacity", "fillOpacity", "strokeOpacity",
    "size", "shape", "strokeDash", "strokeWidth", "text", "tooltip", "detail", "order", "theta", "theta2", "radius",
    "radius2", "href", "description", "key", "url",
];

/// Builds a layer: parses the mark and encoding, derives timeUnit / bin /
/// aggregate / stack fields. None = something we can't draw faithfully.
pub fn build_layer(mark: &Value, encoding: &serde_json::Map<String, Value>, mut table: Table, _cfg: &Config, notes: &mut Vec<String>) -> Option<Layer> {
    let mark = match mark {
        Value::String(s) => MarkDef { ty: s.clone(), props: Default::default() },
        Value::Object(o) => MarkDef {
            ty: o.get("type")?.as_str()?.to_string(),
            props: o.clone(),
        },
        _ => return None,
    };
    if !matches!(
        mark.ty.as_str(),
        "point" | "circle" | "square" | "tick" | "line" | "area" | "bar" | "rect" | "rule" | "text" | "arc"
    ) {
        return None;
    }
    let mut enc: HashMap<String, Chan> = HashMap::new();
    for (name, def) in encoding {
        if !CHANNELS.contains(&name.as_str()) {
            if matches!(name.as_str(), "row" | "column" | "facet") {
                continue;
            }
            return None;
        }
        if matches!(name.as_str(), "tooltip" | "href" | "description" | "key") {
            continue;
        }
        if name == "url" {
            return None;
        }
        // detail / tooltip may be arrays of defs.
        let defs: Vec<&Value> = match def {
            Value::Array(a) => a.iter().collect(),
            d => vec![d],
        };
        for (k, d) in defs.into_iter().enumerate() {
            let mut d = d.clone();
            // Conditions: selections are empty initially -> the condition holds
            // (`empty: true`); test predicates we can't evaluate are refused.
            if let Some(cond) = d.get("condition").cloned() {
                let cond = match cond {
                    Value::Array(a) => a.first().cloned()?,
                    c => c,
                };
                if cond.get("param").is_some() || cond.get("selection").is_some() {
                    let empty = cond.get("empty").and_then(|e| e.as_bool()).unwrap_or(true);
                    if empty {
                        let mut merged = cond.as_object().cloned().unwrap_or_default();
                        merged.remove("param");
                        merged.remove("selection");
                        merged.remove("empty");
                        d = Value::Object(merged);
                    } else if let Some(o) = d.as_object_mut() {
                        o.remove("condition");
                    }
                } else if cond.get("test").is_some() {
                    return None;
                }
            }
            let key = if k == 0 { name.clone() } else { format!("{name}#{k}") };
            let chan = parse_chan(&d, name)?;
            enc.insert(key, chan);
        }
    }
    if mark.ty == "arc" && !enc.contains_key("theta") {
        return None;
    }
    // ---- timeUnit
    let keys: Vec<String> = enc.keys().cloned().collect();
    for k in &keys {
        let ch = enc.get_mut(k)?;
        if let (Some(units), Some(f)) = (ch.units.clone(), ch.raw_field.clone()) {
            let c = table.col(&f)?;
            let name = format!("{}_{}", units.join(""), f);
            let end = format!("{name}_end");
            let nc = table.add_col(&name);
            let ec = table.add_col(&end);
            for row in table.rows.iter_mut() {
                let d = row[c].date();
                row[nc] = d.map(|d| V::Num(data::time_floor(&units, d))).unwrap_or(V::Null);
                row[ec] = d.map(|d| V::Num(data::time_unit_end(&units, data::time_floor(&units, d)))).unwrap_or(V::Null);
            }
            ch.field = Some(name);
            ch.field2 = Some(end);
        } else if ch.ty == Ty::T
            && let Some(f) = ch.field.clone()
        {
            // Temporal fields are parsed as dates.
            let c = table.col(&f)?;
            for row in table.rows.iter_mut() {
                row[c] = row[c].date().map(V::Num).unwrap_or(V::Null);
            }
        }
    }
    // ---- bin
    for k in &keys {
        let ch = enc.get_mut(k)?;
        let Some(bin) = ch.bin.clone() else { continue };
        if ch.binned {
            continue;
        }
        let f = ch.raw_field.clone()?;
        let c = table.col(&f)?;
        let ext = crate::util::extent(table.rows.iter().filter_map(|r| r[c].num()));
        let Some(ext) = ext else { continue };
        let mut spec = if bin.is_object() { bin.clone() } else { Value::Object(Default::default()) };
        if spec.get("maxbins").is_none() && matches!(k.as_str(), "row" | "column" | "shape") {
            spec["maxbins"] = Value::from(6);
        }
        let b = data::bin_params(&spec, ext);
        let name = format!("bin_{f}");
        let end = format!("bin_{f}_end");
        let sc = table.add_col(&name);
        let ec = table.add_col(&end);
        for row in table.rows.iter_mut() {
            let s = row[c].num().and_then(|v| data::bin_of(&b, v));
            row[sc] = s.map(V::Num).unwrap_or(V::Null);
            row[ec] = s.map(|s| V::Num(s + b.step)).unwrap_or(V::Null);
        }
        ch.field = Some(name);
        ch.field2 = Some(end);
        ch.bins = Some(b);
    }
    // ---- aggregate
    if enc.values().any(|c| c.agg.is_some()) {
        let mut groupby: Vec<String> = Vec::new();
        let mut measures: Vec<(String, Option<String>, String)> = Vec::new();
        let mut keys_sorted = keys.clone();
        keys_sorted.sort();
        for k in &keys_sorted {
            let ch = &enc[k];
            match &ch.agg {
                Some(op) => {
                    if matches!(op.as_str(), "argmin" | "argmax" | "ci0" | "ci1") {
                        return None;
                    }
                    let out = data::agg_output_name(op, ch.raw_field.as_deref());
                    if !measures.iter().any(|m| m.2 == out) {
                        measures.push((op.clone(), ch.raw_field.clone(), out));
                    }
                }
                None => {
                    for f in [&ch.field, &ch.field2].into_iter().flatten() {
                        if !groupby.contains(f) {
                            groupby.push(f.clone());
                        }
                    }
                }
            }
        }
        table = data::group_aggregate(&table, &groupby, &measures)?;
        for k in &keys {
            let ch = enc.get_mut(k)?;
            if let Some(op) = &ch.agg {
                ch.field = Some(data::agg_output_name(op, ch.raw_field.as_deref()));
            }
        }
    }
    // ---- orientation (bar/area/line/tick)
    let xq = enc.get("x").is_some_and(|c| c.ty == Ty::Q && c.bins.is_none() && !c.binned && c.has_field());
    let yq = enc.get("y").is_some_and(|c| c.ty == Ty::Q && c.bins.is_none() && !c.binned && c.has_field());
    let x_dim = enc.get("x").is_some_and(|c| c.has_field() && !xq);
    let y_dim = enc.get("y").is_some_and(|c| c.has_field() && !yq);
    let orient_h = match mark.props.get("orient").and_then(|o| o.as_str()) {
        Some("horizontal") => true,
        Some("vertical") => false,
        _ => {
            if xq && !yq {
                // quantitative x, discrete (or absent) y: horizontal bars
                (y_dim || !enc.contains_key("y")) && mark.ty != "line" && mark.ty != "area" || (y_dim && (mark.ty == "line" || mark.ty == "area") && !x_dim)
            } else if xq && yq {
                // both quantitative: aggregated one is the measure
                enc.get("x").is_some_and(|c| c.agg.is_some()) && !enc.get("y").is_some_and(|c| c.agg.is_some())
            } else {
                false
            }
        }
    };
    // ---- stack
    let mut stack = None;
    let stackable = matches!(mark.ty.as_str(), "bar" | "area" | "arc");
    let measure = if mark.ty == "arc" {
        "theta"
    } else if orient_h {
        "x"
    } else {
        "y"
    };
    let stack_spec = enc.get(measure).and_then(|c| c.def.get("stack")).cloned();
    let explicit = match &stack_spec {
        Some(Value::Null) | Some(Value::Bool(false)) => Some(false),
        Some(Value::Bool(true)) | Some(Value::String(_)) => Some(true),
        _ => None,
    };
    let m2 = enc.get(&format!("{measure}2")).is_some_and(|c| c.has_field());
    // Grouped bars (x/yOffset) sit side by side instead of stacking.
    let offset_ch = if orient_h { "yOffset" } else { "xOffset" };
    let explicit = if enc.get(offset_ch).is_some_and(|c| c.has_field()) && explicit.is_none() { Some(false) } else { explicit };
    if let Some(mc) = enc.get(measure)
        && mc.has_field()
        && mc.ty == Ty::Q
        && mc.bins.is_none()
        && !mc.binned
        && !m2
        && explicit.unwrap_or(stackable)
    {
        let offset = match &stack_spec {
            Some(Value::String(s)) => s.clone(),
            _ => "zero".to_string(),
        };
        let dim_ch = if mark.ty == "arc" {
            None
        } else if orient_h {
            Some("y")
        } else {
            Some("x")
        };
        let mut groupby: Vec<String> = Vec::new();
        if let Some(d) = dim_ch.and_then(|d| enc.get(d)).and_then(|c| c.field.clone()) {
            groupby.push(d);
        }
        // Stack order: by the stack-by fields (descending when y is the
        // measure, ascending for x and theta, as Vega-Lite does) or the
        // `order` channel.
        let mut sort_fields: Vec<(String, bool)> = Vec::new();
        if let Some(o) = enc.get("order").and_then(|c| c.field.clone()) {
            let desc = enc["order"].def.get("sort").and_then(|s| s.as_str()) == Some("descending");
            sort_fields.push((o, desc));
        } else {
            for c in ["color", "fill", "stroke", "opacity", "fillOpacity", "strokeOpacity", "size", "shape", "strokeDash", "detail"] {
                for (k, ch) in enc.iter() {
                    if (k == c || k.starts_with(&format!("{c}#"))) && ch.agg.is_none()
                        && let Some(f) = &ch.field
                        && !sort_fields.iter().any(|s| &s.0 == f)
                    {
                        sort_fields.push((f.clone(), measure == "y"));
                    }
                }
            }
        }
        let field = mc.field.clone()?;
        let start = format!("{field}_start");
        let end = format!("{field}_end");
        if mark.ty == "area" && !sort_fields.is_empty() && !groupby.is_empty() {
            impute_zero(&mut table, &groupby[0], &sort_fields.iter().map(|s| s.0.clone()).collect::<Vec<_>>(), &field);
        }
        stack_table(&mut table, &groupby, &sort_fields, &field, &start, &end, &offset)?;
        stack = Some((measure.to_string(), start, end));
    }
    if let Some(c) = enc.get("y").filter(|_| mark.ty == "arc") {
        let _ = c;
    }
    let _ = notes;
    Some(Layer {
        mark,
        enc,
        table,
        stack,
        orient_h,
    })
}

fn parse_chan(d: &Value, name: &str) -> Option<Chan> {
    let o = d.as_object()?;
    let raw_field = match o.get("field") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(f)) => {
            // {"repeat": ...} isn't supported.
            let _ = f;
            return None;
        }
        _ => None,
    };
    let agg = match o.get("aggregate") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(_)) => return None,
        _ => None,
    };
    let bin = match o.get("bin") {
        Some(Value::Bool(true)) => Some(Value::Object(Default::default())),
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(Value::String(s)) if s == "binned" => Some(Value::String("binned".into())),
        _ => None,
    };
    let binned = matches!(&bin, Some(Value::String(_)));
    let units = match o.get("timeUnit") {
        Some(Value::String(s)) => Some(data::time_units(s)?),
        Some(Value::Object(t)) => match t.get("unit").and_then(|u| u.as_str()) {
            Some(u) => Some(data::time_units(u)?),
            None => None,
        },
        _ => None,
    };
    let value = o.get("value").or_else(|| o.get("datum")).cloned();
    let ty = match ty_of(d) {
        Some(t) => t,
        None => {
            if agg.as_deref() == Some("count") || bin.is_some() {
                Ty::Q
            } else if units.is_some() {
                Ty::T
            } else if raw_field.is_some() {
                Ty::N
            } else if matches!(o.get("datum"), Some(Value::Number(_))) {
                Ty::Q
            } else {
                Ty::N
            }
        }
    };
    let title = match o.get("title") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => Some(a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")),
        Some(Value::Null) => Some(String::new()),
        _ => None,
    };
    let field = if agg.as_deref() == Some("count") { None } else { raw_field.clone() };
    let mut ch = Chan {
        def: d.clone(),
        field,
        field2: None,
        raw_field,
        ty,
        agg,
        bin,
        bins: None,
        binned,
        units,
        title,
        value,
    };
    if ch.agg.as_deref() == Some("count") {
        ch.field = Some("__count".into());
    }
    let _ = name;
    Some(ch)
}

/// Default axis / legend title for a channel.
pub fn default_title(ch: &Chan) -> String {
    if let Some(t) = &ch.title {
        return t.clone();
    }
    let f = ch.raw_field.clone().unwrap_or_default();
    if let Some(op) = &ch.agg {
        if op == "count" {
            return "Count of Records".into();
        }
        let op = match op.as_str() {
            "average" => "mean",
            o => o,
        };
        return format!("{} of {}", title_case(op), f);
    }
    if ch.bins.is_some() || ch.binned {
        return if ch.binned { f } else { format!("{f} (binned)") };
    }
    if let Some(u) = &ch.units {
        return format!("{} ({})", f, u.join("-"));
    }
    f
}

fn impute_zero(t: &mut Table, key: &str, series: &[String], field: &str) {
    let (Some(kc), Some(fc)) = (t.col(key), t.col(field)) else { return };
    let sc: Vec<usize> = series.iter().filter_map(|s| t.col(s)).collect();
    let mut keys: Vec<V> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in &t.rows {
        if seen.insert(r[kc].key()) {
            keys.push(r[kc].clone());
        }
    }
    let mut series_rows: HashMap<Vec<String>, (Vec<V>, std::collections::HashSet<String>)> = HashMap::new();
    for r in &t.rows {
        let s: Vec<String> = sc.iter().map(|&c| r[c].key()).collect();
        let e = series_rows.entry(s).or_insert_with(|| (r.clone(), Default::default()));
        e.1.insert(r[kc].key());
    }
    if keys.len() * series_rows.len() > 2_000_000 {
        return;
    }
    let mut add = Vec::new();
    for (_, (proto, present)) in series_rows {
        for k in &keys {
            if !present.contains(&k.key()) {
                let mut row = proto.clone();
                row[kc] = k.clone();
                row[fc] = V::Num(0.);
                add.push(row);
            }
        }
    }
    t.rows.extend(add);
}

fn stack_table(t: &mut Table, groupby: &[String], sort: &[(String, bool)], field: &str, start: &str, end: &str, offset: &str) -> Option<()> {
    let gc: Vec<usize> = groupby.iter().filter_map(|g| t.col(g)).collect();
    let sc: Vec<(usize, bool)> = sort.iter().filter_map(|(f, d)| t.col(f).map(|c| (c, *d))).collect();
    let fc = t.col(field)?;
    let s_col = t.add_col(start);
    let e_col = t.add_col(end);
    let mut order: Vec<usize> = (0..t.rows.len()).collect();
    order.sort_by(|&a, &b| {
        for &g in &gc {
            let c = cmp_v(&t.rows[a][g], &t.rows[b][g]);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        for &(c, desc) in &sc {
            let o = cmp_v(&t.rows[a][c], &t.rows[b][c]);
            let o = if desc { o.reverse() } else { o };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.cmp(&b)
    });
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j < order.len() && gc.iter().all(|&g| t.rows[order[j]][g] == t.rows[order[i]][g]) {
            j += 1;
        }
        let part = &order[i..j];
        let total: f64 = part.iter().filter_map(|&r| t.rows[r][fc].num()).map(f64::abs).sum();
        let (mut pos, mut neg) = (0., 0.);
        let shift = if offset == "center" { -total / 2. } else { 0. };
        for &r in part {
            let v = t.rows[r][fc].num().unwrap_or(0.);
            let v = if offset == "normalize" { if total > 0. { v / total } else { 0. } } else { v };
            let (s, e) = if v < 0. && offset != "center" {
                let s = neg;
                neg += v;
                (s, neg)
            } else {
                let s = pos;
                pos += v.abs();
                (s + shift, pos + shift)
            };
            t.rows[r][s_col] = V::Num(s);
            t.rows[r][e_col] = V::Num(e);
        }
        i = j;
    }
    Some(())
}

// ---------------------------------------------------------------- scales

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Linear,
    Log,
    Pow(u8),
    Time,
    Band,
    Point,
}

#[derive(Clone, Debug)]
pub struct Pos {
    pub kind: Kind,
    pub ty: Ty,
    pub dom: (f64, f64),
    pub cats: Vec<V>,
    pub range: (f64, f64),
    pub pad_inner: f64,
    pub pad_outer: f64,
    pub bins: Option<Bins>,
    pub units: Option<Vec<&'static str>>,
    pub exponent: f64,
    pub title: String,
    pub axis: Option<Value>,
    pub format: Option<String>,
    pub format_time: bool,
    pub reverse: bool,
}

impl Pos {
    pub fn n(&self) -> usize {
        self.cats.len()
    }
    pub fn discrete(&self) -> bool {
        matches!(self.kind, Kind::Band | Kind::Point)
    }
    fn band_geom(&self) -> (f64, f64, f64) {
        // (start, step, bandwidth) d3 band/point with align 0.5
        let (r0, r1) = self.range;
        let (lo, hi) = (r0.min(r1), r0.max(r1));
        let n = self.n() as f64;
        let (pi, po) = if self.kind == Kind::Point { (1., self.pad_outer) } else { (self.pad_inner, self.pad_outer) };
        let space = (n - pi + 2. * po).max(1.);
        let step = (hi - lo) / space;
        let start = lo + (hi - lo - step * (n - pi)) * 0.5;
        let bw = if self.kind == Kind::Point { 0. } else { step * (1. - pi) };
        (start, step, bw)
    }
    pub fn bandwidth(&self) -> f64 {
        if self.discrete() { self.band_geom().2 } else { 0. }
    }
    fn tr(&self, v: f64) -> Option<f64> {
        match self.kind {
            Kind::Log => (v > 0.).then(|| v.log10()),
            Kind::Pow(_) => Some(v.signum() * v.abs().powf(self.exponent)),
            _ => Some(v),
        }
    }
    /// Continuous value -> px.
    pub fn map_num(&self, v: f64) -> Option<f64> {
        if !v.is_finite() {
            return None;
        }
        let (d0, d1) = (self.tr(self.dom.0)?, self.tr(self.dom.1)?);
        let t = self.tr(v)?;
        let (r0, r1) = if self.reverse { (self.range.1, self.range.0) } else { self.range };
        if d1 == d0 {
            return Some((r0 + r1) / 2.);
        }
        Some(r0 + (t - d0) / (d1 - d0) * (r1 - r0))
    }
    /// Band start (discrete) or position (continuous).
    pub fn map(&self, v: &V) -> Option<f64> {
        if self.discrete() {
            let k = v.key();
            let i = self.cats.iter().position(|c| c.key() == k)?;
            let (start, step, _) = self.band_geom();
            let i = if self.reverse_discrete() { self.n() - 1 - i } else { i };
            let pos = start + step * i as f64;
            // y ranges run bottom-up for continuous, top-down for bands.
            return Some(pos);
        }
        self.map_num(v.num()?)
    }
    fn reverse_discrete(&self) -> bool {
        self.reverse
    }
    /// Center of the band (point marks on band scales).
    pub fn center(&self, v: &V) -> Option<f64> {
        let p = self.map(v)?;
        Some(if self.discrete() { p + self.bandwidth() / 2. } else { p })
    }

    /// Ticks: (value, px, label).
    pub fn ticks(&self, count: f64, theme: &Theme) -> Vec<(f64, f64, String)> {
        let fmt_q = |vals: &[f64], step: f64| -> Vec<String> {
            match &self.format {
                Some(f) if self.format_time || self.ty == Ty::T => vals.iter().map(|v| time::strftime(f, *v)).collect(),
                Some(f) => vals.iter().map(|v| format::d3(f, *v)).collect(),
                None => {
                    let p = ticks::d3_precision_fixed(step);
                    let spec = format!(",.{p}f");
                    vals.iter().map(|v| format::d3(&spec, *v)).collect()
                }
            }
        };
        let _ = theme;
        if self.discrete() {
            return self
                .cats
                .iter()
                .filter_map(|c| {
                    let label = match (&self.units, &self.format, c) {
                        (Some(u), None, V::Num(t)) => time::strftime(&data::time_unit_format(u), *t),
                        (_, Some(f), V::Num(t)) if self.ty == Ty::T || self.format_time => time::strftime(f, *t),
                        (_, Some(f), V::Num(n)) => format::d3(f, *n),
                        _ => c.label(),
                    };
                    Some((0., self.center(c)?, label))
                })
                .collect();
        }
        let (lo, hi) = (self.dom.0.min(self.dom.1), self.dom.0.max(self.dom.1));
        if let Some(b) = &self.bins {
            let mut vals = Vec::new();
            let mut v = b.start;
            while v <= b.stop + b.step * 1e-9 && vals.len() < 1000 {
                vals.push(v);
                v = b.start + b.step * vals.len() as f64;
            }
            let k = ((vals.len() as f64) / count.max(1.)).ceil().max(1.) as usize;
            let vals: Vec<f64> = vals.into_iter().step_by(k).collect();
            let labels = fmt_q(&vals, b.step * k as f64);
            return vals.iter().zip(labels).filter_map(|(v, l)| Some((*v, self.map_num(*v)?, l))).collect();
        }
        match self.kind {
            Kind::Time => {
                let mut ts = ticks::d3_time_ticks(lo, hi, count);
                if let Some(u) = &self.units {
                    // tickMinStep: at least one unit apart.
                    let min_step = data::time_unit_end(u, 0.) - 0.;
                    if ts.len() > 1 && ts[1] - ts[0] < min_step * 0.99 {
                        let mut kept: Vec<f64> = Vec::new();
                        for t in ts {
                            let t = data::time_floor(u, t);
                            if kept.last().is_none_or(|l| t - l >= min_step * 0.99) && t >= lo && t <= hi {
                                kept.push(t);
                            }
                        }
                        ts = kept;
                    }
                }
                ts.into_iter()
                    .filter_map(|t| {
                        let label = match (&self.format, &self.units) {
                            (Some(f), _) => time::strftime(f, t),
                            (None, Some(u)) => time::strftime(&data::time_unit_format(u), t),
                            _ => ticks::vega_time_label(t),
                        };
                        Some((t, self.map_num(t)?, label))
                    })
                    .collect()
            }
            Kind::Log => {
                let vals = ticks::d3_log_ticks(lo, hi, count);
                // d3 log tickFormat: "~s"-style labels, only for "nice" mantissas.
                let k = (10. * count / vals.len().max(1) as f64).max(1.);
                vals.iter()
                    .filter_map(|&v| {
                        let mut i = v / 10f64.powf(v.log10().round());
                        if i * 10. < 10. - 0.5 {
                            i *= 10.;
                        }
                        let label = if i <= k {
                            match &self.format {
                                Some(f) => format::d3(f, v),
                                None => format::d3(",", v),
                            }
                        } else {
                            String::new()
                        };
                        Some((v, self.map_num(v)?, label))
                    })
                    .collect()
            }
            _ => {
                let vals = ticks::d3_ticks(lo, hi, count);
                let step = ticks::d3_tick_step(lo, hi, count);
                let labels = fmt_q(&vals, step);
                vals.iter().zip(labels).filter_map(|(v, l)| Some((*v, self.map_num(*v)?, l))).collect()
            }
        }
    }
}

/// Collects every value a channel takes across layers.
fn values_of<'a>(layers: &'a [&'a Layer], ch: &'a str) -> impl Iterator<Item = (&'a Layer, &'a V)> + 'a {
    values_of_ext(layers, ch, true)
}

/// Like `values_of`; `ends` = include bin / time-unit ends (continuous scales).
fn values_of_ext<'a>(layers: &'a [&'a Layer], ch: &'a str, ends: bool) -> impl Iterator<Item = (&'a Layer, &'a V)> + 'a {
    layers.iter().flat_map(move |l| {
        let mut cols: Vec<usize> = Vec::new();
        // Bin / time-unit ends only widen the domain of marks spanning them.
        let spans = ends && matches!(l.mark.ty.as_str(), "bar" | "rect");
        if let Some(c) = l.ch(ch) {
            let f2 = if spans || (ends && c.bins.is_some()) { c.field2.as_ref() } else { None };
            for f in [c.field.as_ref(), f2].into_iter().flatten() {
                if let Some(i) = l.table.col(f) {
                    cols.push(i);
                }
            }
        }
        if let Some((m, s, e)) = &l.stack
            && m == ch
        {
            cols.clear();
            cols.extend(l.table.col(s));
            cols.extend(l.table.col(e));
        }
        // x2/y2 extend the x/y scale.
        let ch2 = format!("{ch}2");
        if matches!(ch, "x" | "y" | "theta" | "radius")
            && let Some(c2) = l.ch(&ch2)
            && let Some(f) = &c2.field
            && let Some(i) = l.table.col(f)
        {
            cols.push(i);
        }
        let n = l.table.rows.len();
        (0..n).flat_map(move |r| {
            let cols = cols.clone();
            cols.into_iter().map(move |c| (*l, l.table.get(r, c)))
        })
    })
}

fn sort_discrete(vals: &mut [V], sort: Option<&Value>, layers: &[&Layer], ch: &str) -> Option<()> {
    match sort {
        None => vals.sort_by(cmp_v),
        Some(Value::String(s)) if s == "ascending" => vals.sort_by(cmp_v),
        Some(Value::String(s)) if s == "descending" => {
            vals.sort_by(cmp_v);
            vals.reverse();
        }
        Some(Value::Null) => {}
        Some(Value::Array(order)) => {
            let order: Vec<String> = order.iter().map(|v| V::from_json(v).key()).collect();
            vals.sort_by(|a, b| {
                let ia = order.iter().position(|o| *o == a.key()).unwrap_or(usize::MAX);
                let ib = order.iter().position(|o| *o == b.key()).unwrap_or(usize::MAX);
                ia.cmp(&ib).then_with(|| cmp_v(a, b))
            });
        }
        Some(Value::String(s)) => {
            // Sort by another encoding channel: "-y", "x", "color"...
            let (desc, other) = match s.strip_prefix('-') {
                Some(o) => (true, o),
                None => (false, s.as_str()),
            };
            sort_by_field(vals, layers, ch, other, None, desc)?;
        }
        Some(Value::Object(o)) => {
            let desc = o.get("order").and_then(|x| x.as_str()) == Some("descending");
            if let Some(f) = o.get("field").and_then(|x| x.as_str()) {
                let op = o.get("op").and_then(|x| x.as_str()).unwrap_or("sum");
                sort_by_field(vals, layers, ch, "", Some((f, op)), desc)?;
            } else if o.get("op").and_then(|x| x.as_str()) == Some("count") {
                sort_by_field(vals, layers, ch, "", Some(("__rows", "count")), desc)?;
            } else {
                vals.sort_by(cmp_v);
                if desc {
                    vals.reverse();
                }
            }
        }
        _ => vals.sort_by(cmp_v),
    }
    Some(())
}

fn sort_by_field(vals: &mut [V], layers: &[&Layer], ch: &str, other_ch: &str, field_op: Option<(&str, &str)>, desc: bool) -> Option<()> {
    let l = layers.iter().find(|l| l.ch(ch).is_some_and(|c| c.field.is_some()))?;
    let key_col = l.table.col(l.ch(ch)?.field.as_ref()?)?;
    let (val_col, op) = match field_op {
        Some((f, op)) => {
            // Aggregated field names after encoding aggregation: try both.
            let agg_name = data::agg_output_name(op, Some(f));
            (l.table.col(&agg_name).or_else(|| l.table.col(f)), op)
        }
        None => {
            let oc = l.ch(other_ch)?;
            (oc.field.as_ref().and_then(|f| l.table.col(f)), "sum")
        }
    };
    let mut agg: HashMap<String, Vec<V>> = HashMap::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for r in 0..l.table.rows.len() {
        let k = l.table.get(r, key_col).key();
        *counts.entry(k.clone()).or_insert(0) += 1;
        if let Some(vc) = val_col {
            agg.entry(k).or_default().push(l.table.get(r, vc).clone());
        }
    }
    let score = |v: &V| -> f64 {
        let k = v.key();
        let vals = agg.get(&k).cloned().unwrap_or_default();
        let n = *counts.get(&k).unwrap_or(&0);
        data::aggregate(op, &vals, n).and_then(|x| x.num()).unwrap_or(f64::NEG_INFINITY)
    };
    vals.sort_by(|a, b| {
        let o = score(a).total_cmp(&score(b));
        if desc { o.reverse() } else { o }
    });
    Some(())
}

/// Builds the position scale for `ch` ("x" | "y") over the layers.
pub fn pos_scale(layers: &[&Layer], ch: &str, cfg: &Config) -> Option<Option<Pos>> {
    let defs: Vec<(&Layer, &Chan)> = layers.iter().filter_map(|l| l.ch(ch).filter(|c| c.has_field() || c.value.is_some()).map(|c| (*l, c))).collect();
    let field_defs: Vec<(&Layer, &Chan)> = defs.iter().filter(|(_, c)| c.has_field()).copied().collect();
    // Datum-only / stacked-only channels still need a scale.
    let Some(&(l0, c0)) = field_defs.first().or(defs.first()) else {
        return Some(None);
    };
    let spec_ty = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("type").and_then(|t| t.as_str()));
    let any_band_mark = field_defs.iter().any(|(l, _)| matches!(l.mark.ty.as_str(), "bar" | "rect" | "tick" | "arc"));
    let ty = c0.ty;
    let mut kind = match spec_ty {
        Some("log") => Kind::Log,
        Some("pow") => Kind::Pow(2),
        Some("sqrt") => Kind::Pow(1),
        Some("linear") => Kind::Linear,
        Some("time") | Some("utc") => Kind::Time,
        Some("band") => Kind::Band,
        Some("point") => Kind::Point,
        Some("symlog") | Some("quantile") | Some("quantize") | Some("threshold") | Some("bin-ordinal") => return None,
        _ => match ty {
            Ty::Q => Kind::Linear,
            Ty::T => Kind::Time,
            Ty::N | Ty::O => {
                if any_band_mark {
                    Kind::Band
                } else {
                    Kind::Point
                }
            }
        },
    };
    if ty.discrete() && !matches!(kind, Kind::Band | Kind::Point) {
        kind = if any_band_mark { Kind::Band } else { Kind::Point };
    }
    let exponent = match kind {
        Kind::Pow(1) => 0.5,
        Kind::Pow(_) => c0.scale("exponent").and_then(|v| v.as_f64()).unwrap_or(1.),
        _ => 1.,
    };
    let scale = |k: &str| layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale(k));
    let reverse = scale("reverse").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut titles: Vec<String> = Vec::new();
    // x2/y2 titles join the axis title when they differ.
    let ch2 = format!("{ch}2");
    for (l, c) in &field_defs {
        for c in std::iter::once(*c).chain(l.ch(&ch2).filter(|c2| c2.has_field())) {
            let t = default_title(c);
            if !t.is_empty() && !titles.contains(&t) {
                titles.push(t);
            }
        }
    }
    let axis = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.def.get("axis").cloned());
    let normalized = layers.iter().any(|l| {
        l.stack.as_ref().is_some_and(|(m, _, _)| m == ch) && l.ch(ch).and_then(|c| c.def.get("stack")).and_then(|s| s.as_str()) == Some("normalize")
    });
    let format = axis
        .as_ref()
        .and_then(|a| a.get("format"))
        .and_then(|f| f.as_str())
        .map(str::to_string)
        .or_else(|| normalized.then(|| ".0%".to_string()));
    let format_time = axis.as_ref().and_then(|a| a.get("formatType")).and_then(|f| f.as_str()) == Some("time");
    let mut p = Pos {
        kind,
        ty,
        dom: (0., 1.),
        cats: Vec::new(),
        range: (0., 1.),
        pad_inner: 0.,
        pad_outer: 0.,
        bins: None,
        units: field_defs.iter().find_map(|(_, c)| c.units.clone()),
        exponent,
        title: titles.join(", "),
        axis,
        format,
        format_time,
        reverse,
    };
    if matches!(kind, Kind::Band | Kind::Point) {
        let mut seen = std::collections::HashSet::new();
        let mut vals: Vec<V> = Vec::new();
        if let Some(Value::Array(d)) = scale("domain") {
            vals = d.iter().map(V::from_json).collect();
        } else {
            for (_, v) in values_of_ext(layers, ch, false) {
                if v.is_valid() && seen.insert(v.key()) {
                    vals.push(v.clone());
                }
            }
            let sort = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.def.get("sort"));
            sort_discrete(&mut vals, sort, layers, ch)?;
        }
        p.cats = vals;
        let has_offset = layers.iter().any(|l| l.ch(&format!("{ch}Offset")).is_some_and(|c| c.has_field()));
        let bar = field_defs.iter().any(|(l, _)| l.mark.ty == "bar");
        let rect = field_defs.iter().all(|(l, _)| l.mark.ty == "rect");
        if kind == Kind::Band {
            let pi = scale("paddingInner")
                .or(scale("padding"))
                .and_then(|v| v.as_f64())
                .unwrap_or(if has_offset { 0.2 } else if rect { 0. } else if bar { cfg.bar_band_padding } else { 0.1 });
            let po = scale("paddingOuter")
                .or(scale("padding"))
                .and_then(|v| v.as_f64())
                .unwrap_or(if has_offset { 0.2 } else if rect { 0. } else { pi / 2. });
            p.pad_inner = pi;
            p.pad_outer = po;
        } else {
            p.pad_outer = scale("padding").and_then(|v| v.as_f64()).unwrap_or(0.5);
            p.pad_inner = 1.;
        }
        let _ = l0;
        return Some(Some(p));
    }
    // continuous
    let explicit = scale("domain");
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (_, v) in values_of(layers, ch) {
        let x = if kind == Kind::Time { v.date() } else { v.num() };
        if let Some(x) = x
            && (kind != Kind::Log || x > 0.)
        {
            lo = lo.min(x);
            hi = hi.max(x);
        }
    }
    // constant datum/value positions on the scale
    for (_, c) in &defs {
        if c.field.is_none()
            && let Some(d) = c.def.get("datum")
        {
            let x = if kind == Kind::Time { V::from_json(d).date().or_else(|| data::datetime_obj(d)) } else { V::from_json(d).num() };
            if let Some(x) = x {
                lo = lo.min(x);
                hi = hi.max(x);
            }
        }
    }
    let binned = field_defs.iter().find_map(|(_, c)| c.bins.clone());
    let mut custom = false;
    match explicit {
        Some(Value::Array(d)) if d.len() >= 2 => {
            let conv = |v: &Value| -> Option<f64> {
                match v {
                    Value::Number(n) => n.as_f64(),
                    Value::String(_) => V::from_json(v).date(),
                    Value::Object(_) => data::datetime_obj(v),
                    _ => None,
                }
            };
            let a = conv(&d[0]);
            let b = conv(&d[d.len() - 1]);
            if let (Some(a), Some(b)) = (a, b) {
                lo = a;
                hi = b;
                custom = true;
            }
        }
        Some(Value::String(s)) if s == "unaggregated" => {}
        Some(_) => return None,
        None => {}
    }
    if let Some(m) = scale("domainMin").and_then(|v| v.as_f64()) {
        lo = m;
        custom = true;
    }
    if let Some(m) = scale("domainMax").and_then(|v| v.as_f64()) {
        hi = m;
        custom = true;
    }
    if !(lo.is_finite() && hi.is_finite()) {
        (lo, hi) = if kind == Kind::Log { (1., 10.) } else { (0., 1.) };
    }
    if let Some(b) = &binned
        && !custom
    {
        lo = b.start;
        hi = b.stop;
        p.bins = Some(b.clone());
    }
    // zero
    if kind == Kind::Linear || matches!(kind, Kind::Pow(_)) {
        let zero_spec = scale("zero").and_then(|v| v.as_bool());
        let default_zero = !custom && binned.is_none() && ty == Ty::Q && {
            let l = field_defs.first().map(|(l, _)| *l).unwrap_or(l0);
            let dim = matches!(l.mark.ty.as_str(), "bar" | "area" | "line" | "trail")
                && ((l.orient_h && ch == "y") || (!l.orient_h && ch == "x"));
            !dim && cfg.scale_zero
        };
        if zero_spec.unwrap_or(default_zero) {
            lo = lo.min(0.);
            hi = hi.max(0.);
        }
    }
    // continuousPadding: bars on a continuous dimension get 5px (their width) of room.
    let bar_dim = field_defs.iter().any(|(l, c)| {
        matches!(l.mark.ty.as_str(), "bar" | "tick") && c.bins.is_none() && c.units.is_none() && !c.binned && ((l.orient_h && ch == "y") || (!l.orient_h && ch == "x"))
    });
    if bar_dim && matches!(kind, Kind::Linear | Kind::Time) && hi > lo {
        let len = if ch == "x" { cfg.continuous_w } else { cfg.continuous_h };
        let pad = scale("padding").and_then(|v| v.as_f64()).unwrap_or(5.);
        if len > 2. * pad {
            let span = (hi - lo) * len / (len - 2. * pad);
            let mid = (lo + hi) / 2.;
            lo = mid - span / 2.;
            hi = mid + span / 2.;
        }
    }
    let nice_default = !custom && binned.is_none() && matches!(kind, Kind::Linear | Kind::Log | Kind::Pow(_));
    let nice = match scale("nice") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(_)) => true,
        Some(Value::String(_)) => kind == Kind::Time,
        _ => nice_default,
    };
    if nice {
        match kind {
            Kind::Log => {
                lo = 10f64.powf(lo.log10().floor());
                hi = 10f64.powf(hi.log10().ceil());
            }
            Kind::Time => {
                let (u, _) = ticks::d3_time_interval(lo, hi, 10.);
                lo = ticks::time_floor(lo, u);
                let f = ticks::time_floor(hi, u);
                if f < hi {
                    hi = data::time_unit_end(&[time_unit_name(u)], f);
                }
            }
            Kind::Pow(_) => {
                let (a, b) = ticks::d3_nice(p_tr(lo, exponent), p_tr(hi, exponent), 10.);
                lo = p_inv(a, exponent);
                hi = p_inv(b, exponent);
            }
            _ => {
                let count = scale("nice").and_then(|v| v.as_f64()).unwrap_or(10.);
                (lo, hi) = ticks::d3_nice(lo, hi, count);
            }
        }
    }
    if lo == hi {
        if kind == Kind::Time {
            lo -= time::DAY;
            hi += time::DAY;
        } else if kind == Kind::Log {
            lo /= 10.;
            hi *= 10.;
        } else {
            lo -= 0.5;
            hi += 0.5;
            if nice {
                (lo, hi) = ticks::d3_nice(lo, hi, 10.);
            }
        }
    }
    p.dom = (lo, hi);
    Some(Some(p))
}

fn p_tr(v: f64, e: f64) -> f64 {
    v.signum() * v.abs().powf(e)
}
fn p_inv(v: f64, e: f64) -> f64 {
    v.signum() * v.abs().powf(1. / e)
}

fn time_unit_name(u: ticks::TimeUnit) -> &'static str {
    match u {
        ticks::TimeUnit::Millisecond => "milliseconds",
        ticks::TimeUnit::Second => "seconds",
        ticks::TimeUnit::Minute => "minutes",
        ticks::TimeUnit::Hour => "hours",
        ticks::TimeUnit::Day => "date",
        ticks::TimeUnit::Week => "week",
        ticks::TimeUnit::Month => "month",
        ticks::TimeUnit::Year => "year",
    }
}

// ---------------------------------------------------------------- non-position scales

#[derive(Clone, Debug)]
pub enum ColorScale {
    Ordinal { domain: Vec<V>, colors: Vec<Rgba> },
    Seq { lo: f64, hi: f64, scale: Colorscale, log: bool },
}

impl ColorScale {
    pub fn color(&self, v: &V) -> Option<Rgba> {
        match self {
            ColorScale::Ordinal { domain, colors } => {
                let k = v.key();
                let i = domain.iter().position(|d| d.key() == k)?;
                Some(colors[i % colors.len().max(1)])
            }
            ColorScale::Seq { lo, hi, scale, log } => {
                let x = v.num().or_else(|| v.date())?;
                let t = if *log {
                    if x <= 0. || *lo <= 0. {
                        return None;
                    }
                    (x.log10() - lo.log10()) / (hi.log10() - lo.log10())
                } else {
                    (x - lo) / (hi - lo)
                };
                Some(scale.at(if t.is_finite() { t } else { 0.5 }))
            }
        }
    }
}

/// Collects a discrete domain for a non-position channel over layers.
fn discrete_domain(layers: &[&Layer], ch: &str) -> Option<Vec<V>> {
    if let Some(Value::Array(d)) = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("domain")) {
        return Some(d.iter().map(V::from_json).collect());
    }
    let mut seen = std::collections::HashSet::new();
    let mut vals = Vec::new();
    for (_, v) in values_of(layers, ch) {
        if v.is_valid() && seen.insert(v.key()) {
            vals.push(v.clone());
        }
    }
    let sort = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.def.get("sort"));
    sort_discrete(&mut vals, sort, layers, ch)?;
    Some(vals)
}

fn continuous_domain(layers: &[&Layer], ch: &str, zero: bool) -> (f64, f64) {
    if let Some(Value::Array(d)) = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("domain"))
        && d.len() >= 2
        && let (Some(a), Some(b)) = (d[0].as_f64(), d[d.len() - 1].as_f64())
    {
        return (a, b);
    }
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (_, v) in values_of(layers, ch) {
        if let Some(x) = v.num().or_else(|| v.date()) {
            lo = lo.min(x);
            hi = hi.max(x);
        }
    }
    if !(lo.is_finite() && hi.is_finite()) {
        return (0., 1.);
    }
    if zero {
        lo = lo.min(0.);
        hi = hi.max(0.);
    }
    if lo == hi {
        hi = lo + 1.;
    }
    (lo, hi)
}

pub fn color_scale(layers: &[&Layer], ch: &str, cfg: &Config) -> Option<Option<ColorScale>> {
    let Some(c) = layers.iter().filter_map(|l| l.ch(ch)).find(|c| c.has_field()) else {
        return Some(None);
    };
    let scheme = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("scheme"));
    let range = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("range"));
    let scheme_name = match scheme {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get("name").and_then(|n| n.as_str()).map(str::to_string),
        _ => None,
    };
    let range_colors: Option<Vec<Rgba>> = match range {
        Some(Value::Array(a)) => Some(a.iter().filter_map(|x| x.as_str().and_then(Rgba::parse)).collect()),
        _ => None,
    };
    let scale_ty = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("type").and_then(|t| t.as_str()));
    if matches!(scale_ty, Some("quantile" | "quantize" | "threshold" | "bin-ordinal" | "symlog" | "pow" | "sqrt")) {
        return None;
    }
    let is_rect = layers.iter().any(|l| l.ch(ch).is_some() && l.mark.ty == "rect");
    if c.ty.discrete() && !(c.ty == Ty::O && c.bins.is_some()) {
        let domain = discrete_domain(layers, ch)?;
        let colors = if let Some(rc) = range_colors.filter(|v| !v.is_empty()) {
            rc
        } else if c.ty == Ty::O && scheme_name.is_none() {
            // ordinal ramp: blues sampled across the domain
            // vega's quantizeInterpolator: samples at i / (n + 1)
            let s = cfg.ordinal_scheme();
            let n = domain.len().max(1);
            (0..n).map(|i| s.at((i + 1) as f64 / (n + 1) as f64)).collect()
        } else {
            let name = scheme_name.clone().unwrap_or_else(|| cfg.category_scheme.clone());
            match crate::color::vega_discrete(&name) {
                Some(c) => c,
                None => {
                    let s = Colorscale::vega_named(&name)?;
                    let n = domain.len().max(1);
                    (0..n).map(|i| s.at(if n == 1 { 0.5 } else { i as f64 / (n - 1) as f64 })).collect()
                }
            }
        };
        if colors.is_empty() {
            return None;
        }
        return Some(Some(ColorScale::Ordinal { domain, colors }));
    }
    let log = scale_ty == Some("log");
    let zero = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("zero").and_then(|z| z.as_bool())).unwrap_or(false);
    let (lo, hi) = continuous_domain(layers, ch, zero);
    let scale = if let Some(rc) = range_colors.filter(|v| v.len() >= 2) {
        Colorscale::from_stops(rc.iter().enumerate().map(|(i, c)| (i as f64 / (rc.len() - 1) as f64, *c)).collect())?
    } else {
        let name = scheme_name.unwrap_or_else(|| if is_rect { cfg.heatmap_scheme.clone() } else { cfg.ramp_scheme.clone() });
        Colorscale::vega_named(&name).or_else(|| crate::color::vega_discrete(&name).and_then(|c| Colorscale::from_stops(c.iter().enumerate().map(|(i, x)| (i as f64 / (c.len().max(2) - 1) as f64, *x)).collect())))?
    };
    let reverse = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("reverse").and_then(|z| z.as_bool())).unwrap_or(false);
    let scale = if reverse { scale.reversed() } else { scale };
    Some(Some(ColorScale::Seq { lo, hi, scale, log }))
}

#[derive(Clone, Debug)]
pub struct NumScale {
    pub discrete: Option<Vec<V>>,
    pub dom: (f64, f64),
    pub range: (f64, f64),
}

impl NumScale {
    pub fn map(&self, v: &V) -> Option<f64> {
        if let Some(d) = &self.discrete {
            let k = v.key();
            let i = d.iter().position(|x| x.key() == k)? as f64;
            let n = (d.len().max(2) - 1) as f64;
            return Some(self.range.0 + (self.range.1 - self.range.0) * i / n);
        }
        let x = v.num()?;
        let (d0, d1) = self.dom;
        if d1 == d0 {
            return Some((self.range.0 + self.range.1) / 2.);
        }
        Some(self.range.0 + (x - d0) / (d1 - d0) * (self.range.1 - self.range.0))
    }
}

pub fn num_scale(layers: &[&Layer], ch: &str, default_range: (f64, f64)) -> Option<Option<NumScale>> {
    let Some(c) = layers.iter().filter_map(|l| l.ch(ch)).find(|c| c.has_field()) else {
        return Some(None);
    };
    let range = layers
        .iter()
        .filter_map(|l| l.ch(ch))
        .find_map(|c| c.scale("range"))
        .and_then(|r| r.as_array())
        .and_then(|a| Some((a.first()?.as_f64()?, a.last()?.as_f64()?)))
        .unwrap_or(default_range);
    if c.ty.discrete() {
        return Some(Some(NumScale {
            discrete: Some(discrete_domain(layers, ch)?),
            dom: (0., 1.),
            range,
        }));
    }
    let zero = layers.iter().filter_map(|l| l.ch(ch)).find_map(|c| c.scale("zero").and_then(|z| z.as_bool())).unwrap_or(ch == "size");
    let (lo, hi) = continuous_domain(layers, ch, zero);
    Some(Some(NumScale { discrete: None, dom: (lo, hi), range }))
}

pub const SHAPES: [&str; 8] = ["circle", "square", "triangle-up", "cross", "diamond", "triangle-right", "triangle-down", "triangle-left"];
pub const DASHES: [&str; 5] = ["", "4,2", "2,1", "1,1", "1,2,4,2"];

// ---------------------------------------------------------------- the view

pub struct Scales {
    pub x: Option<Pos>,
    pub y: Option<Pos>,
    pub x_off: Option<Pos>,
    pub y_off: Option<Pos>,
    pub color: Option<ColorScale>,
    pub color_ch: &'static str,
    pub size: Option<NumScale>,
    pub opacity: Option<NumScale>,
    pub shape: Option<Vec<V>>,
    pub dash: Option<Vec<V>>,
    pub theta: Option<(f64, f64)>,
}

/// Sizes and scales for a (layered) view.
pub struct View {
    pub layers: Vec<Layer>,
    pub sc: Scales,
    pub width: f64,
    pub height: f64,
    pub title: Option<Value>,
}

pub fn build_view(layers: Vec<Layer>, width_spec: Option<&Value>, height_spec: Option<&Value>, cfg: &Config, avail_w: f64, title: Option<Value>) -> Option<View> {
    let refs: Vec<&Layer> = layers.iter().collect();
    let (sc, width, height) = build_scales(&refs, width_spec, height_spec, cfg, avail_w)?;
    Some(View { layers, sc, width, height, title })
}

/// Scales (and the view size they imply) over a set of layers — one
/// view's, or every facet cell's for shared facet scales.
pub fn build_scales(refs: &[&Layer], width_spec: Option<&Value>, height_spec: Option<&Value>, cfg: &Config, avail_w: f64) -> Option<(Scales, f64, f64)> {
    let refs = refs.to_vec();
    let mut x = pos_scale(&refs, "x", cfg)?;
    let mut y = pos_scale(&refs, "y", cfg)?;
    let x_off = offset_scale(&refs, "xOffset")?;
    let y_off = offset_scale(&refs, "yOffset")?;
    let color_ch = ["color", "fill", "stroke"].into_iter().find(|c| refs.iter().any(|l| l.ch(c).is_some_and(|x| x.has_field()))).unwrap_or("color");
    let color = color_scale(&refs, color_ch, cfg)?;
    let size_range = size_range(&refs, &x, &y, cfg);
    let size = num_scale(&refs, "size", size_range)?;
    let opacity = num_scale(&refs, "opacity", (0.3, 0.8))?;
    let shape = match refs.iter().filter_map(|l| l.ch("shape")).find(|c| c.has_field()) {
        Some(_) => Some(discrete_domain(&refs, "shape")?),
        None => None,
    };
    let dash = match refs.iter().filter_map(|l| l.ch("strokeDash")).find(|c| c.has_field()) {
        Some(_) => Some(discrete_domain(&refs, "strokeDash")?),
        None => None,
    };
    let theta = if refs.iter().any(|l| l.mark.ty == "arc") {
        let (lo, hi) = continuous_domain(&refs, "theta", true);
        Some((lo, hi))
    } else {
        None
    };
    // Sizes.
    let step_of = |p: &Option<Pos>, off: &Option<Pos>| -> f64 {
        let base = cfg.step;
        match off {
            Some(o) => base * (o.n() as f64 - o.pad_inner + 2. * o.pad_outer).max(1.) / (1. - p.as_ref().map(|p| p.pad_inner).unwrap_or(0.2)).max(0.1),
            None => base,
        }
    };
    let dim = |spec: Option<&Value>, p: &Option<Pos>, off: &Option<Pos>, continuous: f64| -> f64 {
        match spec {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(continuous).max(1.),
            Some(Value::String(s)) if s == "container" => avail_w,
            Some(Value::Object(o)) if o.contains_key("step") => {
                let step = o.get("step").and_then(|s| s.as_f64()).unwrap_or(20.);
                match p {
                    Some(p) if p.discrete() => {
                        let (pi, po) = if p.kind == Kind::Point { (1., p.pad_outer) } else { (p.pad_inner, p.pad_outer) };
                        step * (p.n() as f64 - pi + 2. * po).max(1.)
                    }
                    _ => continuous,
                }
            }
            _ => match p {
                Some(p) if p.discrete() => {
                    let (pi, po) = if p.kind == Kind::Point { (1., p.pad_outer) } else { (p.pad_inner, p.pad_outer) };
                    step_of(&Some(p.clone()), off) * (p.n() as f64 - pi + 2. * po).max(1.)
                }
                _ => continuous,
            },
        }
    };
    let is_arc_only = refs.iter().all(|l| l.mark.ty == "arc");
    let width = dim(width_spec, &x, &x_off, if x.is_none() && !is_arc_only { cfg.step } else { cfg.continuous_w });
    let height = dim(height_spec, &y, &y_off, if y.is_none() && !is_arc_only { cfg.step } else { cfg.continuous_h });
    let width = width.clamp(1., 20_000.);
    let height = height.clamp(1., 20_000.);
    if let Some(p) = x.as_mut() {
        p.range = (0., width);
    }
    if let Some(p) = y.as_mut() {
        p.range = if p.discrete() { (0., height) } else { (height, 0.) };
    }
    Some((
        Scales {
            x,
            y,
            x_off,
            y_off,
            color,
            color_ch,
            size,
            opacity,
            shape,
            dash,
            theta,
        },
        width,
        height,
    ))
}

fn offset_scale(layers: &[&Layer], ch: &str) -> Option<Option<Pos>> {
    let Some(c) = layers.iter().filter_map(|l| l.ch(ch)).find(|c| c.has_field()) else {
        return Some(None);
    };
    let cats = discrete_domain(layers, ch)?;
    Some(Some(Pos {
        kind: Kind::Band,
        ty: c.ty,
        dom: (0., 1.),
        cats,
        range: (0., 1.),
        pad_inner: 0.,
        pad_outer: 0.,
        bins: None,
        units: None,
        exponent: 1.,
        title: String::new(),
        axis: None,
        format: None,
        format_time: false,
        reverse: false,
    }))
}

fn size_range(layers: &[&Layer], x: &Option<Pos>, y: &Option<Pos>, cfg: &Config) -> (f64, f64) {
    let bar = layers.iter().any(|l| l.ch("size").is_some() && matches!(l.mark.ty.as_str(), "bar" | "tick"));
    let text = layers.iter().any(|l| l.ch("size").is_some() && l.mark.ty == "text");
    let line = layers.iter().any(|l| l.ch("size").is_some() && matches!(l.mark.ty.as_str(), "line" | "rule"));
    if text {
        return (8., 40.);
    }
    if line {
        return (1., 4.);
    }
    if bar {
        return (2., 20.);
    }
    let _ = (x, y, cfg);
    (4., 361.)
}

// ---------------------------------------------------------------- axes

pub struct AxisLayout {
    pub ticks: Vec<(f64, f64, String)>,
    pub visible: Vec<bool>,
    pub angle: f64,
    pub extent: f64,
    pub title: String,
    pub grid: bool,
    pub show: bool,
    pub label_size: f64,
    pub title_size: f64,
    pub flush: bool,
    pub grid_ticks: Vec<f64>,
    pub domain: bool,
    pub ticks_on: bool,
    pub labels_on: bool,
}

fn axis_get<'a>(p: &'a Pos, cfg: &'a Config, key: &str, x: bool) -> Option<&'a Value> {
    p.axis
        .as_ref()
        .and_then(|a| a.get(key))
        .filter(|v| !v.is_null())
        .or_else(|| cfg.axis_prop(key, x, p.discrete(), p.ty))
}

pub fn layout_axis(p: &Pos, x: bool, len: f64, cfg: &Config, theme: &Theme) -> AxisLayout {
    let show = !matches!(p.axis, Some(Value::Null));
    let num = |k: &str| axis_get(p, cfg, k, x).and_then(|v| v.as_f64());
    let flag = |k: &str| axis_get(p, cfg, k, x).and_then(|v| v.as_bool());
    let label_size = num("labelFontSize").unwrap_or(10.);
    let title_size = num("titleFontSize").unwrap_or(11.);
    let label_limit = num("labelLimit").unwrap_or(180.);
    let tick_count = num("tickCount").unwrap_or(if p.bins.is_some() && x { (len / 10.).ceil() } else { (len / 40.).ceil() });
    let mut ticks = p.ticks(tick_count.max(1.), theme);
    if let Some(Value::Array(vals)) = axis_get(p, cfg, "values", x) {
        let set: Vec<V> = vals.iter().map(V::from_json).collect();
        if p.discrete() {
            ticks.retain(|(_, _, l)| set.iter().any(|s| s.label() == *l));
        } else {
            let fmt = |v: f64| -> String {
                match &p.format {
                    Some(f) if p.ty == Ty::T => time::strftime(f, v),
                    Some(f) => format::d3(f, v),
                    None if p.ty == Ty::T => ticks::vega_time_label(v),
                    None => format::d3("~f", v),
                }
            };
            ticks = set
                .iter()
                .filter_map(|v| {
                    let n = if p.ty == Ty::T { v.date() } else { v.num() }?;
                    Some((n, p.map_num(n)?, fmt(n)))
                })
                .collect();
        }
    }
    // Vega-Lite rotates discrete x labels, except time units.
    let angle = num("labelAngle").unwrap_or(if x && p.discrete() && p.units.is_none() { 270. } else { 0. });
    let angle = if (angle - 270.).abs() < 1e-9 || (angle + 90.).abs() < 1e-9 { -90. } else { angle };
    let truncate = |s: &str| text::truncate(s, label_size, false, label_limit);
    for t in ticks.iter_mut() {
        t.2 = truncate(&t.2);
    }
    // Overlap removal (parity) for continuous axes, greedy for log.
    let mut visible = vec![true; ticks.len()];
    let overlap_mode = axis_get(p, cfg, "labelOverlap", x).cloned();
    let strategy = match overlap_mode {
        Some(Value::Bool(false)) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Bool(true)) => Some("parity".to_string()),
        None => {
            if p.discrete() && p.units.is_none() {
                None
            } else if p.kind == Kind::Log {
                Some("greedy".into())
            } else {
                Some("parity".into())
            }
        }
        _ => None,
    };
    let flush = flag("labelFlush").unwrap_or(x && !p.discrete());
    let n_ticks = ticks.len();
    let extent_of = |i: usize| -> (f64, f64) {
        let (_, pos, l) = &ticks[i];
        let w = if x && angle == 0. { text::width(l, label_size, false) } else { label_size };
        if flush && x && angle == 0. && pos.abs() < 1. {
            (*pos, pos + w)
        } else if flush && x && angle == 0. && (pos - len).abs() < 1. && i + 1 == n_ticks {
            (pos - w, *pos)
        } else {
            (pos - w / 2., pos + w / 2.)
        }
    };
    if let Some(s) = strategy {
        for (i, t) in ticks.iter().enumerate() {
            if t.2.is_empty() {
                visible[i] = false;
            }
        }
        let overlaps = |vis: &[bool]| -> bool {
            let idx: Vec<usize> = (0..vis.len()).filter(|&i| vis[i]).collect();
            idx.windows(2).any(|w| {
                let (a0, a1) = extent_of(w[0]);
                let (b0, b1) = extent_of(w[1]);
                a0.max(b0) < a1.min(b1) + 1.
            })
        };
        if s == "greedy" {
            let mut last: Option<usize> = None;
            #[allow(clippy::needless_range_loop)]
            for i in 0..ticks.len() {
                if !visible[i] {
                    continue;
                }
                if let Some(l) = last {
                    let (a0, a1) = extent_of(l);
                    let (b0, b1) = extent_of(i);
                    if a0.max(b0) < a1.min(b1) + 1. {
                        visible[i] = false;
                        continue;
                    }
                }
                last = Some(i);
            }
        } else {
            let mut guard = 0;
            while overlaps(&visible) && guard < 10 {
                let idx: Vec<usize> = (0..visible.len()).filter(|&i| visible[i]).collect();
                for (k, i) in idx.into_iter().enumerate() {
                    if k % 2 == 1 {
                        visible[i] = false;
                    }
                }
                guard += 1;
            }
        }
    }
    let ticks_px: Vec<f64> = ticks.iter().map(|t| t.1).collect();
    let labels_on = flag("labels").unwrap_or(true);
    let ticks_on = flag("ticks").unwrap_or(true);
    let domain = flag("domain").unwrap_or(true);
    let grid = flag("grid").unwrap_or(!p.discrete() && cfg.grid_default);
    let title = match axis_get(p, cfg, "title", x) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => String::new(),
        _ => p.title.clone(),
    };
    let title = if matches!(p.axis.as_ref().and_then(|a| a.get("title")), Some(Value::Null)) { String::new() } else { title };
    let tick_size = if ticks_on { num("tickSize").unwrap_or(5.) } else { 0. };
    let pad = num("labelPadding").unwrap_or(2.);
    let max_label = ticks
        .iter()
        .zip(&visible)
        .filter(|(_, v)| **v)
        .map(|(t, _)| text::width(&t.2, label_size, false))
        .fold(0., f64::max);
    let label_extent = if !labels_on || ticks.is_empty() {
        0.
    } else if x {
        if angle == 0. { label_size } else { max_label * angle.to_radians().sin().abs() + label_size * angle.to_radians().cos().abs() }
    } else {
        max_label
    };
    let title_pad = num("titlePadding").unwrap_or(4.);
    let extent = tick_size + pad + label_extent + if title.is_empty() { 0. } else { title_pad + title_size };
    AxisLayout {
        ticks,
        visible,
        angle,
        extent: if show { extent } else { 0. },
        title,
        grid,
        show,
        label_size,
        title_size,
        flush,
        grid_ticks: if p.bins.is_some() {
            p.ticks((len / 40.).ceil().max(1.), theme).into_iter().map(|t| t.1).collect()
        } else {
            ticks_px.clone()
        },
        domain,
        ticks_on,
        labels_on,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw_grid(svg: &mut Svg, a: &AxisLayout, x: bool, w: f64, h: f64, ox: f64, oy: f64, theme: &Theme) {
    if !a.grid || !a.show {
        return;
    }
    let mut d = PathData::new();
    for pos in &a.grid_ticks {
        let p = pos.round() + 0.5;
        if x {
            d.move_to(ox + p, oy);
            d.line_to(ox + p, oy + h);
        } else {
            d.move_to(ox, oy + p);
            d.line_to(ox + w, oy + p);
        }
    }
    svg.path(&d.d, &Style::stroke(theme.grid, 1.));
}

/// Draws an axis. `x`: bottom axis at y = oy + h; else left axis at x = ox.
#[allow(clippy::too_many_arguments)]
pub fn draw_axis(svg: &mut Svg, a: &AxisLayout, x: bool, w: f64, h: f64, ox: f64, oy: f64, theme: &Theme, draw_title: bool) {
    if !a.show {
        return;
    }
    let tick = if a.ticks_on { 5. } else { 0. };
    let line = Style::stroke(theme.axis, 1.);
    if x {
        let y0 = oy + h + 0.5;
        if a.domain {
            svg.line(ox, y0, ox + w + 1., y0, &line);
        }
        if a.ticks_on {
            let mut d = PathData::new();
            for (_, pos, _) in &a.ticks {
                let p = ox + pos.round() + 0.5;
                d.move_to(p, y0);
                d.line_to(p, y0 + tick);
            }
            svg.path(&d.d, &line);
        }
        if a.labels_on {
            let st = TextStyle::new(a.label_size, theme.text);
            let n = a.ticks.len();
            for (i, (_, pos, l)) in a.ticks.iter().enumerate() {
                if !a.visible[i] {
                    continue;
                }
                let px = ox + pos;
                if a.angle == 0. {
                    let anchor = if a.flush && (*pos - 0.).abs() < 1. {
                        Anchor::Start
                    } else if a.flush && (*pos - w).abs() < 1. && i + 1 == n {
                        Anchor::End
                    } else {
                        Anchor::Middle
                    };
                    svg.text(px, y0 + tick + 2. + a.label_size * 0.79, l, &st.clone().anchor(anchor));
                } else {
                    // -90: right-aligned, middle baseline, reading upwards.
                    let yy = y0 + tick + 2.;
                    svg.text(px + a.label_size * 0.3, yy, l, &st.clone().anchor(Anchor::End).rotate(a.angle));
                }
            }
        }
        if draw_title && !a.title.is_empty() {
            let st = TextStyle::new(a.title_size, theme.text).anchor(Anchor::Middle).bold(true);
            let top = y0 + a.extent - a.title_size;
            svg.text(ox + w / 2., top + a.title_size * 0.79, &text::truncate(&a.title, a.title_size, true, w.max(180.)), &st);
        }
    } else {
        let x0 = ox + 0.5;
        if a.domain {
            svg.line(x0, oy, x0, oy + h + 1., &line);
        }
        if a.ticks_on {
            let mut d = PathData::new();
            for (_, pos, _) in &a.ticks {
                let p = oy + pos.round() + 0.5;
                d.move_to(x0, p);
                d.line_to(x0 - tick, p);
            }
            svg.path(&d.d, &line);
        }
        if a.labels_on {
            let st = TextStyle::new(a.label_size, theme.text).anchor(Anchor::End);
            for (i, (_, pos, l)) in a.ticks.iter().enumerate() {
                if !a.visible[i] {
                    continue;
                }
                svg.text(x0 - tick - 2., oy + pos + a.label_size * 0.3, l, &st);
            }
        }
        if draw_title && !a.title.is_empty() {
            let st = TextStyle::new(a.title_size, theme.text).anchor(Anchor::Middle).bold(true).rotate(-90.);
            let xx = x0 - a.extent + a.title_size * 0.8;
            svg.text(xx, oy + h / 2., &text::truncate(&a.title, a.title_size, true, h.max(180.)), &st);
        }
    }
}

// ---------------------------------------------------------------- marks

fn chan_value<'a>(l: &'a Layer, ch: &str, r: usize) -> Option<&'a V> {
    let c = l.ch(ch)?;
    let f = c.field.as_ref()?;
    let col = l.table.col(f)?;
    Some(l.table.get(r, col))
}

/// Constant (value) or scaled color for a row.
fn row_color(l: &Layer, sc: &Scales, r: usize, cfg: &Config, default: Rgba) -> Rgba {
    for ch in ["fill", "color", "stroke"] {
        if let Some(c) = l.ch(ch) {
            if let Some(v) = chan_value(l, ch, r)
                && let Some(cs) = &sc.color
                && let Some(col) = cs.color(v)
            {
                return col;
            }
            if let Some(Value::String(s)) = &c.value
                && let Some(col) = Rgba::parse(s)
            {
                return col;
            }
        }
    }
    let _ = cfg;
    default
}

fn mark_color(l: &Layer, cfg: &Config) -> Rgba {
    l.mark.color("color", cfg).or_else(|| l.mark.color("fill", cfg)).or_else(|| l.mark.color("stroke", cfg)).unwrap_or(cfg.mark_color)
}

fn row_opacity(l: &Layer, sc: &Scales, r: usize, default: f64) -> f64 {
    if let Some(v) = chan_value(l, "opacity", r)
        && let Some(s) = &sc.opacity
        && let Some(o) = s.map(v)
    {
        return o.clamp(0., 1.);
    }
    if let Some(c) = l.ch("opacity")
        && let Some(v) = c.value.as_ref().and_then(|v| v.as_f64())
    {
        return v.clamp(0., 1.);
    }
    default
}

fn row_size(l: &Layer, sc: &Scales, r: usize, default: f64) -> f64 {
    if let Some(v) = chan_value(l, "size", r)
        && let Some(s) = &sc.size
        && let Some(o) = s.map(v)
    {
        return o.max(0.);
    }
    if let Some(c) = l.ch("size")
        && let Some(v) = c.value.as_ref().and_then(|v| v.as_f64())
    {
        return v.max(0.);
    }
    default
}

fn row_shape(l: &Layer, sc: &Scales, r: usize, default: &str) -> String {
    if let Some(v) = chan_value(l, "shape", r)
        && let Some(d) = &sc.shape
    {
        let k = v.key();
        if let Some(i) = d.iter().position(|x| x.key() == k) {
            return SHAPES[i % SHAPES.len()].to_string();
        }
    }
    if let Some(c) = l.ch("shape")
        && let Some(v) = c.value.as_ref().and_then(|v| v.as_str())
    {
        return v.to_string();
    }
    default.to_string()
}

/// Vega symbol path (area `size`).
pub fn symbol_path(d: &mut PathData, shape: &str, x: f64, y: f64, size: f64) {
    let r = size.max(0.).sqrt() / 2.;
    let tri = |d: &mut PathData, pts: &[(f64, f64)]| {
        for (i, p) in pts.iter().enumerate() {
            if i == 0 {
                d.move_to(x + p.0, y + p.1);
            } else {
                d.line_to(x + p.0, y + p.1);
            }
        }
        d.close();
    };
    match shape {
        "square" => d.rect(x - r, y - r, 2. * r, 2. * r),
        "cross" => {
            let s = r / 2.5 * 1.;
            let (a, b) = (s, r);
            tri(d, &[(-b, -a), (-a, -a), (-a, -b), (a, -b), (a, -a), (b, -a), (b, a), (a, a), (a, b), (-a, b), (-a, a), (-b, a)]);
        }
        "diamond" => tri(d, &[(0., -r), (r, 0.), (0., r), (-r, 0.)]),
        "triangle-up" | "triangle" => {
            let h = r * 3f64.sqrt() / 2. * 2. / 3f64.sqrt();
            tri(d, &[(0., -h), (r, h), (-r, h)]);
        }
        "triangle-down" => tri(d, &[(0., r), (r, -r), (-r, -r)]),
        "triangle-right" => tri(d, &[(r, 0.), (-r, -r), (-r, r)]),
        "triangle-left" => tri(d, &[(-r, 0.), (r, -r), (r, r)]),
        "stroke" => {
            d.move_to(x - r, y);
            d.line_to(x + r, y);
        }
        "wedge" | "arrow" => tri(d, &[(0., -r), (r * 0.4, r), (-r * 0.4, r)]),
        _ => {
            d.move_to(x + r, y);
            d.arc_to(r, true, true, x - r, y);
            d.arc_to(r, true, true, x + r, y);
            d.close();
        }
    }
}

/// Draws all layers of the view with the plot origin at (ox, oy).
pub fn draw_marks(svg: &mut Svg, v: &View, ox: f64, oy: f64, cfg: &Config, notes: &mut Vec<String>) {
    for l in &v.layers {
        match l.mark.ty.as_str() {
            "point" | "circle" | "square" => draw_points(svg, v, l, ox, oy, cfg),
            "bar" | "rect" => draw_bars(svg, v, l, ox, oy, cfg),
            "line" => draw_lines(svg, v, l, ox, oy, cfg, false),
            "area" => draw_lines(svg, v, l, ox, oy, cfg, true),
            "rule" => draw_rules(svg, v, l, ox, oy, cfg),
            "tick" => draw_ticks(svg, v, l, ox, oy, cfg),
            "text" => draw_text(svg, v, l, ox, oy, cfg),
            "arc" => draw_arcs(svg, v, l, ox, oy, cfg),
            _ => {}
        }
        // line / area overlays: `point: true`, area `line: true`
        if matches!(l.mark.ty.as_str(), "line" | "area")
            && let Some(p) = l.mark.props.get("point").filter(|p| !matches!(p, Value::Bool(false) | Value::Null))
        {
            if p.as_str() == Some("transparent") {
                continue;
            }
            let mut props = p.as_object().cloned().unwrap_or_default();
            props.entry("filled").or_insert(Value::Bool(true));
            props.entry("opacity").or_insert(Value::from(1.));
            if !props.contains_key("color") && l.ch("color").is_none_or(|c| !c.has_field()) {
                props.insert("color".into(), Value::String(mark_color(l, cfg).hex()));
            }
            let over = Layer {
                mark: MarkDef { ty: "point".into(), props },
                enc: l.enc.clone(),
                table: l.table.clone(),
                stack: l.stack.clone(),
                orient_h: l.orient_h,
            };
            draw_points(svg, v, &over, ox, oy, cfg);
        }
    }
    let _ = notes;
}

/// Pixel x/y for a row (band centers for discrete scales, stack ends).
fn xy(v: &View, l: &Layer, r: usize, ch: &str) -> Option<f64> {
    let p = if ch == "x" { v.sc.x.as_ref() } else { v.sc.y.as_ref() };
    let c = l.ch(ch);
    let Some(p) = p else {
        // No scale: center of the view (or a value in px).
        if let Some(px) = c.and_then(|c| c.value.as_ref()).and_then(|x| x.as_f64()) {
            return Some(px);
        }
        return Some(if ch == "x" { v.width / 2. } else { v.height / 2. });
    };
    if let Some((m, _, e)) = &l.stack
        && m == ch
    {
        let col = l.table.col(e)?;
        return p.map_num(l.table.get(r, col).num()?);
    }
    let c = c?;
    let val = match &c.field {
        Some(f) => l.table.get(r, l.table.col(f)?).clone(),
        None => match (&c.value, c.def.get("datum")) {
            (_, Some(d)) => {
                let dv = V::from_json(d);
                if p.ty == Ty::T { V::Num(dv.date().or_else(|| data::datetime_obj(d))?) } else { dv }
            }
            (Some(px), None) => return px.as_f64(),
            _ => return None,
        },
    };
    if p.discrete() {
        let mut pos = p.map(&val)?;
        let off_ch = if ch == "x" { "xOffset" } else { "yOffset" };
        let off = if ch == "x" { v.sc.x_off.as_ref() } else { v.sc.y_off.as_ref() };
        if let (Some(o), Some(ov)) = (off, chan_value(l, off_ch, r)) {
            let mut o = o.clone();
            o.range = (0., p.bandwidth());
            pos += o.map(ov)? + o.bandwidth() / 2.;
            return Some(pos);
        }
        return Some(pos + p.bandwidth() / 2.);
    }
    if c.bins.is_some()
        && let Some(f2) = &c.field2
    {
        // binned positions for points/text: bin centers
        let a = val.num()?;
        let b = l.table.get(r, l.table.col(f2)?).num()?;
        return p.map_num((a + b) / 2.);
    }
    let n = if p.ty == Ty::T { val.date() } else { val.num() }?;
    p.map_num(n)
}

fn draw_points(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config) {
    let shape_default = match l.mark.ty.as_str() {
        "square" => "square",
        _ => l.mark.get("shape", cfg).and_then(|s| s.as_str()).unwrap_or("circle"),
    };
    let filled = l.mark.flag("filled", cfg).unwrap_or(l.mark.ty != "point");
    let size_default = l.mark.num("size", cfg).unwrap_or(30.);
    let opacity_default = l.mark.num("opacity", cfg).unwrap_or(0.7);
    let stroke_w = l.mark.num("strokeWidth", cfg).unwrap_or(if l.mark.ty == "point" { 2. } else { 0. });
    let base = mark_color(l, cfg);
    let n = l.table.rows.len();
    let compact = n > 4000;
    let mut groups: HashMap<(String, u64, u64), Vec<(f64, f64)>> = HashMap::new();
    let mut order = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in 0..n {
        let (Some(x), Some(y)) = (xy(v, l, r, "x"), xy(v, l, r, "y")) else { continue };
        let col = row_color(l, &v.sc, r, cfg, base);
        let size = row_size(l, &v.sc, r, size_default);
        let op = row_opacity(l, &v.sc, r, opacity_default);
        let shape = row_shape(l, &v.sc, r, shape_default);
        let (x, y) = (ox + x, oy + y);
        if compact {
            if op >= 0.99 && !seen.insert(((x * 2.).round() as i64, (y * 2.).round() as i64, col.hex())) {
                continue;
            }
            let k = (format!("{}|{}", col.hex(), shape), (size * 4.).round().to_bits(), (op * 100.).round().to_bits());
            groups
                .entry(k.clone())
                .or_insert_with(|| {
                    order.push(k.clone());
                    Vec::new()
                })
                .push((x, y));
            continue;
        }
        let mut d = PathData::new();
        symbol_path(&mut d, &shape, x, y, size);
        let mut st = if filled { Style::fill(col) } else { Style::stroke(col, stroke_w.max(0.5)) };
        st.opacity = op;
        svg.path(&d.d, &st);
    }
    for k in order {
        let pts = &groups[&k];
        let mut parts = k.0.split('|');
        let col = Rgba::parse(parts.next().unwrap_or("#000")).unwrap_or(base);
        let shape = parts.next().unwrap_or("circle");
        let size = f64::from_bits(k.1) / 4.;
        let op = f64::from_bits(k.2) / 100.;
        // Interleaved paths keep translucent overlaps accumulating.
        let chunks = if op < 0.99 { 8.min(pts.len().max(1)) } else { 1 };
        for c in 0..chunks {
            let mut d = PathData::with_decimals(1);
            let dots = filled && shape == "circle";
            for &(x, y) in pts.iter().skip(c).step_by(chunks) {
                if dots {
                    d.move_to(x, y);
                    d.d.push_str("h0");
                } else {
                    symbol_path(&mut d, shape, x, y, size);
                }
            }
            let mut st = if dots {
                let mut s = Style::stroke(col, size.max(0.).sqrt());
                s.round_joins = true;
                s
            } else if filled {
                Style::fill(col)
            } else {
                Style::stroke(col, stroke_w.max(0.5))
            };
            st.opacity = op;
            svg.path(&d.d, &st);
        }
    }
}

fn draw_bars(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config) {
    let base = mark_color(l, cfg);
    let opacity_default = l.mark.num("opacity", cfg).unwrap_or(1.);
    let corner = l.mark.num("cornerRadius", cfg).unwrap_or(0.);
    let rect = l.mark.ty == "rect";
    let bin_spacing = l.mark.num("binSpacing", cfg).unwrap_or(if rect { 0. } else { 1. });
    let continuous_band = l.mark.num("continuousBandSize", cfg).unwrap_or(5.);
    let explicit_w = l.mark.num("width", cfg).or_else(|| l.mark.get("width", cfg).and_then(|w| w.get("band")).and_then(|b| b.as_f64()).map(|b| -b));
    let n = l.table.rows.len();
    let stroke = l.mark.color("stroke", cfg);
    let stroke_w = l.mark.num("strokeWidth", cfg).unwrap_or(1.);
    let mut batches: Vec<((Rgba, u64), PathData)> = Vec::new();
    // Extent along one axis for row r: (a, b) px.
    let extent = |r: usize, ch: &str| -> Option<(f64, f64)> {
        let p = if ch == "x" { v.sc.x.as_ref() } else { v.sc.y.as_ref() };
        let full = if ch == "x" { (0., v.width) } else { (0., v.height) };
        let Some(p) = p else { return Some(full) };
        let c = l.ch(ch);
        let c2 = l.ch(&format!("{ch}2"));
        if let Some((m, s, e)) = &l.stack
            && m == ch
        {
            let a = l.table.get(r, l.table.col(s)?).num()?;
            let b = l.table.get(r, l.table.col(e)?).num()?;
            return Some((p.map_num(a)?, p.map_num(b)?));
        }
        let c = c?;
        if p.discrete() {
            let val = match &c.field {
                Some(f) => l.table.get(r, l.table.col(f)?).clone(),
                None => return Some(full),
            };
            let mut start = p.map(&val)?;
            let mut bw = p.bandwidth();
            let off_ch = if ch == "x" { "xOffset" } else { "yOffset" };
            let off = if ch == "x" { v.sc.x_off.as_ref() } else { v.sc.y_off.as_ref() };
            if let (Some(o), Some(ov)) = (off, chan_value(l, off_ch, r)) {
                let mut o = o.clone();
                o.range = (0., bw);
                start += o.map(ov)?;
                bw = o.bandwidth();
            }
            if let Some(w) = explicit_w {
                let w = if w < 0. { -w * bw } else { w };
                start += (bw - w) / 2.;
                bw = w;
            }
            return Some((start, start + bw.max(0.25)));
        }
        // continuous
        let get = |_c: &Chan, f: &Option<String>| -> Option<f64> {
            let f = f.as_ref()?;
            let val = l.table.get(r, l.table.col(f)?);
            let n = if p.ty == Ty::T { val.date() } else { val.num() }?;
            p.map_num(n)
        };
        if c.bins.is_some() || c.binned || c.units.is_some() {
            let a = get(c, &c.field)?;
            let b = match &c.field2 {
                Some(_) => get(c, &c.field2)?,
                None => get(c2?, &c2?.field)?,
            };
            let (a, b) = (a.min(b), a.max(b));
            return Some((a + bin_spacing.min((b - a) / 2.), b));
        }
        let a = get(c, &c.field)?;
        if let Some(c2) = c2 {
            let b = match &c2.field {
                Some(_) => get(c2, &c2.field)?,
                None => c2.value.as_ref().and_then(|x| x.as_f64())?,
            };
            return Some((a, b));
        }
        // The measure of a bar: from the baseline (0 or the domain edge).
        let is_measure = (ch == "x") == l.orient_h;
        if is_measure && !rect {
            let zero = if p.kind == Kind::Log { p.dom.0.min(p.dom.1) } else { crate::util::clamp(0., p.dom.0.min(p.dom.1), p.dom.0.max(p.dom.1)) };
            return Some((p.map_num(zero)?, a));
        }
        let w = explicit_w.map(|w| w.abs()).unwrap_or(continuous_band);
        Some((a - w / 2., a + w / 2.))
    };
    for r in 0..n {
        let (Some((x0, x1)), Some((y0, y1))) = (extent(r, "x"), extent(r, "y")) else { continue };
        let fill = row_color(l, &v.sc, r, cfg, base);
        let op = row_opacity(l, &v.sc, r, opacity_default);
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));
        let k = (fill, (op * 1000.).round().to_bits());
        let d = match batches.iter_mut().find(|(kk, _)| *kk == k) {
            Some((_, d)) => d,
            None => {
                batches.push((k, PathData::new()));
                &mut batches.last_mut().expect("pushed").1
            }
        };
        if corner > 0. {
            let rr = corner.min((x1 - x0) / 2.).min((y1 - y0) / 2.);
            d.move_to(ox + x0 + rr, oy + y0);
            d.line_to(ox + x1 - rr, oy + y0);
            d.arc_to(rr, false, true, ox + x1, oy + y0 + rr);
            d.line_to(ox + x1, oy + y1 - rr);
            d.arc_to(rr, false, true, ox + x1 - rr, oy + y1);
            d.line_to(ox + x0 + rr, oy + y1);
            d.arc_to(rr, false, true, ox + x0, oy + y1 - rr);
            d.line_to(ox + x0, oy + y0 + rr);
            d.arc_to(rr, false, true, ox + x0 + rr, oy + y0);
            d.close();
        } else {
            d.rect(ox + x0, oy + y0, x1 - x0, y1 - y0);
        }
    }
    // Adjacent heatmap cells meet without anti-aliasing seams.
    let crisp = rect && stroke.is_none() && corner == 0.;
    if crisp {
        svg.out.push_str("<g shape-rendering=\"crispEdges\">\n");
    }
    for ((fill, opb), d) in batches {
        let mut st = Style::fill(fill);
        st.opacity = f64::from_bits(opb) / 1000.;
        if let Some(s) = stroke {
            st.stroke = Some(s);
            st.stroke_width = stroke_w;
        }
        svg.path(&d.d, &st);
    }
    if crisp {
        svg.group_close();
    }
}

/// Series grouping for line/area: by every non-position field channel.
fn series_key(l: &Layer, r: usize) -> String {
    let mut k = String::new();
    let mut names: Vec<&String> = l.enc.keys().collect();
    names.sort();
    for name in names {
        let base = name.split('#').next().unwrap_or("");
        if matches!(base, "color" | "fill" | "stroke" | "detail" | "strokeDash" | "opacity" | "size" | "shape" | "strokeWidth")
            && let Some(c) = l.enc.get(name)
            && c.agg.is_none()
            && let Some(f) = &c.field
            && let Some(col) = l.table.col(f)
        {
            k.push_str(&l.table.get(r, col).key());
            k.push('\u{1f}');
        }
    }
    k
}

fn draw_lines(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config, area: bool) {
    let base = mark_color(l, cfg);
    let n = l.table.rows.len();
    let mut series: Vec<(String, Vec<usize>)> = Vec::new();
    let mut idx: HashMap<String, usize> = HashMap::new();
    for r in 0..n {
        let k = series_key(l, r);
        let i = *idx.entry(k.clone()).or_insert_with(|| {
            series.push((k, Vec::new()));
            series.len() - 1
        });
        series[i].1.push(r);
    }
    let horizontal = l.orient_h;
    let dim = if horizontal { "y" } else { "x" };
    let interp = l.mark.get("interpolate", cfg).and_then(|s| s.as_str()).unwrap_or("linear").to_string();
    let stroke_w = l.mark.num("strokeWidth", cfg).unwrap_or(2.);
    let opacity = l.mark.num("opacity", cfg).unwrap_or(1.);
    let fill_opacity = l.mark.num("fillOpacity", cfg).unwrap_or(1.);
    let area_line = area && l.mark.props.get("line").is_some_and(|x| !matches!(x, Value::Bool(false) | Value::Null));
    // Draw in color-domain order so stacks paint consistently.
    for (_, rows) in &series {
        let mut pts: Vec<(f64, f64, f64, usize)> = Vec::new();
        for &r in rows {
            let (Some(x), Some(y)) = (xy(v, l, r, "x"), xy(v, l, r, "y")) else { continue };
            // area baseline
            let base_px = if area {
                if let Some((m, s, _)) = &l.stack {
                    let p = if m == "x" { v.sc.x.as_ref() } else { v.sc.y.as_ref() };
                    p.and_then(|p| p.map_num(l.table.get(r, l.table.col(s)?).num()?)).unwrap_or(0.)
                } else if let Some(c2) = l.ch(if horizontal { "x2" } else { "y2" }).and_then(|c| c.field.clone()) {
                    let p = if horizontal { v.sc.x.as_ref() } else { v.sc.y.as_ref() };
                    p.and_then(|p| p.map_num(l.table.get(r, l.table.col(&c2)?).num()?)).unwrap_or(0.)
                } else {
                    let p = if horizontal { v.sc.x.as_ref() } else { v.sc.y.as_ref() };
                    p.and_then(|p| {
                        let z = if p.kind == Kind::Log { p.dom.0.min(p.dom.1) } else { crate::util::clamp(0., p.dom.0.min(p.dom.1), p.dom.0.max(p.dom.1)) };
                        p.map_num(z)
                    })
                    .unwrap_or(if horizontal { 0. } else { v.height })
                }
            } else {
                0.
            };
            pts.push((x, y, base_px, r));
        }
        if pts.is_empty() {
            continue;
        }
        // Sort along the dimension (VL sorts lines by x unless `order`).
        if l.ch("order").is_none() {
            pts.sort_by(|a, b| if horizontal { a.1.total_cmp(&b.1) } else { a.0.total_cmp(&b.0) });
        } else if let Some(f) = l.ch("order").and_then(|c| c.field.clone())
            && let Some(col) = l.table.col(&f)
        {
            pts.sort_by(|a, b| cmp_v(l.table.get(a.3, col), l.table.get(b.3, col)));
        }
        let _ = dim;
        let color = row_color(l, &v.sc, pts[0].3, cfg, base);
        let dash = l.ch("strokeDash").and_then(|_| chan_value(l, "strokeDash", pts[0].3)).and_then(|dv| {
            let d = v.sc.dash.as_ref()?;
            let i = d.iter().position(|x| x.key() == dv.key())?;
            let s = DASHES[i % DASHES.len()];
            (!s.is_empty()).then(|| s.to_string())
        });
        let line_pts: Vec<(f64, f64)> = pts.iter().map(|p| (ox + p.0, oy + p.1)).collect();
        let line_pts = if line_pts.len() > 4000 { decimate(&line_pts) } else { line_pts };
        if area {
            let mut d = PathData::new();
            path_interp(&mut d, &line_pts, &interp, false, false);
            let base_pts: Vec<(f64, f64)> = pts
                .iter()
                .rev()
                .map(|p| if horizontal { (ox + p.2, oy + p.1) } else { (ox + p.0, oy + p.2) })
                .collect();
            path_interp(&mut d, &base_pts, &interp, true, true);
            d.close();
            let mut st = Style::fill(color.scale_alpha(fill_opacity));
            st.opacity = opacity;
            svg.path(&d.d, &st);
            if area_line {
                let mut d = PathData::new();
                path_interp(&mut d, &line_pts, &interp, false, false);
                svg.path(&d.d, &Style::stroke(color, 1.));
            }
        } else {
            let mut d = PathData::new();
            path_interp(&mut d, &line_pts, &interp, false, false);
            let mut st = Style::stroke(color, stroke_w);
            st.dash = dash;
            st.opacity = opacity;
            st.round_joins = false;
            svg.path(&d.d, &st);
        }
    }
}

fn decimate(pts: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = vec![pts[0]];
    for &p in &pts[1..] {
        let l = out[out.len() - 1];
        if (p.0 - l.0).abs() >= 0.5 || (p.1 - l.1).abs() >= 0.5 {
            out.push(p);
        }
    }
    out
}

fn path_interp(d: &mut PathData, pts: &[(f64, f64)], interp: &str, cont: bool, _rev: bool) {
    if pts.is_empty() {
        return;
    }
    if cont {
        d.line_to(pts[0].0, pts[0].1);
    } else {
        d.move_to(pts[0].0, pts[0].1);
    }
    match interp {
        "step" | "step-after" | "step-before" => {
            for w in pts.windows(2) {
                let (a, b) = (w[0], w[1]);
                match interp {
                    "step-after" => {
                        d.line_to(b.0, a.1);
                    }
                    "step-before" => {
                        d.line_to(a.0, b.1);
                    }
                    _ => {
                        let m = (a.0 + b.0) / 2.;
                        d.line_to(m, a.1);
                        d.line_to(m, b.1);
                    }
                }
                d.line_to(b.0, b.1);
            }
        }
        "monotone" | "basis" | "cardinal" | "natural" | "catmull-rom" if pts.len() > 2 => {
            // Monotone cubic (d3 curveMonotoneX) for all smooth variants.
            let n = pts.len();
            let mut m = vec![0.; n];
            let slope = |i: usize| {
                let dx = pts[i + 1].0 - pts[i].0;
                if dx == 0. { 0. } else { (pts[i + 1].1 - pts[i].1) / dx }
            };
            #[allow(clippy::needless_range_loop)]
            for i in 0..n {
                m[i] = if i == 0 {
                    slope(0)
                } else if i == n - 1 {
                    slope(n - 2)
                } else {
                    let (s0, s1) = (slope(i - 1), slope(i));
                    if s0 * s1 <= 0. { 0. } else { 2. * s0 * s1 / (s0 + s1) }
                };
            }
            for i in 0..n - 1 {
                let (a, b) = (pts[i], pts[i + 1]);
                let h = (b.0 - a.0) / 3.;
                d.cubic_to((a.0 + h, a.1 + m[i] * h), (b.0 - h, b.1 - m[i + 1] * h), b);
            }
        }
        _ => {
            for p in &pts[1..] {
                d.line_to(p.0, p.1);
            }
        }
    }
}

fn draw_rules(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config) {
    let base = l.mark.color("color", cfg).or_else(|| l.mark.color("stroke", cfg)).unwrap_or(cfg.rule_color);
    let w = l.mark.num("strokeWidth", cfg).unwrap_or(1.);
    let opacity = l.mark.num("opacity", cfg).unwrap_or(1.);
    let n = l.table.rows.len().max(if l.enc.values().all(|c| c.field.is_none()) { 1 } else { 0 });
    let dash = l.mark.get("strokeDash", cfg).and_then(|d| d.as_array()).map(|a| a.iter().filter_map(|x| x.as_f64()).map(crate::svg::fmt_num).collect::<Vec<_>>().join(","));
    for r in 0..n {
        let has = |c: &str| l.ch(c).is_some();
        let col = row_color(l, &v.sc, r, cfg, base);
        let (x0, x1, y0, y1) = if has("x") && !has("y") {
            let x = xy(v, l, r, "x");
            let Some(x) = x else { continue };
            let x2 = if has("x2") { xy_ch2(v, l, r, "x2").unwrap_or(x) } else { x };
            if has("x2") {
                (x, x2, v.height / 2., v.height / 2.)
            } else {
                (x, x, 0., v.height)
            }
        } else if has("y") && !has("x") {
            let Some(y) = xy(v, l, r, "y") else { continue };
            if has("y2") {
                let y2 = xy_ch2(v, l, r, "y2").unwrap_or(y);
                (v.width / 2., v.width / 2., y, y2)
            } else {
                (0., v.width, y, y)
            }
        } else {
            let (Some(x), Some(y)) = (xy(v, l, r, "x"), xy(v, l, r, "y")) else { continue };
            let x2 = if has("x2") { xy_ch2(v, l, r, "x2").unwrap_or(x) } else { x };
            let y2 = if has("y2") { xy_ch2(v, l, r, "y2").unwrap_or(y) } else { y };
            if !has("x2") && !has("y2") {
                // band rule spanning the band
                continue;
            }
            (x, x2, y, y2)
        };
        let mut st = Style::stroke(col, w);
        st.opacity = opacity;
        st.dash = dash.clone();
        svg.line(ox + x0, oy + y0, ox + x1, oy + y1, &st);
    }
}

fn xy_ch2(v: &View, l: &Layer, r: usize, ch2: &str) -> Option<f64> {
    let main = &ch2[..1];
    let p = if main == "x" { v.sc.x.as_ref() } else { v.sc.y.as_ref() }?;
    let c = l.ch(ch2)?;
    let val = match &c.field {
        Some(f) => l.table.get(r, l.table.col(f)?).clone(),
        None => match c.def.get("datum") {
            Some(d) => V::from_json(d),
            None => return c.value.as_ref().and_then(|x| x.as_f64()),
        },
    };
    if p.discrete() {
        return p.center(&val);
    }
    let n = if p.ty == Ty::T { val.date() } else { val.num() }?;
    p.map_num(n)
}

fn draw_ticks(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config) {
    let base = mark_color(l, cfg);
    let thickness = l.mark.num("thickness", cfg).unwrap_or(1.);
    let opacity = l.mark.num("opacity", cfg).unwrap_or(0.7);
    for r in 0..l.table.rows.len() {
        let (Some(x), Some(y)) = (xy(v, l, r, "x"), xy(v, l, r, "y")) else { continue };
        let col = row_color(l, &v.sc, r, cfg, base);
        // Vertical ticks along a quantitative x; horizontal along y.
        let vertical = v.sc.x.as_ref().is_some_and(|p| !p.discrete());
        let band = if vertical {
            v.sc.y.as_ref().map(|p| if p.discrete() { p.bandwidth() * 0.75 } else { 14. }).unwrap_or(14.)
        } else {
            v.sc.x.as_ref().map(|p| if p.discrete() { p.bandwidth() * 0.75 } else { 14. }).unwrap_or(14.)
        };
        let size = l.mark.num("size", cfg).unwrap_or(band.max(4.));
        let mut st = Style::stroke(col, thickness);
        st.opacity = opacity;
        if vertical {
            svg.line(ox + x, oy + y - size / 2., ox + x, oy + y + size / 2., &st);
        } else {
            svg.line(ox + x - size / 2., oy + y, ox + x + size / 2., oy + y, &st);
        }
    }
}

/// How far text marks reach past the right / top of the plot (Vega's
/// autosize "pad" includes mark bounds).
pub fn text_overflow(v: &View, cfg: &Config) -> (f64, f64) {
    let (mut right, mut top) = (0f64, 0f64);
    for l in v.layers.iter().filter(|l| l.mark.ty == "text") {
        let size = l.mark.num("fontSize", cfg).unwrap_or(11.);
        let dx = l.mark.num("dx", cfg).unwrap_or(0.);
        let dy = l.mark.num("dy", cfg).unwrap_or(0.);
        let align = l.mark.get("align", cfg).and_then(|a| a.as_str()).unwrap_or("center").to_string();
        let tc = l.ch("text");
        let col = tc.and_then(|c| c.field.as_ref()).and_then(|f| l.table.col(f));
        for r in 0..l.table.rows.len().min(5000) {
            let (Some(x), Some(y)) = (xy(v, l, r, "x"), xy(v, l, r, "y")) else { continue };
            let w = col.map(|c| text::width(&l.table.get(r, c).label(), size, false)).unwrap_or(0.);
            let x1 = x + dx + match align.as_str() {
                "left" => w,
                "right" => 0.,
                _ => w / 2.,
            };
            right = right.max(x1 - v.width);
            top = top.max(-(y + dy - size));
        }
    }
    (right.max(0.), top.max(0.))
}

fn draw_text(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config) {
    let base = l.mark.color("color", cfg).or_else(|| l.mark.color("fill", cfg)).unwrap_or(cfg.text_color);
    let size_default = l.mark.num("fontSize", cfg).unwrap_or(11.);
    let dx = l.mark.num("dx", cfg).unwrap_or(0.);
    let dy = l.mark.num("dy", cfg).unwrap_or(0.);
    let anchor = match l.mark.get("align", cfg).and_then(|a| a.as_str()) {
        Some("left") => Anchor::Start,
        Some("right") => Anchor::End,
        _ => Anchor::Middle,
    };
    let baseline = l.mark.get("baseline", cfg).and_then(|a| a.as_str()).unwrap_or("middle").to_string();
    let bold = matches!(l.mark.get("fontWeight", cfg), Some(Value::String(s)) if s == "bold") || l.mark.num("fontWeight", cfg).is_some_and(|w| w >= 600.);
    let angle = l.mark.num("angle", cfg).unwrap_or(0.);
    let opacity = l.mark.num("opacity", cfg).unwrap_or(1.);
    let tc = l.ch("text");
    let fmt = tc.and_then(|c| c.def.get("format")).and_then(|f| f.as_str()).map(str::to_string);
    let fmt_time = tc.and_then(|c| c.def.get("formatType")).and_then(|f| f.as_str()) == Some("time") || tc.is_some_and(|c| c.ty == Ty::T);
    let n = l.table.rows.len().max(if l.table.rows.is_empty() && tc.is_some_and(|c| c.value.is_some()) { 1 } else { 0 });
    for r in 0..n {
        let txt = match tc {
            Some(c) => match &c.field {
                Some(f) => {
                    let Some(col) = l.table.col(f) else { continue };
                    let val = l.table.get(r, col);
                    match (&fmt, val) {
                        (Some(f), V::Num(x)) if fmt_time => time::strftime(f, *x),
                        (Some(f), V::Num(x)) => format::d3(f, *x),
                        (None, V::Num(x)) if fmt_time => ticks::vega_time_label(*x),
                        (None, V::Num(x)) => format::js_string(*x),
                        (_, other) => other.label(),
                    }
                }
                None => match &c.value {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => format::js_string(n.as_f64().unwrap_or(0.)),
                    _ => continue,
                },
            },
            None => match l.mark.props.get("text") {
                Some(Value::String(s)) => s.clone(),
                _ => continue,
            },
        };
        let (Some(x), Some(y)) = (xy(v, l, r, "x"), xy(v, l, r, "y")) else { continue };
        let size = row_size(l, &v.sc, r, size_default);
        let col = row_color(l, &v.sc, r, cfg, base);
        let shift = match baseline.as_str() {
            "top" => size * 0.79,
            "bottom" => -size * 0.21,
            "alphabetic" => 0.,
            _ => size * 0.3,
        };
        let mut st = TextStyle::new(size, col).anchor(anchor).bold(bold).rotate(angle);
        st.opacity = opacity;
        svg.text(ox + x + dx, oy + y + dy + shift, &txt, &st);
    }
}

fn draw_arcs(svg: &mut Svg, v: &View, l: &Layer, ox: f64, oy: f64, cfg: &Config) {
    let Some((_, (lo, hi))) = v.sc.theta.map(|t| ((), t)) else { return };
    let base = mark_color(l, cfg);
    let cx = ox + v.width / 2.;
    let cy = oy + v.height / 2.;
    let outer = l.mark.num("outerRadius", cfg).or_else(|| l.mark.num("radius", cfg)).unwrap_or(v.width.min(v.height) / 2.);
    let inner = l.mark.num("innerRadius", cfg).unwrap_or(0.);
    let pad = l.mark.num("padAngle", cfg).unwrap_or(0.);
    let span = hi - lo;
    if span <= 0. {
        return;
    }
    let Some((_, s, e)) = &l.stack else { return };
    let (Some(sc), Some(ec)) = (l.table.col(s), l.table.col(e)) else { return };
    for r in 0..l.table.rows.len() {
        let (Some(a), Some(b)) = (l.table.get(r, sc).num(), l.table.get(r, ec).num()) else { continue };
        let a0 = (a - lo) / span * std::f64::consts::TAU + pad / 2.;
        let a1 = (b - lo) / span * std::f64::consts::TAU - pad / 2.;
        if a1 <= a0 {
            continue;
        }
        let col = row_color(l, &v.sc, r, cfg, base);
        let at = |ang: f64, rr: f64| (cx + rr * ang.sin(), cy - rr * ang.cos());
        let mut d = PathData::new();
        let large = a1 - a0 > std::f64::consts::PI;
        if a1 - a0 >= std::f64::consts::TAU - 1e-9 {
            let (p, q) = (at(0., outer), at(std::f64::consts::PI, outer));
            d.move_to(p.0, p.1);
            d.arc_to(outer, true, true, q.0, q.1);
            d.arc_to(outer, true, true, p.0, p.1);
            d.close();
        } else {
            let (s0, s1) = (at(a0, outer), at(a1, outer));
            if inner > 0. {
                let (i0, i1) = (at(a0, inner), at(a1, inner));
                d.move_to(s0.0, s0.1);
                d.arc_to(outer, large, true, s1.0, s1.1);
                d.line_to(i1.0, i1.1);
                d.arc_to(inner, large, false, i0.0, i0.1);
            } else {
                d.move_to(cx, cy);
                d.line_to(s0.0, s0.1);
                d.arc_to(outer, large, true, s1.0, s1.1);
            }
            d.close();
        }
        let mut st = Style::fill(col);
        if let Some(sc) = l.mark.color("stroke", cfg) {
            st.stroke = Some(sc);
            st.stroke_width = l.mark.num("strokeWidth", cfg).unwrap_or(1.);
        }
        st.opacity = row_opacity(l, &v.sc, r, l.mark.num("opacity", cfg).unwrap_or(1.));
        svg.path(&d.d, &st);
    }
}

// ---------------------------------------------------------------- legends

/// Legend requests for this view (merged globally by the caller).
pub fn legend_requests(v: &View, cfg: &Config, out: &mut Guides) {
    let first_mark = v.layers.iter().find(|l| l.enc.keys().any(|k| matches!(k.as_str(), "color" | "fill" | "stroke" | "shape" | "size" | "opacity"))).map(|l| l.mark.ty.clone()).unwrap_or_default();
    let symbol_type = match first_mark.as_str() {
        "bar" | "rect" | "square" => "square",
        "line" | "rule" | "trail" => "stroke",
        _ => "circle",
    };
    let filled = v
        .layers
        .iter()
        .find(|l| l.mark.ty == first_mark)
        .map(|l| l.mark.flag("filled", cfg).unwrap_or(l.mark.ty != "point"))
        .unwrap_or(true);
    let opacity = v.layers.iter().find(|l| l.mark.ty == first_mark).and_then(|l| l.mark.num("opacity", cfg)).unwrap_or(match first_mark.as_str() {
        "point" | "circle" | "square" => 0.7,
        _ => 1.,
    });
    let chan_def = |ch: &str| -> Option<(String, &Chan)> {
        v.layers.iter().filter_map(|l| l.ch(ch)).find(|c| c.has_field()).map(|c| (default_title(c), c))
    };
    let legend_spec = |c: &Chan| c.def.get("legend").cloned();
    if let (Some((title, c)), Some(cs)) = (chan_def(v.sc.color_ch), &v.sc.color)
        && !matches!(legend_spec(c), Some(Value::Null))
    {
        let title = legend_title(c, title);
        match cs {
            ColorScale::Ordinal { domain, colors } => {
                let shape_same = chan_def("shape").is_some_and(|(_, sc)| sc.raw_field == c.raw_field);
                out.push(LegendReq::Symbols {
                    title,
                    entries: domain
                        .iter()
                        .enumerate()
                        .map(|(i, d)| {
                            let shape = if shape_same {
                                v.sc.shape.as_ref().and_then(|s| s.iter().position(|x| x.key() == d.key())).map(|k| SHAPES[k % SHAPES.len()]).unwrap_or("circle")
                            } else {
                                symbol_type
                            };
                            (label_for(c, d), colors[i % colors.len()], shape.to_string(), 100.)
                        })
                        .collect(),
                    filled: filled || symbol_type == "square",
                    opacity,
                });
            }
            ColorScale::Seq { lo, hi, scale, .. } => out.push(LegendReq::Gradient {
                title,
                lo: *lo,
                hi: *hi,
                scale: scale.clone(),
                temporal: c.ty == Ty::T,
                opacity: if first_mark == "rect" { 1. } else { opacity },
                length: v.height.clamp(64., 200.),
            }),
        }
    }
    if let (Some((title, c)), Some(sh)) = (chan_def("shape"), &v.sc.shape)
        && !matches!(legend_spec(c), Some(Value::Null))
        && !chan_def(v.sc.color_ch).is_some_and(|(_, cc)| cc.raw_field == c.raw_field)
    {
        out.push(LegendReq::Symbols {
            title: legend_title(c, title),
            entries: sh.iter().enumerate().map(|(i, d)| (label_for(c, d), Rgba::hex_rgb(0x888888), SHAPES[i % SHAPES.len()].to_string(), 100.)).collect(),
            filled: false,
            opacity: 1.,
        });
    }
    if let (Some((title, c)), Some(ss)) = (chan_def("size"), &v.sc.size)
        && !matches!(legend_spec(c), Some(Value::Null))
    {
        let entries: Vec<(String, Rgba, String, f64)> = match &ss.discrete {
            Some(d) => d.iter().map(|x| (label_for(c, x), Rgba::hex_rgb(0x888888), "circle".into(), ss.map(x).unwrap_or(30.))).collect(),
            None => {
                let vals = ticks::d3_ticks(ss.dom.0, ss.dom.1, 5.);
                let step = ticks::d3_tick_step(ss.dom.0, ss.dom.1, 5.);
                let p = ticks::d3_precision_fixed(step);
                // Vega drops a first entry only when its symbol would be empty.
                vals.iter()
                    .map(|x| (format::d3(&format!(",.{p}f"), *x), Rgba::BLACK, "circle".to_string(), ss.map(&V::Num(*x)).unwrap_or(30.)))
                    .enumerate()
                    .filter(|(i, e)| *i > 0 || e.3 > 0.)
                    .map(|(_, e)| e)
                    .collect()
            }
        };
        out.push(LegendReq::Symbols {
            title: legend_title(c, title),
            entries,
            filled: true,
            opacity: 0.7,
        });
    }
    if let (Some((title, c)), Some(_)) = (chan_def("strokeDash"), &v.sc.dash)
        && !matches!(legend_spec(c), Some(Value::Null))
    {
        let d = v.sc.dash.as_ref().cloned().unwrap_or_default();
        out.push(LegendReq::Symbols {
            title: legend_title(c, title),
            entries: d.iter().map(|x| (label_for(c, x), Rgba::hex_rgb(0x888888), "stroke".into(), 100.)).collect(),
            filled: false,
            opacity: 1.,
        });
    }
}

fn legend_title(c: &Chan, default: String) -> String {
    match c.def.get("legend").and_then(|l| l.get("title")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => String::new(),
        _ => default,
    }
}

fn label_for(c: &Chan, v: &V) -> String {
    match (v, &c.units, c.ty) {
        (V::Num(t), Some(u), _) => time::strftime(&data::time_unit_format(u), *t),
        (V::Num(t), None, Ty::T) => ticks::vega_time_label(*t),
        _ => {
            if let (Some(f), V::Num(x)) = (c.def.get("legend").and_then(|l| l.get("format")).and_then(|f| f.as_str()), v) {
                return format::d3(f, *x);
            }
            v.label()
        }
    }
}
