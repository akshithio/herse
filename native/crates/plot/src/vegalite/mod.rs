//! Vega-Lite spec -> SVG.
//!
//! Follows what `vl.compile` + Vega would produce on first render: Vega's
//! default config (fonts, #888 axes, #ddd grid, tableau10), Vega-Lite's
//! encoding rules (zero/nice scales, band steps, stacking, binning,
//! aggregation, time units) and Vega's "pad" autosize layout.

mod data;
mod expr;
pub mod html;
mod view;

use std::collections::HashMap;

use serde_json::Value;

use crate::color::{Colorscale, Rgba};
use crate::svg::{Anchor, PathData, Style, Svg, TextStyle};
use crate::text;
use crate::ticks;
use crate::{RenderOptions, Rendered};
use data::{Table, V};
use view::{Layer, Ty, View};

/// Resolved `config` with Vega / Vega-Lite defaults.
pub struct Config {
    raw: Value,
    pub continuous_w: f64,
    pub continuous_h: f64,
    pub step: f64,
    pub mark_color: Rgba,
    pub rule_color: Rgba,
    pub text_color: Rgba,
    pub category_scheme: String,
    pub ramp_scheme: String,
    pub heatmap_scheme: String,
    pub ordinal_scheme_name: String,
    pub bar_band_padding: f64,
    pub scale_zero: bool,
    pub grid_default: bool,
}

impl Config {
    fn new(raw: Option<&Value>, dark: bool) -> Config {
        let raw = raw.cloned().unwrap_or(Value::Null);
        let g = |p: &str| crate::util::path(&raw, p);
        let num = |p: &str, d: f64| g(p).and_then(|v| v.as_f64()).unwrap_or(d);
        let scheme = |p: &str, d: &str| -> String {
            match g(p) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Object(o)) => o.get("scheme").and_then(|s| s.as_str()).unwrap_or(d).to_string(),
                _ => d.to_string(),
            }
        };
        let mark_color = g("mark.color").and_then(|v| v.as_str()).and_then(Rgba::parse).unwrap_or(Rgba::hex_rgb(0x4c78a8));
        let text_default = if dark { Rgba::rgb(0xd7, 0xdb, 0xe2) } else { Rgba::BLACK };
        Config {
            continuous_w: num("view.continuousWidth", num("view.width", 200.)),
            continuous_h: num("view.continuousHeight", num("view.height", 200.)),
            step: num("view.step", num("view.discreteWidth.step", 20.)),
            mark_color,
            rule_color: g("rule.color").and_then(|v| v.as_str()).and_then(Rgba::parse).unwrap_or(text_default),
            text_color: g("text.color").and_then(|v| v.as_str()).and_then(Rgba::parse).unwrap_or(text_default),
            category_scheme: scheme("range.category", "tableau10"),
            ramp_scheme: scheme("range.ramp", "blues"),
            heatmap_scheme: scheme("range.heatmap", "yellowgreenblue"),
            ordinal_scheme_name: scheme("range.ordinal", "blues"),
            bar_band_padding: num("scale.barBandPaddingInner", num("scale.bandPaddingInner", 0.1)),
            scale_zero: g("scale.zero").and_then(|v| v.as_bool()).unwrap_or(true),
            grid_default: g("axis.grid").and_then(|v| v.as_bool()).unwrap_or(true),
            raw,
        }
    }

    pub fn mark_prop(&self, mark: &str, key: &str) -> Option<&Value> {
        let g = |p: &str| self.raw.get(p).and_then(|m| m.get(key)).filter(|v| !v.is_null());
        g(mark).or_else(|| g("mark"))
    }

    pub fn axis_prop(&self, key: &str, x: bool, discrete: bool, ty: Ty) -> Option<&Value> {
        let names: [&str; 4] = [
            if x { "axisX" } else { "axisY" },
            if discrete { "axisBand" } else if ty == Ty::T { "axisTemporal" } else { "axisQuantitative" },
            if discrete { if x { "axisXBand" } else { "axisYBand" } } else if x { "axisXQuantitative" } else { "axisYQuantitative" },
            "axis",
        ];
        names.iter().find_map(|n| self.raw.get(*n).and_then(|a| a.get(key)).filter(|v| !v.is_null()))
    }

    pub fn ordinal_scheme(&self) -> Colorscale {
        Colorscale::vega_named(&self.ordinal_scheme_name).unwrap_or_else(|| Colorscale::vega_named("blues").expect("builtin"))
    }

    fn get(&self, p: &str) -> Option<&Value> {
        crate::util::path(&self.raw, p)
    }
}

/// Chrome colors (text, axes, grid) for the current mode.
pub struct Theme {
    pub text: Rgba,
    pub axis: Rgba,
    pub grid: Rgba,
    pub cell: Option<Rgba>,
    pub bg: Option<Rgba>,
}

impl Theme {
    fn new(cfg: &Config, dark: bool, blend: bool) -> Theme {
        let c = |p: &str| cfg.get(p).and_then(|v| v.as_str()).and_then(Rgba::parse);
        let cell_stroke = match cfg.get("view.stroke") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Rgba::parse(s),
            _ => Some(Rgba::hex_rgb(0xdddddd)),
        };
        if dark {
            // Keep a config's own colors only if they're already light.
            let light = |x: Option<Rgba>, d: Rgba| x.filter(|c| c.luminance() > 0.35).unwrap_or(d);
            Theme {
                text: light(c("axis.labelColor"), Rgba::rgb(0xd7, 0xdb, 0xe2)),
                axis: Rgba::WHITE.with_alpha(0.35),
                grid: Rgba::WHITE.with_alpha(0.12),
                cell: cell_stroke.map(|_| Rgba::WHITE.with_alpha(0.15)),
                bg: None,
            }
        } else if blend {
            // On the page: the page's ink, faint grids, and no background
            // unless the spec asks for one.
            let ink = Rgba::rgb(0x1c, 0x1a, 0x17);
            Theme {
                text: c("axis.labelColor").unwrap_or(ink),
                axis: c("axis.domainColor").unwrap_or(ink.with_alpha(0.3)),
                grid: c("axis.gridColor").unwrap_or(ink.with_alpha(0.09)),
                cell: cell_stroke.map(|_| ink.with_alpha(0.12)),
                bg: c("background").filter(|b| b.luminance() < 0.97),
            }
        } else {
            Theme {
                text: c("axis.labelColor").unwrap_or(Rgba::BLACK),
                axis: c("axis.domainColor").unwrap_or(Rgba::hex_rgb(0x888888)),
                grid: c("axis.gridColor").unwrap_or(Rgba::hex_rgb(0xdddddd)),
                cell: cell_stroke,
                bg: Some(c("background").unwrap_or(Rgba::WHITE)),
            }
        }
    }
}

/// A legend to draw (merged across views by title + kind).
#[derive(Clone)]
pub enum LegendReq {
    Symbols {
        title: String,
        entries: Vec<(String, Rgba, String, f64)>,
        filled: bool,
        opacity: f64,
    },
    Gradient {
        title: String,
        lo: f64,
        hi: f64,
        scale: Colorscale,
        temporal: bool,
        opacity: f64,
        length: f64,
    },
}

impl LegendReq {
    fn title(&self) -> &str {
        match self {
            LegendReq::Symbols { title, .. } | LegendReq::Gradient { title, .. } => title,
        }
    }
}

#[derive(Default)]
pub struct Guides {
    pub legends: Vec<LegendReq>,
}

impl Guides {
    pub fn push(&mut self, l: LegendReq) {
        if self.legends.iter().any(|x| x.title() == l.title() && std::mem::discriminant(x) == std::mem::discriminant(&l)) {
            return;
        }
        self.legends.push(l);
    }
}

struct Ctx<'a> {
    cfg: Config,
    theme: Theme,
    datasets: Option<&'a Value>,
    params: HashMap<String, V>,
    notes: Vec<String>,
    avail_w: f64,
}

pub(crate) fn render(spec: &Value, opts: &RenderOptions) -> Option<Rendered> {
    let o = spec.as_object()?;
    if let Some(s) = o.get("$schema").and_then(|s| s.as_str())
        && s.contains("/schema/vega/")
    {
        return None;
    }
    let cfg = Config::new(o.get("config"), opts.dark);
    let theme = Theme::new(&cfg, opts.dark, opts.blend);
    let mut params = HashMap::new();
    collect_params(spec, &mut params);
    let interactive = has_selection(spec, 0);
    let mut ctx = Ctx {
        cfg,
        theme,
        datasets: o.get("datasets"),
        params,
        notes: Vec::new(),
        avail_w: (opts.width as f64 - 120.).max(100.),
    };
    if interactive {
        ctx.notes.push("interactive selection shown in its initial state".into());
    }
    let top_data = load_data(spec, None, &ctx)?;
    let mut node = build_node(spec, top_data, &mut ctx, None)?;
    // The top-level title is drawn below (above legends and all).
    if let Node::View(v, _) = &mut node {
        v.title = None;
    }
    #[cfg(test)]
    tests::record(&node);
    // Layout: node size, then legends on the right, title above.
    let mut guides = Guides::default();
    node.legends(&ctx.cfg, &mut guides);
    let (nw, nh, left, top) = node.size();
    let fs = 13.;
    let title = title_of(spec.get("title"));
    let subtitle = spec.get("title").and_then(|t| t.get("subtitle")).and_then(|s| s.as_str()).map(str::to_string);
    let title_h = if title.is_empty() { 0. } else { fs + 9. + if subtitle.is_some() { 15. } else { 0. } };
    let legend_sizes: Vec<(f64, f64)> = guides.legends.iter().map(legend_size).collect();
    let legend_w = legend_sizes.iter().map(|s| s.0).fold(0., f64::max);
    let legend_h: f64 = legend_sizes.iter().map(|s| s.1).sum::<f64>() + 16. * (legend_sizes.len().saturating_sub(1)) as f64;
    let pad = 5.;
    let width = (pad + left).round() + nw + if legend_w > 0. { 18. + legend_w } else { 0. } + pad;
    let height = pad + title_h + top + nh.max(legend_h) + pad;
    let mut svg = Svg::new(width, height, &opts.font_family, 10.);
    if let Some(bg) = ctx.theme.bg {
        svg.fill_rect(0., 0., width, height, bg);
    }
    // Integer origin: crisp axis lines and seamless band cells.
    let ox = (pad + left).round();
    let oy = (pad + title_h + top).round();
    node.draw(&mut svg, ox, oy, &mut ctx);
    // Legends.
    let mut ly = oy - top;
    let lx = ox + nw + 18.;
    for (l, s) in guides.legends.iter().zip(&legend_sizes) {
        draw_legend(&mut svg, l, lx, ly, &ctx);
        ly += s.1 + 16.;
    }
    if !title.is_empty() {
        let anchor = spec
            .get("title")
            .and_then(|t| t.get("anchor"))
            .and_then(|a| a.as_str())
            .unwrap_or(if node.is_composite() { "start" } else { "middle" });
        let (x, a) = match anchor {
            "start" => (ox, Anchor::Start),
            "end" => (ox + nw, Anchor::End),
            _ => (ox + nw / 2., Anchor::Middle),
        };
        let size = spec.get("title").and_then(|t| t.get("fontSize")).and_then(|v| v.as_f64()).unwrap_or(fs);
        let color = spec
            .get("title")
            .and_then(|t| t.get("color"))
            .and_then(|c| c.as_str())
            .and_then(Rgba::parse)
            .filter(|_| !opts.dark)
            .unwrap_or(ctx.theme.text);
        let y = pad + size * 0.79;
        svg.text(x, y, &title, &TextStyle::new(size, color).anchor(a).bold(true));
        if let Some(st) = subtitle {
            svg.text(x, y + 3. + 12., &st, &TextStyle::new(12., color).anchor(a));
        }
    }
    let w = width as f32;
    let h = height as f32;
    // Keep within the requested width by scaling down (Vega charts have a fixed size).
    let (svg_s, w, h) = if w > opts.width {
        let s = opts.width / w;
        let out = svg.finish();
        let scaled = out.replacen(
            &format!("width=\"{}\" height=\"{}\"", crate::svg::fmt_num(width), crate::svg::fmt_num(height)),
            &format!("width=\"{}\" height=\"{}\"", crate::svg::fmt_num((w * s) as f64), crate::svg::fmt_num((h * s) as f64)),
            1,
        );
        (scaled, w * s, h * s)
    } else {
        (svg.finish(), w, h)
    };
    Some(Rendered {
        svg: svg_s,
        width: w,
        height: h,
        notes: ctx.notes,
    })
}

fn title_of(t: Option<&Value>) -> String {
    match t {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
        Some(Value::Object(o)) => match o.get("text") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// Selection parameters (`select` / VL4 `selection`) anywhere in the spec.
fn has_selection(v: &Value, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    match v {
        Value::Object(o) => {
            if o.contains_key("selection") && o.get("selection").is_some_and(|s| s.is_object()) {
                return true;
            }
            if let Some(Value::Array(ps)) = o.get("params")
                && ps.iter().any(|p| p.get("select").is_some() && p.get("bind").and_then(|b| b.as_str()) != Some("scales"))
            {
                return true;
            }
            ["layer", "hconcat", "vconcat", "concat", "spec"].iter().any(|k| match o.get(*k) {
                Some(Value::Array(a)) => a.iter().any(|c| has_selection(c, depth + 1)),
                Some(c @ Value::Object(_)) => has_selection(c, depth + 1),
                _ => false,
            })
        }
        _ => false,
    }
}

fn collect_params(spec: &Value, out: &mut HashMap<String, V>) {
    for key in ["params"] {
        if let Some(ps) = spec.get(key).and_then(|p| p.as_array()) {
            for p in ps {
                if let (Some(name), Some(v)) = (p.get("name").and_then(|n| n.as_str()), p.get("value"))
                    && p.get("select").is_none()
                {
                    out.insert(name.to_string(), V::from_json(v));
                }
            }
        }
    }
    for key in ["layer", "hconcat", "vconcat", "concat"] {
        if let Some(arr) = spec.get(key).and_then(|a| a.as_array()) {
            for s in arr {
                collect_params(s, out);
            }
        }
    }
    if let Some(s) = spec.get("spec") {
        collect_params(s, out);
    }
}

/// The spec's own data (or the inherited one), with its transforms applied.
fn load_data(spec: &Value, inherited: Option<&Table>, ctx: &Ctx) -> Option<Option<Table>> {
    let mut t = match spec.get("data") {
        Some(Value::Null) | None => inherited.cloned(),
        Some(d) => Some(data::load(d, ctx.datasets)?),
    };
    if let Some(tr) = spec.get("transform").and_then(|t| t.as_array())
        && !tr.is_empty()
    {
        let tab = t.as_mut()?;
        data::apply(tab, tr, &data::Ctx { params: &ctx.params })?;
    }
    Some(t)
}

enum Node {
    View(Box<View>, Box<ViewGuides>),
    Facet(Box<Facet>),
    Concat { horizontal: bool, wrap: Option<usize>, children: Vec<Node>, spacing: f64 },
}

struct ViewGuides {
    xa: Option<view::AxisLayout>,
    ya: Option<view::AxisLayout>,
    /// Text marks reaching past the plot (right, top).
    overflow: (f64, f64),
}

struct Facet {
    cells: Vec<(V, Box<View>)>,
    /// Row values when faceting by row and column at once.
    rows: Option<Vec<V>>,
    col_title: String,
    row_title: String,
    columns: usize,
    by_row: bool,
    labels: Vec<String>,
    xa: Option<view::AxisLayout>,
    ya: Option<view::AxisLayout>,
    spacing: f64,
    legends: Vec<LegendReq>,
}

fn merge_encoding(parent: Option<&serde_json::Map<String, Value>>, child: Option<&serde_json::Map<String, Value>>) -> serde_json::Map<String, Value> {
    let mut m = parent.cloned().unwrap_or_default();
    if let Some(c) = child {
        for (k, v) in c {
            m.insert(k.clone(), v.clone());
        }
    }
    m
}

/// Flattens nested layers into unit layers.
fn collect_layers(spec: &Value, data: Option<&Table>, enc: Option<&serde_json::Map<String, Value>>, ctx: &mut Ctx, out: &mut Vec<Layer>) -> Option<()> {
    let table = load_data(spec, data, ctx)?;
    let encoding = merge_encoding(enc, spec.get("encoding").and_then(|e| e.as_object()));
    if let Some(layers) = spec.get("layer").and_then(|l| l.as_array()) {
        for l in layers {
            collect_layers(l, table.as_ref(), Some(&encoding), ctx, out)?;
        }
        return Some(());
    }
    let mark = spec.get("mark")?;
    let mark_ty = mark.as_str().or_else(|| mark.get("type").and_then(|t| t.as_str())).unwrap_or("");
    if matches!(mark_ty, "boxplot" | "errorbar" | "errorband") {
        return expand_composite(mark, &encoding, table?, ctx, out);
    }
    let l = view::build_layer(mark, &encoding, table?, &ctx.cfg, &mut ctx.notes)?;
    out.push(l);
    Some(())
}

/// boxplot -> rule (whiskers) + bar (box) + tick (median) + outlier points.
fn expand_composite(mark: &Value, enc: &serde_json::Map<String, Value>, table: Table, ctx: &mut Ctx, out: &mut Vec<Layer>) -> Option<()> {
    let ty = mark.as_str().or_else(|| mark.get("type").and_then(|t| t.as_str()))?;
    if ty != "boxplot" {
        return None;
    }
    let extent = mark.get("extent").and_then(|e| e.as_f64()).unwrap_or(1.5);
    // The continuous channel holds the values.
    let (vch, pch) = if enc.get("x").and_then(|d| d.get("type")).and_then(|t| t.as_str()) == Some("quantitative") && enc.get("y").and_then(|d| d.get("type")).and_then(|t| t.as_str()) != Some("quantitative") {
        ("x", "y")
    } else {
        ("y", "x")
    };
    let vdef = enc.get(vch)?;
    let vfield = vdef.get("field")?.as_str()?.to_string();
    let pfield = enc.get(pch).and_then(|d| d.get("field")).and_then(|f| f.as_str()).map(str::to_string);
    let color_field = enc.get("color").and_then(|d| d.get("field")).and_then(|f| f.as_str()).map(str::to_string);
    let mut groupby = Vec::new();
    groupby.extend(pfield.clone());
    if let Some(c) = &color_field
        && Some(c) != pfield.as_ref()
    {
        groupby.push(c.clone());
    }
    let measures: Vec<(String, Option<String>, String)> = ["q1", "median", "q3", "min", "max"].iter().map(|op| (op.to_string(), Some(vfield.clone()), format!("{op}_{vfield}"))).collect();
    let mut stats = data::group_aggregate(&table, &groupby, &measures)?;
    // whisker ends: data extremes within extent * IQR
    let lo_c = stats.add_col("lower_box");
    let hi_c = stats.add_col("upper_box");
    let q1c = stats.col(&format!("q1_{vfield}"))?;
    let q3c = stats.col(&format!("q3_{vfield}"))?;
    let vcol = table.col(&vfield)?;
    let gcols: Vec<usize> = groupby.iter().filter_map(|g| table.col(g)).collect();
    let sgcols: Vec<usize> = groupby.iter().filter_map(|g| stats.col(g)).collect();
    let mut outliers = Table::default();
    for n in &groupby {
        outliers.add_col(n);
    }
    let oc = outliers.add_col(&vfield);
    for r in 0..stats.rows.len() {
        let key: Vec<String> = sgcols.iter().map(|&c| stats.rows[r][c].key()).collect();
        let (q1, q3) = (stats.rows[r][q1c].num()?, stats.rows[r][q3c].num()?);
        let iqr = q3 - q1;
        let (lf, uf) = (q1 - extent * iqr, q3 + extent * iqr);
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for row in &table.rows {
            if gcols.iter().map(|&c| row[c].key()).collect::<Vec<_>>() != key {
                continue;
            }
            let Some(v) = row[vcol].num() else { continue };
            if v >= lf && v <= uf {
                lo = lo.min(v);
                hi = hi.max(v);
            } else {
                let mut orow: Vec<V> = gcols.iter().map(|&c| row[c].clone()).collect();
                orow.resize(oc, V::Null);
                orow.push(V::Num(v));
                outliers.rows.push(orow);
            }
        }
        stats.rows[r][lo_c] = if lo.is_finite() { V::Num(lo) } else { V::Num(q1) };
        stats.rows[r][hi_c] = if hi.is_finite() { V::Num(hi) } else { V::Num(q3) };
    }
    let title = vdef.get("title").and_then(|t| t.as_str()).unwrap_or(&vfield).to_string();
    let mut base: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Some(p) = enc.get(pch) {
        base.insert(pch.into(), p.clone());
    }
    if let Some(c) = enc.get("color") {
        base.insert("color".into(), c.clone());
    }
    let qdef = |f: &str| serde_json::json!({"field": f, "type": "quantitative", "title": title, "scale": vdef.get("scale").cloned().unwrap_or(Value::Null)});
    let add = |mark: Value, extra: &[(&str, Value)], t: Table, ctx: &mut Ctx, out: &mut Vec<Layer>| -> Option<()> {
        let mut e = base.clone();
        for (k, v) in extra {
            e.insert(k.to_string(), v.clone());
        }
        let l = view::build_layer(&mark, &e, t, &ctx.cfg, &mut ctx.notes)?;
        out.push(l);
        Some(())
    };
    let v2 = format!("{vch}2");
    let mcolor = mark.get("color").cloned();
    let rule_mark = serde_json::json!({"type": "rule", "color": "black"});
    add(rule_mark.clone(), &[(vch, qdef("lower_box")), (v2.as_str(), serde_json::json!({"field": format!("q1_{vfield}")}))], stats.clone(), ctx, out)?;
    add(rule_mark, &[(vch, qdef(&format!("q3_{vfield}"))), (v2.as_str(), serde_json::json!({"field": "upper_box"}))], stats.clone(), ctx, out)?;
    let mut bar = serde_json::json!({"type": "bar", "size": 14});
    if let Some(c) = &mcolor {
        bar["color"] = c.clone();
    }
    add(bar, &[(vch, qdef(&format!("q1_{vfield}"))), (v2.as_str(), serde_json::json!({"field": format!("q3_{vfield}")}))], stats.clone(), ctx, out)?;
    add(serde_json::json!({"type": "tick", "color": "white", "size": 14, "opacity": 1}), &[(vch, qdef(&format!("median_{vfield}")))], stats, ctx, out)?;
    if !outliers.rows.is_empty() {
        add(serde_json::json!({"type": "point", "opacity": 0.7}), &[(vch, qdef(&vfield))], outliers, ctx, out)?;
    }
    Some(())
}

fn build_node(spec: &Value, inherited: Option<Table>, ctx: &mut Ctx, size_hint: Option<(&Value, &Value)>) -> Option<Node> {
    let o = spec.as_object()?;
    if o.contains_key("repeat") {
        return None;
    }
    for (key, horizontal) in [("hconcat", true), ("vconcat", false), ("concat", true)] {
        if let Some(children) = o.get(key).and_then(|c| c.as_array()) {
            let table = load_data(spec, inherited.as_ref(), ctx)?;
            let mut out = Vec::new();
            for c in children {
                out.push(build_node(c, table.clone(), ctx, None)?);
            }
            let wrap = if key == "concat" { o.get("columns").and_then(|c| c.as_u64()).map(|c| c.max(1) as usize) } else { None };
            let spacing = o.get("spacing").and_then(|s| s.as_f64()).unwrap_or(20.);
            return Some(Node::Concat { horizontal, wrap, children: out, spacing });
        }
    }
    if let Some(facet) = o.get("facet") {
        let table = load_data(spec, inherited.as_ref(), ctx)?;
        let inner = o.get("spec")?;
        return build_facet(facet, inner, table?, o.get("columns").and_then(|c| c.as_u64()), ctx, None);
    }
    let enc = o.get("encoding").and_then(|e| e.as_object());
    if let Some(e) = enc
        && (e.contains_key("row") || e.contains_key("column") || e.contains_key("facet"))
    {
        // Encoding-level facet: split off the facet channels.
        let table = load_data(spec, inherited.as_ref(), ctx)?;
        let mut facet = serde_json::Map::new();
        for k in ["row", "column", "facet"] {
            if let Some(d) = e.get(k) {
                facet.insert(k.to_string(), d.clone());
            }
        }
        let mut inner = o.clone();
        inner.remove("data");
        inner.remove("transform");
        let mut e2 = e.clone();
        for k in ["row", "column", "facet"] {
            e2.remove(k);
        }
        inner.insert("encoding".into(), Value::Object(e2));
        let facet_v = if facet.contains_key("facet") { facet["facet"].clone() } else { Value::Object(facet) };
        let columns = e.get("facet").and_then(|f| f.get("columns")).and_then(|c| c.as_u64());
        return build_facet(&facet_v, &Value::Object(inner), table?, columns, ctx, None);
    }
    let mut layers = Vec::new();
    collect_layers(spec, inherited.as_ref(), None, ctx, &mut layers)?;
    if layers.is_empty() {
        return None;
    }
    let (w, h) = match size_hint {
        Some((w, h)) => (Some(w), Some(h)),
        None => (o.get("width"), o.get("height")),
    };
    let v = view::build_view(layers, w, h, &ctx.cfg, ctx.avail_w, o.get("title").cloned())?;
    let g = view_guides(&v, ctx);
    Some(Node::View(Box::new(v), Box::new(g)))
}

fn view_guides(v: &View, ctx: &Ctx) -> ViewGuides {
    let arc_only = v.layers.iter().all(|l| l.mark.ty == "arc");
    ViewGuides {
        xa: if arc_only { None } else { v.sc.x.as_ref().map(|p| view::layout_axis(p, true, v.width, &ctx.cfg, &ctx.theme)) },
        ya: if arc_only { None } else { v.sc.y.as_ref().map(|p| view::layout_axis(p, false, v.height, &ctx.cfg, &ctx.theme)) },
        overflow: view::text_overflow(v, &ctx.cfg),
    }
}

/// Copies `spec` with `bin.extent` filled in from `table` (after the spec's
/// own transforms) for every binned encoding channel.
fn with_bin_extents(spec: &Value, table: &Table, ctx: &Ctx) -> Option<Value> {
    let full = load_data(spec, Some(table), ctx)?.unwrap_or_default();
    let mut out = spec.clone();
    fn walk(v: &mut Value, t: &Table) {
        if let Some(enc) = v.get_mut("encoding").and_then(|e| e.as_object_mut()) {
            for (_, def) in enc.iter_mut() {
                let Some(field) = def.get("field").and_then(|f| f.as_str()).map(str::to_string) else { continue };
                let bin = match def.get("bin") {
                    Some(Value::Bool(true)) => serde_json::Map::new(),
                    Some(Value::Object(o)) if !o.contains_key("extent") => o.clone(),
                    _ => continue,
                };
                let Some(c) = t.col(&field) else { continue };
                let Some((lo, hi)) = crate::util::extent(t.rows.iter().filter_map(|r| r[c].num())) else { continue };
                let mut bin = bin;
                bin.insert("extent".into(), serde_json::json!([lo, hi]));
                def["bin"] = Value::Object(bin);
            }
        }
        if let Some(layers) = v.get_mut("layer").and_then(|l| l.as_array_mut()) {
            for l in layers {
                walk(l, t);
            }
        }
    }
    walk(&mut out, &full);
    Some(out)
}

fn build_facet(facet: &Value, inner: &Value, table: Table, columns: Option<u64>, ctx: &mut Ctx, _hint: Option<()>) -> Option<Node> {
    // facet: {field, type, ...} (wrapped) or {row: def, column: def}
    let (row_def, col_def) = if facet.get("field").is_some() {
        (None, Some(facet))
    } else {
        (facet.get("row"), facet.get("column"))
    };
    let wrapped = facet.get("field").is_some();
    let field_of = |d: Option<&Value>| d.and_then(|d| d.get("field")).and_then(|f| f.as_str()).map(str::to_string);
    let row_f = field_of(row_def);
    let col_f = field_of(col_def);
    if row_f.is_none() && col_f.is_none() {
        return None;
    }
    if row_def.is_some_and(|d| d.get("timeUnit").is_some() || d.get("bin").is_some()) || col_def.is_some_and(|d| d.get("timeUnit").is_some() || d.get("bin").is_some()) {
        return None;
    }
    let distinct = |f: &Option<String>, def: Option<&Value>| -> Option<Vec<V>> {
        let Some(f) = f else { return Some(vec![V::Null]) };
        let c = table.col(f)?;
        let mut seen = std::collections::HashSet::new();
        let mut vals: Vec<V> = table.rows.iter().map(|r| r[c].clone()).filter(|v| seen.insert(v.key())).collect();
        match def.and_then(|d| d.get("sort")) {
            Some(Value::String(s)) if s == "descending" => {
                vals.sort_by(data::cmp_v);
                vals.reverse();
            }
            Some(Value::Array(order)) => {
                let order: Vec<String> = order.iter().map(|v| V::from_json(v).key()).collect();
                vals.sort_by(|a, b| {
                    let ia = order.iter().position(|o| *o == a.key()).unwrap_or(usize::MAX);
                    let ib = order.iter().position(|o| *o == b.key()).unwrap_or(usize::MAX);
                    ia.cmp(&ib).then_with(|| data::cmp_v(a, b))
                });
            }
            Some(Value::Null) => {}
            _ => vals.sort_by(data::cmp_v),
        }
        Some(vals)
    };
    let rows = distinct(&row_f, row_def)?;
    let cols = distinct(&col_f, col_def)?;
    if rows.len() * cols.len() > 400 {
        return None;
    }
    let ow = inner.get("width");
    let oh = inner.get("height");
    // Vega-Lite bins before faceting: every cell shares the bin edges of
    // the whole dataset, so pin each bin's extent to the full data's.
    let inner = &with_bin_extents(inner, &table, ctx)?;
    // Each cell's data goes through the inner spec's pipeline; scales are
    // shared over all cells (Vega-Lite's default facet resolution).
    let rc = row_f.as_ref().and_then(|f| table.col(f));
    let cc = col_f.as_ref().and_then(|f| table.col(f));
    let mut cell_layers: Vec<(V, Vec<Layer>)> = Vec::new();
    let mut labels = Vec::new();
    for r in &rows {
        for c in &cols {
            let mut sub = table.clone();
            sub.rows.retain(|row| rc.is_none_or(|i| row[i].key() == r.key()) && cc.is_none_or(|i| row[i].key() == c.key()));
            if sub.rows.is_empty() && wrapped {
                continue;
            }
            let mut layers = Vec::new();
            collect_layers(inner, Some(&sub), None, ctx, &mut layers)?;
            let key = if col_f.is_some() { c.clone() } else { r.clone() };
            labels.push(key.label());
            cell_layers.push((key, layers));
        }
    }
    let refs: Vec<&Layer> = cell_layers.iter().flat_map(|(_, ls)| ls.iter()).collect();
    if refs.is_empty() {
        return None;
    }
    let (shared_sc, cw, ch) = view::build_scales(&refs, ow, oh, &ctx.cfg, ctx.avail_w)?;
    let mut cells = Vec::new();
    for (key, layers) in cell_layers {
        let v = View {
            layers,
            sc: view::Scales {
                x: shared_sc.x.clone(),
                y: shared_sc.y.clone(),
                x_off: shared_sc.x_off.clone(),
                y_off: shared_sc.y_off.clone(),
                color: shared_sc.color.clone(),
                color_ch: shared_sc.color_ch,
                size: shared_sc.size.clone(),
                opacity: shared_sc.opacity.clone(),
                shape: shared_sc.shape.clone(),
                dash: shared_sc.dash.clone(),
                theta: shared_sc.theta,
            },
            width: cw,
            height: ch,
            title: None,
        };
        cells.push((key, Box::new(v)));
    }
    let title = |d: Option<&Value>, f: &Option<String>| -> String {
        match d.and_then(|d| d.get("title")) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) => String::new(),
            _ => f.clone().unwrap_or_default(),
        }
    };
    let by_row = row_f.is_some() && col_f.is_none();
    let columns = if wrapped {
        columns.map(|c| c.max(1) as usize).unwrap_or(cells.len().max(1))
    } else if by_row {
        1
    } else {
        cols.len().max(1)
    };
    let g = view_guides(&cells[0].1, ctx);
    let mut gds = Guides::default();
    // Legends from all cells (merged by title).
    for (_, v) in &cells {
        view::legend_requests(v, &ctx.cfg, &mut gds);
    }
    let f = Facet {
        cells,
        rows: if row_f.is_some() && col_f.is_some() { Some(rows) } else { None },
        col_title: title(col_def, &col_f),
        row_title: title(row_def, &row_f),
        columns,
        by_row,
        labels,
        xa: g.xa,
        ya: g.ya,
        spacing: 20.,
        legends: gds.legends,
    };
    Some(Node::Facet(Box::new(f)))
}

impl Node {
    fn is_composite(&self) -> bool {
        !matches!(self, Node::View(..))
    }

    fn legends(&self, cfg: &Config, out: &mut Guides) {
        match self {
            Node::View(v, _) => view::legend_requests(v, cfg, out),
            Node::Facet(f) => {
                for x in &f.legends {
                    out.push(x.clone());
                }
            }
            Node::Concat { children, .. } => {
                for c in children {
                    c.legends(cfg, out);
                }
            }
        }
    }

    /// (width right of the origin, height below the origin, left extent,
    /// top extent). The origin is the first plot's top-left corner.
    fn size(&self) -> (f64, f64, f64, f64) {
        match self {
            Node::View(v, g) => {
                let left = g.ya.as_ref().map(|a| a.extent).unwrap_or(0.);
                let bottom = g.xa.as_ref().map(|a| a.extent).unwrap_or(0.);
                let title = if title_of(v.title.as_ref()).is_empty() { 0. } else { 22. };
                (v.width + g.overflow.0, v.height + bottom, left, g.overflow.1 + title)
            }
            Node::Facet(f) => {
                let (cw, ch) = f.cell_size();
                let cols = f.columns.max(1);
                let nrows = f.cells.len().div_ceil(cols).max(1);
                let row_header = f.by_row || f.rows.is_some();
                let left = f.ya.as_ref().map(|a| a.extent).unwrap_or(0.) + if row_header { 20. + if f.row_title.is_empty() { 0. } else { 21. } } else { 0. };
                let bottom = f.xa.as_ref().map(|a| a.extent).unwrap_or(0.);
                let header = f.header();
                let top = header + if f.col_title.is_empty() || f.by_row { 0. } else { 21. };
                let w = cols as f64 * cw + (cols as f64 - 1.) * f.spacing;
                let h = nrows as f64 * ch + (nrows as f64 - 1.) * (header + f.spacing) + bottom;
                (w, h, left, top)
            }
            Node::Concat { horizontal, wrap, children, spacing } => {
                let sizes: Vec<(f64, f64, f64, f64)> = children.iter().map(|c| c.size()).collect();
                if sizes.is_empty() {
                    return (0., 0., 0., 0.);
                }
                match wrap {
                    Some(cols) => {
                        let cols = (*cols).max(1);
                        let cw = sizes.iter().map(|s| s.0 + s.2).fold(0., f64::max);
                        let chh = sizes.iter().map(|s| s.1 + s.3).fold(0., f64::max);
                        let rows = sizes.len().div_ceil(cols);
                        let left = sizes.iter().map(|s| s.2).fold(0., f64::max);
                        let top = sizes.iter().map(|s| s.3).fold(0., f64::max);
                        (cols as f64 * cw + (cols as f64 - 1.) * spacing - left, rows as f64 * chh + (rows as f64 - 1.) * spacing - top, left, top)
                    }
                    None if *horizontal => {
                        let w: f64 = sizes.iter().map(|s| s.0 + s.2).sum::<f64>() + spacing * (sizes.len() - 1) as f64;
                        let h = sizes.iter().map(|s| s.1).fold(0., f64::max);
                        let top = sizes.iter().map(|s| s.3).fold(0., f64::max);
                        (w - sizes[0].2, h, sizes[0].2, top)
                    }
                    None => {
                        let w = sizes.iter().map(|s| s.0).fold(0., f64::max);
                        let h: f64 = sizes.iter().map(|s| s.1 + s.3).sum::<f64>() + spacing * (sizes.len() - 1) as f64;
                        let left = sizes.iter().map(|s| s.2).fold(0., f64::max);
                        (w, h - sizes[0].3, left, sizes[0].3)
                    }
                }
            }
        }
    }

    fn draw(&self, svg: &mut Svg, ox: f64, oy: f64, ctx: &mut Ctx) {
        match self {
            Node::View(v, g) => {
                // Sub-view titles (concat children).
                let t = title_of(v.title.as_ref());
                if !t.is_empty() {
                    let y = oy - g.overflow.1 - 22. + 13. * 0.79;
                    svg.text(ox + v.width / 2., y, &t, &TextStyle::new(13., ctx.theme.text).anchor(Anchor::Middle).bold(true));
                }
                draw_view(svg, v, g.xa.as_ref(), g.ya.as_ref(), ox, oy, ctx)
            }
            Node::Facet(f) => f.draw(svg, ox, oy, ctx),
            Node::Concat { horizontal, wrap, children, spacing } => {
                let sizes: Vec<(f64, f64, f64, f64)> = children.iter().map(|c| c.size()).collect();
                if sizes.is_empty() {
                    return;
                }
                match wrap {
                    Some(cols) => {
                        let cols = (*cols).max(1);
                        let cw = sizes.iter().map(|s| s.0 + s.2).fold(0., f64::max);
                        let chh = sizes.iter().map(|s| s.1 + s.3).fold(0., f64::max);
                        let left = sizes.iter().map(|s| s.2).fold(0., f64::max);
                        let top = sizes.iter().map(|s| s.3).fold(0., f64::max);
                        for (i, (c, s)) in children.iter().zip(&sizes).enumerate() {
                            let cx = ox - left + (i % cols) as f64 * (cw + spacing) + s.2;
                            let cy = oy - top + (i / cols) as f64 * (chh + spacing) + s.3;
                            c.draw(svg, cx, cy, ctx);
                        }
                    }
                    None if *horizontal => {
                        let mut x = ox - sizes[0].2;
                        for (c, s) in children.iter().zip(&sizes) {
                            c.draw(svg, x + s.2, oy, ctx);
                            x += s.2 + s.0 + spacing;
                        }
                    }
                    None => {
                        let mut y = oy - sizes[0].3;
                        for (c, s) in children.iter().zip(&sizes) {
                            c.draw(svg, ox, y + s.3, ctx);
                            y += s.3 + s.1 + spacing;
                        }
                    }
                }
            }
        }
    }
}

impl Facet {
    fn cell_size(&self) -> (f64, f64) {
        self.cells.first().map(|(_, v)| (v.width, v.height)).unwrap_or((200., 200.))
    }

    /// Height of the label row above each cell (none for row facets).
    fn header(&self) -> f64 {
        if self.by_row { 0. } else { 20. }
    }

    fn draw(&self, svg: &mut Svg, ox: f64, oy: f64, ctx: &mut Ctx) {
        let (cw, ch) = self.cell_size();
        let header = self.header();
        let cols = self.columns.max(1);
        let text_c = ctx.theme.text;
        let nrows = self.cells.len().div_ceil(cols).max(1);
        if !self.col_title.is_empty() && !self.by_row {
            let total_w = cols as f64 * cw + (cols as f64 - 1.) * self.spacing;
            svg.text(ox + total_w / 2., oy - header - 10., &self.col_title, &TextStyle::new(11., text_c).anchor(Anchor::Middle).bold(true));
        }
        let row_header = self.by_row || self.rows.is_some();
        if !self.row_title.is_empty() && row_header {
            let total_h = nrows as f64 * ch + (nrows as f64 - 1.) * (header + self.spacing);
            let x = ox - self.ya.as_ref().map(|a| a.extent).unwrap_or(0.) - 20. - 10.;
            svg.text(x, oy + total_h / 2., &self.row_title, &TextStyle::new(11., text_c).anchor(Anchor::Middle).bold(true).rotate(-90.));
        }
        for (i, (_, v)) in self.cells.iter().enumerate() {
            let col = i % cols;
            let row = i / cols;
            let cx = ox + col as f64 * (cw + self.spacing);
            let cy = oy + row as f64 * (ch + header + self.spacing);
            if row_header && col == 0 {
                let label = match &self.rows {
                    Some(r) => r.get(row).map(|v| v.label()).unwrap_or_default(),
                    None => self.labels[i].clone(),
                };
                let x = cx - self.ya.as_ref().map(|a| a.extent).unwrap_or(0.) - 10.;
                svg.text(x, cy + ch / 2., &label, &TextStyle::new(10., text_c).anchor(Anchor::Middle).rotate(-90.));
            }
            if !self.by_row && (self.rows.is_none() || row == 0) {
                svg.text(cx + cw / 2., cy - 10., &self.labels[i], &TextStyle::new(10., text_c).anchor(Anchor::Middle));
            }
            let show_y = col == 0;
            let show_x = row + 1 == nrows || i + cols >= self.cells.len();
            if !show_x && let Some(a) = &self.xa {
                view::draw_grid(svg, a, true, v.width, v.height, cx, cy, &ctx.theme);
            }
            if !show_y && let Some(a) = &self.ya {
                view::draw_grid(svg, a, false, v.width, v.height, cx, cy, &ctx.theme);
            }
            draw_view(svg, v, if show_x { self.xa.as_ref() } else { None }, if show_y { self.ya.as_ref() } else { None }, cx, cy, ctx);
        }
    }
}

fn draw_view(svg: &mut Svg, v: &View, xa: Option<&view::AxisLayout>, ya: Option<&view::AxisLayout>, ox: f64, oy: f64, ctx: &mut Ctx) {
    let (titles_x, titles_y) = (true, true);
    let arc_only = v.layers.iter().all(|l| l.mark.ty == "arc");
    // Cell background stroke (style "cell").
    if !arc_only && let Some(c) = ctx.theme.cell {
        svg.rect(ox + 0.5, oy + 0.5, v.width, v.height, &Style::stroke(c, 1.));
    }
    let axes_above = v.layers.iter().any(|l| l.mark.ty == "rect");
    if let Some(a) = xa {
        view::draw_grid(svg, a, true, v.width, v.height, ox, oy, &ctx.theme);
    }
    if let Some(a) = ya {
        view::draw_grid(svg, a, false, v.width, v.height, ox, oy, &ctx.theme);
    }
    if !axes_above {
        if let Some(a) = xa {
            view::draw_axis(svg, a, true, v.width, v.height, ox, oy, &ctx.theme, titles_x);
        }
        if let Some(a) = ya {
            view::draw_axis(svg, a, false, v.width, v.height, ox, oy, &ctx.theme, titles_y);
        }
    }
    let clip = v.layers.iter().any(|l| l.mark.props.get("clip").and_then(|c| c.as_bool()) == Some(true))
        || v.sc.x.as_ref().is_some_and(|p| p.axis.is_some() && false);
    if clip {
        let id = svg.clip_rect(ox, oy, v.width, v.height);
        svg.group_clip(&id);
    }
    view::draw_marks(svg, v, ox, oy, &ctx.cfg, &mut ctx.notes);
    if clip {
        svg.group_close();
    }
    if axes_above {
        if let Some(a) = xa {
            view::draw_axis(svg, a, true, v.width, v.height, ox, oy, &ctx.theme, titles_x);
        }
        if let Some(a) = ya {
            view::draw_axis(svg, a, false, v.width, v.height, ox, oy, &ctx.theme, titles_y);
        }
    }
}

// ---------------------------------------------------------------- legends

fn legend_size(l: &LegendReq) -> (f64, f64) {
    match l {
        LegendReq::Symbols { title, entries, .. } => {
            let title_w = text::width(title, 11., true);
            let n = entries.len().min(30);
            let mut h = if title.is_empty() { 0. } else { 16. };
            let mut w: f64 = title_w;
            for (label, _, _, size) in entries.iter().take(n) {
                let d = size.sqrt().max(10.);
                h += (d + 1.5).max(12.5) + 3.5;
                w = w.max(d + 1.5 + 4. + text::width(&text::truncate(label, 10., false, 160.), 10., false));
            }
            if entries.len() > 30 {
                h += 14.;
            }
            (w, h)
        }
        LegendReq::Gradient { title, lo, hi, temporal, length, .. } => {
            let labels = gradient_labels(*lo, *hi, *temporal, *length);
            let lw = labels.iter().map(|(_, l)| text::width(l, 10., false)).fold(0., f64::max);
            (text::width(title, 11., true).max(16. + 2. + lw), 16. + length + 6.)
        }
    }
}

/// Vega's gradient legend labels: `max(2, 2*floor(length/100))` ticks,
/// or just the domain ends when that yields fewer than three.
fn gradient_labels(lo: f64, hi: f64, temporal: bool, length: f64) -> Vec<(f64, String)> {
    let count = (2. * (length / 100.).floor()).max(2.);
    if temporal {
        let mut t = ticks::d3_time_ticks(lo, hi, count);
        if t.len() < 3 && lo != hi {
            t = vec![lo, hi];
        }
        return t.into_iter().map(|t| (t, ticks::vega_time_label(t))).collect();
    }
    let mut vals = ticks::d3_ticks(lo, hi, count);
    if vals.len() < 3 && lo != hi {
        vals = vec![lo, hi];
    }
    let step = ticks::d3_tick_step(lo, hi, count);
    let p = ticks::d3_precision_fixed(step);
    let spec = format!(",.{p}f");
    vals.into_iter().map(|v| (v, crate::format::d3(&spec, v))).collect()
}

fn draw_legend(svg: &mut Svg, l: &LegendReq, x: f64, y: f64, ctx: &Ctx) {
    let tc = ctx.theme.text;
    let title_style = TextStyle::new(11., tc).bold(true);
    match l {
        LegendReq::Symbols { title, entries, filled, opacity } => {
            let mut yy = y;
            if !title.is_empty() {
                svg.text(x, y + 9., &text::truncate(title, 11., true, 180.), &title_style);
                yy += 16.;
            }
            let max_d = entries.iter().take(30).map(|e| e.3.sqrt().max(10.)).fold(10., f64::max);
            for (label, color, shape, size) in entries.iter().take(30) {
                let d = size.sqrt().max(10.);
                let row_h = (d + 1.5).max(12.5);
                let cx = x + max_d / 2. + 1.;
                let cy = yy + row_h / 2.;
                let mut p = PathData::new();
                if shape == "stroke" {
                    p.move_to(cx - 5., cy);
                    p.line_to(cx + 5., cy);
                    let mut st = Style::stroke(*color, 1.5);
                    st.opacity = *opacity;
                    svg.path(&p.d, &st);
                } else {
                    view::symbol_path(&mut p, shape, cx, cy, *size);
                    let mut st = if *filled { Style::fill(*color) } else { Style::stroke(*color, 1.5) };
                    st.opacity = *opacity;
                    svg.path(&p.d, &st);
                }
                svg.text(x + max_d + 1.5 + 4., cy + 3., &text::truncate(label, 10., false, 160.), &TextStyle::new(10., tc));
                yy += row_h + 3.5;
            }
            if entries.len() > 30 {
                svg.text(x, yy + 9., &format!("… {} entries", entries.len()), &TextStyle::new(10., tc));
            }
        }
        LegendReq::Gradient { title, lo, hi, scale, temporal, opacity, length } => {
            if !title.is_empty() {
                svg.text(x, y + 9., &text::truncate(title, 11., true, 180.), &title_style);
            }
            let gy = y + 16.;
            let len = *length;
            let strips = 50;
            let h = len / strips as f64;
            if *opacity < 0.999 {
                svg.group_opacity(*opacity);
            }
            for k in 0..strips {
                let t = 1. - (k as f64 + 0.5) / strips as f64;
                svg.fill_rect(x, gy + k as f64 * h, 16., h + 0.5, scale.at(t));
            }
            if *opacity < 0.999 {
                svg.group_close();
            }
            for (v, label) in gradient_labels(*lo, *hi, *temporal, len) {
                let t = (v - lo) / (hi - lo);
                if !(-1e-9..=1. + 1e-9).contains(&t) {
                    continue;
                }
                let yy = gy + (1. - t) * len;
                // baseline: top at the max end, bottom at the min end, else middle
                let base = if t >= 1. - 1e-9 { yy + 8. } else if t <= 1e-9 { yy - 2. } else { yy + 3. };
                svg.text(x + 18., base, &label, &TextStyle::new(10., tc));
            }
        }
    }
}


#[cfg(test)]
#[allow(clippy::type_complexity)]
mod tests {
    use std::cell::RefCell;

    use super::data::V;
    use super::{Node, view};
    use crate::RenderOptions;

    #[derive(Default, Clone)]
    pub struct Snap {
        /// (continuous domain, discrete domain labels, tick labels) for x, y
        pub x: Option<((f64, f64), Vec<String>, Vec<String>)>,
        pub y: Option<((f64, f64), Vec<String>, Vec<String>)>,
        /// First layer's derived table: column names + rows
        pub cols: Vec<String>,
        pub rows: Vec<Vec<V>>,
        pub cells: usize,
    }

    thread_local! {
        static SNAP: RefCell<Snap> = RefCell::new(Snap::default());
    }

    fn axis(a: &Option<view::AxisLayout>, p: &Option<view::Pos>) -> Option<((f64, f64), Vec<String>, Vec<String>)> {
        let p = p.as_ref()?;
        Some((p.dom, p.cats.iter().map(|c| c.label()).collect(), a.as_ref().map(|a| a.ticks.iter().map(|t| t.2.clone()).collect()).unwrap_or_default()))
    }

    pub fn record(node: &Node) {
        let mut s = Snap::default();
        match node {
            Node::View(v, g) => {
                s.x = axis(&g.xa, &v.sc.x);
                s.y = axis(&g.ya, &v.sc.y);
                s.cols = v.layers[0].table.names.clone();
                s.rows = v.layers[0].table.rows.clone();
                s.cells = 1;
            }
            Node::Facet(f) => {
                let v = &f.cells[0].1;
                s.x = axis(&f.xa, &v.sc.x);
                s.y = axis(&f.ya, &v.sc.y);
                s.cols = v.layers[0].table.names.clone();
                s.rows = f.cells.iter().flat_map(|c| c.1.layers[0].table.rows.clone()).collect();
                s.cells = f.cells.len();
            }
            Node::Concat { children, .. } => s.cells = children.len(),
        }
        SNAP.with(|x| *x.borrow_mut() = s);
    }

    fn snap(name: &str) -> Snap {
        let p = format!("{}/tests/data/vegalite/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert!(super::render(&spec, &RenderOptions::default()).is_some(), "{name} should render");
        SNAP.with(|x| x.borrow().clone())
    }

    fn col(s: &Snap, name: &str) -> Vec<f64> {
        let c = s.cols.iter().position(|n| n == name).unwrap_or_else(|| panic!("no column {name} in {:?}", s.cols));
        s.rows.iter().filter_map(|r| r[c].num()).collect()
    }

    #[test]
    fn zero_and_nice_domains() {
        let s = snap("point_color");
        let (xd, _, xt) = s.x.unwrap();
        assert_eq!(xd, (0., 260.));
        assert_eq!(xt, ["0", "50", "100", "150", "200", "250"]);
        assert_eq!(s.y.unwrap().0, (0., 50.));
        // custom domain + zero:false
        let s = snap("custom_domain_scheme");
        assert_eq!(s.x.unwrap().0, (50., 260.));
        assert!(s.y.unwrap().0.0 > 0.);
        // line x (the dimension) is not zero-based
        let s = snap("area_stacked");
        assert_eq!(s.x.unwrap().0, (2000., 2010.));
    }

    #[test]
    fn binning_and_counting() {
        let s = snap("histogram");
        assert_eq!(col(&s, "__count").iter().sum::<f64>(), 150.);
        let starts = col(&s, "bin_mpg");
        let ends = col(&s, "bin_mpg_end");
        assert!(starts.iter().zip(&ends).all(|(a, b)| b - a == 5.));
        assert_eq!(s.x.clone().unwrap().0, (10., 50.)); // mpg 11.8..49.4, step 5
        let s = snap("bar_count");
        assert_eq!(col(&s, "__count").iter().sum::<f64>(), 150.);
        assert_eq!(s.x.as_ref().unwrap().1, ["Europe", "Japan", "USA"]);
        assert_eq!(s.y.unwrap().2.first().map(String::as_str), Some("0"));
    }

    #[test]
    fn aggregation_sort_and_stack() {
        let s = snap("bar_sorted");
        // sort: -y (sum of weight) descending
        let cats = s.x.clone().unwrap().1;
        let sums = col(&s, "sum_weight");
        let order: Vec<f64> = cats
            .iter()
            .map(|c| {
                let i = s.rows.iter().position(|r| r[0].label() == *c).unwrap();
                sums[i]
            })
            .collect();
        assert!(order.windows(2).all(|w| w[0] >= w[1]), "{order:?}");
        let s = snap("bar_stacked");
        let ends = col(&s, "__count_end");
        assert_eq!(ends.iter().cloned().fold(0., f64::max), 58.);
        assert_eq!(s.y.unwrap().0, (0., 60.));
    }

    #[test]
    fn time_units() {
        let s = snap("line_timeunit_month");
        assert_eq!(s.rows.len(), 12);
        let t = s.x.unwrap();
        assert_eq!(t.2.first().map(String::as_str), Some("Jan 2022"));
        let s = snap("bar_timeunit_month");
        assert_eq!(s.x.unwrap().1.len(), 12);
    }

    #[test]
    fn facets_share_scales() {
        let s = snap("nb_like_facet_hist");
        assert_eq!(s.cells, 3);
        // every cell bins with the same edges
        let starts = col(&s, "bin_mpg");
        let step = 2.;
        assert!(starts.iter().all(|v| ((v / step).round() * step - v).abs() < 1e-9));
    }
}
